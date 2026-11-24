//! taba: start, list, inspect and stop local processes, grouped by work scope.

mod error;
mod ports;
mod proc;
mod ready;
mod scope;
mod state;
mod util;

use clap::{Parser, Subcommand};
use error::{Next, Output, Result, TabaError, Warning, emit};
use ports::{PortSpec, RunRequest};
use ready::{Outcome, ReadySpec};
use scope::{Caller, ScopeSource, check_ident};
use serde_json::{Value, json};
use state::{OpLock, Service, Store};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;
use util::{local_timestamp, log_tail, tilde};

#[derive(Parser)]
#[command(
    name = "taba",
    version,
    about = "Start, list and stop local processes, bundled by work scope"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a command detached, owned by SCOPE/NAME
    Run {
        /// Work scope that owns the service (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Service name, unique within the scope
        #[arg(long)]
        name: String,
        /// Port: a number, `auto` (a free port in 4100-4999) or `auto:START`.
        /// taba replaces `{port}` in the command and sets PORT
        #[arg(long, value_parser = ports::parse_port_spec, value_name = "PORT|auto|auto:START")]
        port: Option<PortSpec>,
        /// Block until the service is ready (default 30 seconds), then report
        #[arg(long, value_name = "SECS", num_args = 0..=1, require_equals = true,
              default_missing_value = "30")]
        wait: Option<f64>,
        /// Ready check for --wait: tcp (default with a port), http:PATH or log:REGEX
        #[arg(long, value_parser = ready::parse_ready_spec, requires = "wait",
              value_name = "tcp|http:PATH|log:REGEX")]
        ready: Option<ReadySpec>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
        /// Command to run, after `--`
        #[arg(last = true, required = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Show the caller's scope, where it came from, and its services
    Whoami {
        /// Scope to report (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// List services
    Ps {
        /// Only show this scope
        #[arg(long)]
        scope: Option<String>,
        /// Only show running services (exited ones are listed by default)
        #[arg(long)]
        running: bool,
        /// Print JSON instead of a table (paths are absolute; also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Print a service's log
    Logs {
        /// SCOPE/NAME, or NAME with $TABA_SCOPE or the git worktree's scope
        target: String,
        /// Keep printing new output until the service exits
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop a service (SCOPE/NAME) or every service in a scope (--scope)
    Stop {
        /// SCOPE/NAME, or NAME in the caller's scope
        target: Option<String>,
        /// The caller's scope; without a target, stop all its services
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Seconds to wait after SIGTERM before sending SIGKILL
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
        /// Stop a service of another scope (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why --force is needed (recorded in events.jsonl)
        #[arg(long, requires = "force")]
        reason: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Remove stopped services' records and logs (SCOPE/NAME, or --scope)
    Rm {
        /// SCOPE/NAME, or NAME in the caller's scope
        target: Option<String>,
        /// The caller's scope; without a target, clean up all its stopped services
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Remove a service of another scope (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why --force is needed (recorded in events.jsonl)
        #[arg(long, requires = "force")]
        reason: Option<String>,
    },
}

fn env_json() -> bool {
    std::env::var("TABA_OUTPUT").is_ok_and(|v| v == "json")
}

/// Whether JSON output was asked for, judged from the raw arguments (used
/// when clap itself rejects them).
fn json_requested_raw() -> bool {
    env_json()
        || std::env::args()
            .skip(1)
            .take_while(|a| a != "--")
            .any(|a| a == "--json")
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            let informational = matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            if informational || !json_requested_raw() {
                e.exit();
            }
            let rendered = e.render().to_string();
            let msg = rendered
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .to_string();
            let err =
                TabaError::usage(msg).next(Next::run("see the usage", "taba <command> --help"));
            return ExitCode::from(emit(true, Err(err)));
        }
    };

    let (json_mode, res): (bool, Result<Option<Output>>) = match cli.cmd {
        Cmd::Run {
            scope,
            agent,
            name,
            port,
            wait,
            ready,
            json,
            command,
        } => (
            json || env_json(),
            cmd_run(RunArgs {
                scope,
                agent,
                name,
                port,
                wait,
                ready,
                command,
            })
            .map(Some),
        ),
        Cmd::Whoami { scope, agent, json } => {
            (json || env_json(), cmd_whoami(scope, agent).map(Some))
        }
        Cmd::Ps {
            scope,
            running,
            json,
        } => {
            let json = json || env_json();
            (json, cmd_ps(scope, running, json).map(|_| None))
        }
        Cmd::Logs { target, follow } => (false, cmd_logs(&target, follow).map(|_| None)),
        Cmd::Stop {
            target,
            scope,
            agent,
            timeout,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            cmd_stop(target, scope, agent, timeout, force, reason).map(Some),
        ),
        Cmd::Rm {
            target,
            scope,
            agent,
            force,
            reason,
        } => (false, cmd_rm(target, scope, agent, force, reason).map(Some)),
    };
    let code = match res {
        Ok(None) => 0,
        Ok(Some(out)) => emit(json_mode, Ok(out)),
        Err(e) => emit(json_mode, Err(e)),
    };
    ExitCode::from(code)
}

/// Parses `SCOPE/NAME`, or `NAME` in the caller's scope.
fn resolve_target(target: &str, caller: &Caller) -> Result<(String, String)> {
    let (scope, name) = match target.split_once('/') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => (caller.require_scope()?, target.to_string()),
    };
    check_ident("scope", &scope)?;
    check_ident("name", &name)?;
    Ok((scope, name))
}

fn not_found(scope: &str, name: &str) -> TabaError {
    TabaError::new("E_NOT_FOUND", format!("no such service: {scope}/{name}"))
        .details(json!({ "service": format!("{scope}/{name}") }))
        .next(Next::run("list services", "taba ps --json"))
}

/// `--force` needs a non-empty `--reason`.
fn check_force(force: bool, reason: Option<String>) -> Result<Option<String>> {
    if !force {
        return Ok(None);
    }
    match reason.map(|r| r.trim().to_string()) {
        Some(r) if !r.is_empty() => Ok(Some(r)),
        _ => Err(TabaError::new(
            "E_FORCE_NEEDS_REASON",
            "--force needs --reason TEXT; the reason is recorded in events.jsonl",
        )),
    }
}

fn busy(svc_id: &str, op: &OpLock) -> TabaError {
    TabaError::new(
        "E_BUSY",
        format!(
            "{svc_id} is busy: another taba `{}` (pid {}, agent {}) is in progress; \
             retry when it has finished",
            op.op, op.pid, op.agent
        ),
    )
    .details(json!({
        "service": svc_id, "op": op.op, "pid": op.pid,
        "agent": op.agent, "started_at": op.started_at,
    }))
    .next(Next::run(
        "wait for the other operation, then check the service",
        format!("taba ps --scope {}", svc_id.split('/').next().unwrap_or("")),
    ))
}

/// The caller may mutate `svc` only if it is in the caller's own scope.
fn not_owner(caller: &Caller, svc: &Service, verb: &str) -> TabaError {
    let you = caller
        .scope
        .as_deref()
        .map_or("you have no scope".to_string(), |s| {
            format!("you are in {s}")
        });
    let mut err = TabaError::new(
        "E_NOT_OWNER",
        format!(
            "{} belongs to scope {} ({}), and {you}; taba does not {verb} another scope's \
             service. A human can override this with: taba {verb} {} --force --reason TEXT",
            svc.id(),
            svc.scope,
            tilde(&svc.cwd),
            svc.id()
        ),
    )
    .details(json!({
        "service": svc.id(),
        "owner_scope": svc.scope,
        "directory": svc.cwd,
        "caller_scope": caller.scope,
        "agent": caller.agent,
        "url": svc.url(),
    }));
    if let Some(url) = svc.url() {
        err = err.next(Next::just(&format!(
            "use the server read-only at {url}; do not stop it"
        )));
    }
    err.next(Next::just(&format!(
        "ask whoever works in {} to {verb} it",
        tilde(&svc.cwd)
    )))
    .next(Next::just("ask the human"))
}

/// Records a `--force` override in events.jsonl. Call under the global lock.
fn record_force(
    store: &Store,
    event: &str,
    caller: &Caller,
    svc: &Service,
    reason: &str,
    overrode: &[&str],
) -> Result<()> {
    store.append_event(&json!({
        "at": util::rfc3339_now(),
        "event": event,
        "agent": caller.agent,
        "caller_scope": caller.scope,
        "cwd": caller.cwd,
        "target": svc.id(),
        "reason": reason,
        "instance": svc.instance(),
        "overrode": overrode,
    }))?;
    Ok(())
}

/// Short multi-line summary printed by `run`, e.g.
///
/// ```text
/// started login-fix/web → http://localhost:5173
///   dir  ~/dev/app-login
///   logs taba logs login-fix/web
/// ```
fn describe(verb: &str, svc: &Service) -> String {
    let head = match svc.url() {
        Some(url) => format!("{verb} {} → {url}", svc.id()),
        None => format!("{verb} {}", svc.id()),
    };
    format!(
        "{head}\n  dir  {}\n  logs taba logs {}",
        tilde(&svc.cwd),
        svc.id()
    )
}

// ---------------------------------------------------------------- run

struct RunArgs {
    scope: Option<String>,
    agent: Option<String>,
    name: String,
    port: Option<PortSpec>,
    wait: Option<f64>,
    ready: Option<ReadySpec>,
    command: Vec<String>,
}

fn cmd_run(a: RunArgs) -> Result<Output> {
    let caller = scope::resolve(a.scope.clone(), a.agent.clone())?;
    let scope = caller.require_scope()?;
    check_ident("name", &a.name)?;
    let name = a.name.clone();
    let wait = match a.wait {
        None => None,
        Some(s) if s.is_finite() && s > 0.0 => Some(Duration::from_secs_f64(s)),
        Some(_) => {
            return Err(TabaError::usage(
                "--wait takes a positive number of seconds",
            ));
        }
    };
    if let Some(r) = &a.ready
        && r.needs_port()
        && a.port.is_none()
    {
        return Err(TabaError::usage(format!(
            "--ready {} needs --port (use log:REGEX for a service without a port)",
            r.name()
        )));
    }
    // The ready check: --ready, else tcp when there is a port. Without
    // either, --wait keeps v0.1's "still alive after 200 ms" check.
    let check = wait.and_then(|_| {
        a.ready
            .clone()
            .or_else(|| a.port.as_ref().map(|_| ReadySpec::Tcp))
    });
    let template = a.command.clone();
    let port_request = a.port.as_ref().map(PortSpec::request);
    let scope_flag = (caller.source == Some(ScopeSource::Flag)).then_some(scope.as_str());
    let req = RunRequest {
        scope_flag,
        name: &name,
        template: &template,
    };

    let store = Store::open()?;
    let lock = store.lock()?;
    store.mark_version()?;
    let id = format!("{scope}/{name}");
    if let Some(op) = store.live_op(&scope, &name)? {
        return Err(busy(&id, &op));
    }
    store.clear_stale_op(&scope, &name)?;

    let existing = store.get(&scope, &name)?;
    if let Some(ex) = &existing
        && ex.is_running()
    {
        if ex.template() == template.as_slice()
            && ex.cwd == caller.cwd
            && ex.port_request() == port_request
        {
            drop(lock);
            return finish_existing(ex.clone(), check, wait);
        }
        return Err(TabaError::new(
            "E_CONFLICT",
            format!(
                "{} is already running with a different command, directory or port \
                 (command {:?}, dir {}, port {}); stop it first",
                ex.id(),
                ex.template(),
                tilde(&ex.cwd),
                ex.port_request().unwrap_or_else(|| "-".into()),
            ),
        )
        .details(json!({
            "service": ex.id(), "command": ex.template(), "directory": ex.cwd,
            "port": ex.port, "port_request": ex.port_request(), "instance": ex.instance(),
        }))
        .next(Next::run(
            "stop it, then run again",
            format!("taba stop {}", ex.id()),
        ))
        .next(Next::just("or use another --name")));
    }
    // An exited record is replaced.
    let replacing = existing.is_some();

    let port = match &a.port {
        None => None,
        Some(PortSpec::Fixed(p)) => {
            ports::check_port_free(&store, *p, &scope, &req)?;
            Some(*p)
        }
        Some(PortSpec::Auto { first, last, .. }) => {
            Some(ports::pick_auto(&store, *first, *last, &scope, &name)?)
        }
    };
    let mut warnings = Vec::new();
    if matches!(a.port, Some(PortSpec::Auto { .. }))
        && !template.iter().any(|x| x.contains("{port}"))
    {
        warnings.push(Warning {
            code: "W_PORT_NOT_IN_COMMAND",
            message: format!(
                "the command has no {{port}}; taba only set PORT={}, which the app may ignore",
                port.unwrap_or(0)
            ),
        });
    }
    let command: Vec<String> = match port {
        Some(p) => template
            .iter()
            .map(|x| x.replace("{port}", &p.to_string()))
            .collect(),
        None => template.clone(),
    };

    let log_path = store.log_path(&scope, &name);
    std::fs::create_dir_all(log_path.parent().expect("log has parent"))?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| TabaError::state(format!("cannot open log {}: {e}", log_path.display())))?;
    writeln!(
        log,
        "--- taba: {} starting {:?} in {} ---",
        local_timestamp(),
        command,
        caller.cwd.display()
    )?;
    let log_offset = log.metadata()?.len();
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(&caller.cwd)
        .env("TABA_SCOPE", &scope)
        .env("TABA_SERVICE", &name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(p) = port {
        cmd.env("PORT", p.to_string());
    }
    // New session => new process group with pgid == pid and no controlling
    // terminal, so closing the launching terminal does not SIGHUP it.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start_failed = |message: String| {
        TabaError::new("E_START_FAILED", message)
            .details(json!({
                "service": id, "log_path": log_path, "log_tail": log_tail(&log_path, 20),
            }))
            .next(Next::run("read the log", format!("taba logs {id}")))
    };
    let child = cmd
        .spawn()
        .map_err(|e| start_failed(format!("cannot start {:?}: {e}", command[0])))?;
    let pid = child.id() as i32;
    // The child is intentionally not waited on: once taba exits it is
    // re-parented to launchd, which reaps it.
    drop(child);

    let info = proc::info(pid)
        .ok_or_else(|| start_failed(format!("{id} exited immediately; see taba logs {id}")))?;
    let mut svc = Service {
        scope: scope.clone(),
        name: name.clone(),
        command,
        cwd: caller.cwd.clone(),
        port,
        pid,
        pgid: pid,
        start_time: info.start_time,
        log_path: log_path.clone(),
        command_template: Some(template),
        port_request,
        instance: Some(format!("{pid}-{}", info.start_time)),
        started_by: Some(caller.agent.clone()),
        ready: None,
    };
    store.put(&svc)?;
    let op = if replacing { "replace" } else { "start" };
    store.put_op(&scope, &name, &OpLock::mine(op, &caller.agent))?;
    // The port is reserved by the record now; wait without the global lock.
    drop(lock);

    let outcome = match (&check, wait) {
        (Some(c), Some(w)) => ready::wait(&svc, c, w, log_offset),
        _ => {
            thread::sleep(Duration::from_millis(200));
            if svc.is_running() {
                Outcome::Ready { ms: 200 }
            } else {
                Outcome::Exited
            }
        }
    };

    {
        let _lock = store.lock()?;
        if let (Outcome::Ready { .. }, Some(c)) = (&outcome, &check) {
            svc.ready = Some(state::Ready {
                check: c.name().to_string(),
                ready_at: util::now_micros(),
            });
            // Only update the record if it still describes this instance.
            if store
                .get(&scope, &name)?
                .is_some_and(|cur| cur.instance() == svc.instance())
            {
                store.put(&svc)?;
            }
        }
        store.release_op(&scope, &name)?;
    }
    run_result(svc, "started", check.as_ref(), wait, outcome, warnings)
}

/// `run` found the same service already running: report it, after the
/// ready check when --wait was given.
fn finish_existing(
    svc: Service,
    check: Option<ReadySpec>,
    wait: Option<Duration>,
) -> Result<Output> {
    let outcome = match (&check, wait) {
        (Some(c), Some(w)) => ready::wait(&svc, c, w, 0),
        _ => Outcome::Ready { ms: 0 },
    };
    run_result(
        svc,
        "already_running",
        check.as_ref(),
        wait,
        outcome,
        Vec::new(),
    )
}

fn run_result(
    svc: Service,
    status: &str,
    check: Option<&ReadySpec>,
    wait: Option<Duration>,
    outcome: Outcome,
    warnings: Vec<Warning>,
) -> Result<Output> {
    let id = svc.id();
    match outcome {
        Outcome::Ready { ms } => {
            let ready = check.map(|c| json!({ "check": c.name(), "ms": ms }));
            let body = json!({
                "status": status,
                "service": id,
                "scope": svc.scope,
                "name": svc.name,
                "url": svc.url(),
                "port": svc.port,
                "instance": svc.instance(),
                "ready": ready,
                "pid": svc.pid,
                "directory": svc.cwd,
                "log_path": svc.log_path,
            });
            let verb = if status == "started" {
                "started"
            } else {
                "already running"
            };
            let mut text = describe(verb, &svc);
            if let Some(c) = check {
                text.push_str(&format!("\n  ready {} check passed in {ms} ms", c.name()));
            }
            let mut out = Output::new(body, text);
            out.warnings = warnings;
            Ok(out)
        }
        Outcome::Exited => Err(TabaError::new(
            "E_START_FAILED",
            format!("{id} exited before it was ready; see taba logs {id}"),
        )
        .details(json!({
            "service": id, "pid": svc.pid, "instance": svc.instance(),
            "log_path": svc.log_path, "log_tail": log_tail(&svc.log_path, 20),
        }))
        .next(Next::run("read the log", format!("taba logs {id}")))),
        Outcome::TimedOut { ms } => {
            let secs = wait.map_or(0.0, |w| w.as_secs_f64());
            let check_name = check.map_or("alive", ReadySpec::name);
            Err(TabaError::new(
                "E_NOT_READY",
                format!(
                    "{id} is running but its {check_name} check did not pass within {secs}s; \
                     it was left running (pid {})",
                    svc.pid
                ),
            )
            .details(json!({
                "service": id, "status": "running", "pid": svc.pid, "instance": svc.instance(),
                "port": svc.port, "url": svc.url(), "check": check_name, "waited_ms": ms,
                "log_path": svc.log_path, "log_tail": log_tail(&svc.log_path, 20),
            }))
            .next(Next::run("read the log", format!("taba logs {id}")))
            .next(Next::run(
                "stop it if it is stuck",
                format!("taba stop {id}"),
            )))
        }
    }
}

// ---------------------------------------------------------------- whoami

fn cmd_whoami(scope: Option<String>, agent: Option<String>) -> Result<Output> {
    let caller = scope::resolve(scope, agent)?;
    let store = Store::open()?;
    let services: Vec<String> = match &caller.scope {
        Some(s) => store.list(Some(s))?.iter().map(Service::id).collect(),
        None => Vec::new(),
    };
    let body = json!({
        "scope": caller.scope,
        "scope_source": caller.source.map(ScopeSource::as_str),
        "worktree": caller.worktree,
        "branch": caller.branch,
        "agent": caller.agent,
        "human": caller.is_human(),
        "directory": caller.cwd,
        "services": services,
    });
    let mut text = match (&caller.scope, caller.source) {
        (Some(s), Some(src)) => format!("scope     {s} (from {})", src.as_str()),
        _ => "scope     none (pass --scope, set TABA_SCOPE, or cd into a git worktree)".into(),
    };
    if let Some(w) = &caller.worktree {
        text.push_str(&format!("\nworktree  {}", tilde(w)));
    }
    if let Some(b) = &caller.branch {
        text.push_str(&format!("\nbranch    {b}"));
    }
    text.push_str(&format!("\nagent     {}", caller.agent));
    if caller.scope.is_some() {
        let list = if services.is_empty() {
            "none".to_string()
        } else {
            services.join(", ")
        };
        text.push_str(&format!("\nservices  {list}"));
    }
    Ok(Output::new(body, text))
}

// ---------------------------------------------------------------- ps

fn status_of(store: &Store, s: &Service) -> &'static str {
    match store.live_op(&s.scope, &s.name) {
        Ok(Some(op)) if op.op == "stop" => "stopping",
        Ok(Some(_)) => "starting",
        _ if s.is_running() => "running",
        _ => "exited",
    }
}

fn cmd_ps(scope: Option<String>, running_only: bool, json: bool) -> Result<()> {
    if let Some(s) = &scope {
        check_ident("scope", s)?;
    }
    let store = Store::open()?;
    let mut services = store.list(scope.as_deref())?;
    if running_only {
        services.retain(|s| s.is_running());
    }
    let statuses: Vec<&str> = services.iter().map(|s| status_of(&store, s)).collect();

    if json {
        let rows: Vec<Value> = services
            .iter()
            .zip(&statuses)
            .map(|(s, status)| {
                json!({
                    "service": s.id(),
                    "scope": s.scope,
                    "name": s.name,
                    "status": status,
                    "url": s.url(),
                    "port": s.port,
                    "directory": s.cwd,
                    "command": s.command,
                    "pid": s.pid,
                    "pgid": s.pgid,
                    "start_time": s.start_time,
                    "log_path": s.log_path,
                    "instance": s.instance(),
                })
            })
            .collect();
        error::print_json(&Value::Array(rows));
        return Ok(());
    }

    let mut rows = vec![[
        "SERVICE".to_string(),
        "STATUS".to_string(),
        "URL".to_string(),
        "DIRECTORY".to_string(),
    ]];
    for (s, status) in services.iter().zip(&statuses) {
        rows.push([
            s.id(),
            status.to_string(),
            s.url().unwrap_or_else(|| "-".into()),
            tilde(&s.cwd),
        ]);
    }
    let widths: Vec<usize> = (0..3)
        .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    for r in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            r[0],
            r[1],
            r[2],
            r[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- logs

fn cmd_logs(target: &str, follow: bool) -> Result<()> {
    let caller = scope::resolve(None, None)?;
    let (scope, name) = resolve_target(target, &caller)?;
    let store = Store::open()?;
    let svc = store
        .get(&scope, &name)?
        .ok_or_else(|| not_found(&scope, &name))?;
    let mut file = File::open(&svc.log_path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", svc.log_path.display())))?;
    let mut stdout = io::stdout().lock();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    stdout.write_all(&buf)?;
    stdout.flush()?;
    if !follow {
        return Ok(());
    }
    let mut pos = file.stream_position()?;
    loop {
        let running = svc.is_running();
        file.seek(SeekFrom::Start(pos))?;
        buf.clear();
        file.read_to_end(&mut buf)?;
        if !buf.is_empty() {
            pos += buf.len() as u64;
            if stdout.write_all(&buf).and_then(|_| stdout.flush()).is_err() {
                return Ok(()); // reader went away (e.g. `| head`)
            }
        } else if !running {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------- stop

/// The services a `stop` or `rm` acts on: `SCOPE/NAME`, `NAME` in the
/// caller's scope, or with no target every service of the explicit `--scope`.
fn targets(
    store: &Store,
    target: Option<String>,
    scope_flag_given: bool,
    caller: &Caller,
    verb: &str,
) -> Result<(Vec<(String, String)>, bool)> {
    match target {
        Some(t) => Ok((vec![resolve_target(&t, caller)?], true)),
        None => {
            let Some(s) = caller.scope.clone().filter(|_| scope_flag_given) else {
                return Err(TabaError::usage(format!(
                    "nothing to {verb}: give SCOPE/NAME or --scope SCOPE"
                )));
            };
            let list = store
                .list(Some(&s))?
                .into_iter()
                .map(|x| (x.scope, x.name))
                .collect();
            Ok((list, false))
        }
    }
}

struct Stopped {
    svc: Service,
    result: &'static str,
    signal: Option<&'static str>,
    forced: bool,
    /// The recorded pid is alive but is another process now (pid reuse).
    unverifiable: bool,
}

fn cmd_stop(
    target: Option<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    timeout: f64,
    force: bool,
    reason: Option<String>,
) -> Result<Output> {
    if !timeout.is_finite() || timeout < 0.0 {
        return Err(TabaError::usage(
            "--timeout must be a non-negative number of seconds",
        ));
    }
    let timeout = Duration::from_secs_f64(timeout);
    let reason = check_force(force, reason)?;
    let scope_flag_given = scope_flag.is_some();
    let caller = scope::resolve(scope_flag, agent)?;
    let store = Store::open()?;
    let (targets, single) = targets(&store, target, scope_flag_given, &caller, "stop")?;

    let mut done = Vec::new();
    let mut refused: Vec<(String, TabaError)> = Vec::new();
    for (s, n) in &targets {
        match stop_one(&store, &caller, s, n, timeout, reason.as_deref()) {
            Ok(d) => done.push(d),
            Err(e) if single => return Err(e),
            Err(e) => refused.push((format!("{s}/{n}"), e)),
        }
    }

    let mut results: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for d in &done {
        let mut r = json!({
            "service": d.svc.id(),
            "result": d.result,
            "instance": d.svc.instance(),
            "directory": d.svc.cwd,
        });
        if let Some(sig) = d.signal {
            r["signal"] = json!(sig);
        }
        if d.forced {
            r["forced"] = json!(true);
        }
        if d.unverifiable {
            r["unverifiable"] = json!(true);
        }
        results.push(r);
        let what = match d.result {
            "stopped" => "stopped".to_string(),
            "killed" => "stopped (SIGKILL after timeout)".to_string(),
            _ if d.unverifiable => format!(
                "not running (exited; pid {} is now another process, not signalled)",
                d.svc.pid
            ),
            _ => "not running (exited)".to_string(),
        };
        let forced = if d.forced { " (forced)" } else { "" };
        lines.push(format!(
            "{}: {what}{forced}  {}",
            d.svc.id(),
            tilde(&d.svc.cwd)
        ));
    }
    for (id, e) in &refused {
        let result = if e.code == "E_SIGNAL_FAILED" {
            "failed"
        } else {
            "refused"
        };
        results.push(json!({ "service": id, "result": result, "error": e.to_json() }));
    }
    if targets.is_empty() {
        lines.push(format!(
            "no services in scope {}",
            caller.scope.clone().unwrap_or_default()
        ));
    }
    let mut out = Output::new(json!({ "results": results }), lines.join("\n"));
    if !refused.is_empty() {
        // A refusal (exit 3) outranks a signal failure (exit 1).
        let first = refused
            .iter()
            .map(|(_, e)| e)
            .max_by_key(|e| e.exit_code() == 3)
            .expect("non-empty");
        let detail: Vec<String> = refused
            .iter()
            .map(|(id, e)| format!("{id}: {}", e.message))
            .collect();
        out.failure = Some(TabaError::new(
            first.code,
            format!(
                "{} of {} service(s) were not stopped:\n  {}",
                refused.len(),
                targets.len(),
                detail.join("\n  ")
            ),
        ));
    }
    Ok(out)
}

/// Stops one service under the operation lock: checks and lock-taking happen
/// under the global lock, signalling and waiting without it.
fn stop_one(
    store: &Store,
    caller: &Caller,
    scope: &str,
    name: &str,
    timeout: Duration,
    force_reason: Option<&str>,
) -> Result<Stopped> {
    let svc = {
        let _lock = store.lock()?;
        let svc = store
            .get(scope, name)?
            .ok_or_else(|| not_found(scope, name))?;
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        if !owner && force_reason.is_none() {
            return Err(not_owner(caller, &svc, "stop"));
        }
        if let Some(op) = store.live_op(scope, name)? {
            return Err(busy(&svc.id(), &op));
        }
        store.clear_stale_op(scope, name)?;
        if !svc.is_running() {
            let unverifiable = matches!(proc::info(svc.pid), Some(p) if !p.zombie && p.start_time != svc.start_time);
            return Ok(Stopped {
                svc,
                result: "not_running",
                signal: None,
                forced: false,
                unverifiable,
            });
        }
        store.put_op(scope, name, &OpLock::mine("stop", &caller.agent))?;
        if let Some(reason) = force_reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            record_force(store, "force_stop", caller, &svc, reason, overrode)?;
        }
        svc
    };

    // Re-verify identity immediately before signalling (as v0.1 does).
    let res = if svc.is_running() {
        proc::stop_group(svc.pgid, timeout).map(Some)
    } else {
        Ok(None)
    };
    {
        let _lock = store.lock()?;
        store.release_op(scope, name)?;
    }
    let forced = force_reason.is_some();
    match res {
        Ok(Some(proc::StopResult::Terminated)) => Ok(Stopped {
            svc,
            result: "stopped",
            signal: Some("TERM"),
            forced,
            unverifiable: false,
        }),
        Ok(Some(proc::StopResult::Killed)) => Ok(Stopped {
            svc,
            result: "killed",
            signal: Some("KILL"),
            forced,
            unverifiable: false,
        }),
        Ok(None) => Ok(Stopped {
            svc,
            result: "not_running",
            signal: None,
            forced: false,
            unverifiable: false,
        }),
        Err(e) => Err(TabaError::new(
            "E_SIGNAL_FAILED",
            format!(
                "{}: cannot signal process group {}: {e}",
                svc.id(),
                svc.pgid
            ),
        )
        .details(json!({ "service": svc.id(), "pgid": svc.pgid }))),
    }
}

// ---------------------------------------------------------------- rm

fn cmd_rm(
    target: Option<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    force: bool,
    reason: Option<String>,
) -> Result<Output> {
    let reason = check_force(force, reason)?;
    let scope_flag_given = scope_flag.is_some();
    let caller = scope::resolve(scope_flag, agent)?;
    let store = Store::open()?;
    let _lock = store.lock()?;
    let (targets, single) = targets(&store, target, scope_flag_given, &caller, "remove")?;

    let mut lines = Vec::new();
    let mut refusals: Vec<TabaError> = Vec::new();
    for (s, n) in &targets {
        let svc = store.get(s, n)?.ok_or_else(|| not_found(s, n))?;
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        let refusal = if !owner && reason.is_none() {
            Some(not_owner(&caller, &svc, "rm"))
        } else if let Some(op) = store.live_op(s, n)? {
            Some(busy(&svc.id(), &op))
        } else if svc.is_running() {
            Some(
                TabaError::new(
                    "E_STILL_RUNNING",
                    format!(
                        "{}: still running; stop it first: taba stop {}",
                        svc.id(),
                        svc.id()
                    ),
                )
                .details(json!({ "service": svc.id() }))
                .next(Next::run(
                    "stop it first",
                    format!("taba stop {}", svc.id()),
                )),
            )
        } else {
            None
        };
        if let Some(e) = refusal {
            if single {
                return Err(e);
            }
            refusals.push(e);
            continue;
        }
        if let Some(r) = &reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            record_force(&store, "force_rm", &caller, &svc, r, overrode)?;
        }
        store.remove(&svc.scope, &svc.name)?;
        lines.push(format!("{}: removed (record and log)", svc.id()));
    }
    if targets.is_empty() {
        lines.push(format!(
            "no services in scope {}",
            caller.scope.clone().unwrap_or_default()
        ));
    }
    let mut out = Output::new(json!({}), lines.join("\n"));
    if let Some(first) = refusals.first() {
        let detail: Vec<String> = refusals.iter().map(|e| e.message.clone()).collect();
        out.failure = Some(TabaError::new(
            first.code,
            format!(
                "{}\n{} service(s) were not removed",
                detail.join("\n"),
                refusals.len()
            ),
        ));
    }
    Ok(out)
}
