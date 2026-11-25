//! taba: start, list, inspect and stop local processes, grouped by work scope.

mod borrows;
mod claims;
mod config;
mod error;
mod gc;
mod handoff;
mod hooks;
mod lease;
mod listing;
mod ports;
mod proc;
mod ready;
mod scope;
mod state;
mod ui;
mod update;
mod util;
mod watch;

use clap::{Parser, Subcommand};
use error::{Next, Output, Result, TabaError, Warning, emit};
use ports::{PortSpec, RunRequest};
use ready::{Outcome, ReadySpec};
use scope::{Caller, ScopeSource, check_ident};
use serde_json::{Value, json};
use state::{OpLock, Service, Store};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        #[arg(long, value_parser = config::parse_service_ttl_flag)]
        service_ttl: Option<u64>,
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
        #[arg(long, conflicts_with = "all")]
        scope: Option<String>,
        /// Only show running services (exited ones are listed by default)
        #[arg(long, conflicts_with = "all")]
        running: bool,
        /// List every listening TCP port of this user, managed by taba or not
        /// (read-only)
        #[arg(long)]
        all: bool,
        /// Print JSON instead of a table (paths are absolute; also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
        /// Keep running and print a full `--json` snapshot, one per line
        /// (NDJSON), first at once and then whenever it changes (needs --json)
        #[arg(long, conflicts_with = "all")]
        watch: bool,
        /// How often --watch re-reads state: SECS, or a number with ms or s
        /// (default 1s, minimum 250ms)
        #[arg(long, requires = "watch", value_parser = watch::parse_interval,
              value_name = "DURATION")]
        interval: Option<Duration>,
    },
    /// Browse every scope's services in a terminal UI; stop, remove,
    /// follow logs and filter with the keyboard
    Ui,
    /// Print a service's log
    Logs {
        /// SCOPE/NAME, or NAME with $TABA_SCOPE or the git worktree's scope
        target: String,
        /// Keep printing new output until the service exits
        #[arg(short, long, conflicts_with = "json")]
        follow: bool,
        /// Print one JSON document with the log text (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
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
        /// Seconds to wait after SIGTERM (Windows: Ctrl+Break) before SIGKILL
        /// (Windows: TerminateJobObject)
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
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Renew the current scope's lease
    Renew {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        #[arg(long)]
        json: bool,
    },
    /// Manage leases
    Lease {
        #[command(subcommand)]
        command: LeaseCmd,
    },
    /// Keep the scope's services after its lease ends
    Keep {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Collect services past their lease or TTL
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Record a shared or exclusive borrow of another scope's service
    Borrow {
        target: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, conflicts_with_all = ["exclusive", "ttl", "wait_lock", "command"])]
        check: bool,
        #[arg(long = "mut")]
        exclusive: bool,
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        #[arg(long, default_value_t = 0.0)]
        wait_lock: f64,
        #[arg(long)]
        json: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// End a borrow by ID or service
    Release {
        target: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Transfer a scope lease or move a service without restarting it
    Move {
        target: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, conflicts_with = "take", required_unless_present = "take")]
        to: Option<String>,
        #[arg(long, conflicts_with = "target")]
        take: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, requires = "force")]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Queue for ownership of another scope's service
    Claim {
        target: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        cancel: bool,
        #[arg(long)]
        json: bool,
    },
    /// Hand a service to its first live claimant
    Disown {
        target: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check for or install a newer stable release from official GitHub Releases
    Update {
        /// Only report whether a newer stable release exists; change nothing
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect agent hook input on stdin
    Hook {
        harness: hooks::Harness,
        event: hooks::Event,
    },
    /// Internal (Windows): send Ctrl+Break to console process group PGID,
    /// attaching to the console of process ATTACH
    #[cfg(windows)]
    #[command(name = "__ctrl-break", hide = true)]
    CtrlBreak { attach: u32, pgid: u32 },
}

#[derive(Subcommand)]
enum LeaseCmd {
    /// End leases without immediately stopping services
    End {
        #[arg(long, conflicts_with = "agent")]
        scope: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn mutating_cmd(
    f: impl FnOnce(&Store, &config::Config) -> Result<Output>,
) -> Result<Option<Output>> {
    let store = Store::open()?;
    let cfg = config::Config::load(store.root())?;
    f(&store, &cfg).map(Some)
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
        #[cfg(windows)]
        Cmd::CtrlBreak { attach, pgid } => {
            return ExitCode::from(proc::ctrl_break_helper(attach, pgid));
        }
        Cmd::Run {
            scope,
            agent,
            name,
            port,
            wait,
            ready,
            ttl,
            service_ttl,
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
                ttl,
                service_ttl,
                command,
            })
            .map(Some),
        ),
        Cmd::Renew {
            scope,
            agent,
            ttl,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|_, cfg| lease::cmd_renew(&scope::resolve(scope, agent)?, cfg, ttl)),
        ),
        Cmd::Lease {
            command: LeaseCmd::End { scope, agent, json },
        } => {
            let bulk = scope.is_some();
            (
                json || env_json(),
                scope::resolve(scope, agent)
                    .and_then(|c| lease::cmd_lease_end(&c, bulk))
                    .map(Some),
            )
        }
        Cmd::Keep {
            scope,
            agent,
            off,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|_, cfg| {
                lease::cmd_keep(&scope::resolve(scope, agent)?, cfg, off, reason, force)
            }),
        ),
        Cmd::Gc { dry_run, json } => (
            json || env_json(),
            mutating_cmd(|store, cfg| gc::collect(store, cfg, dry_run, usize::MAX, None)),
        ),
        Cmd::Borrow {
            target,
            scope,
            agent,
            check,
            exclusive,
            ttl,
            wait_lock,
            json,
            command,
        } => {
            let json = json || env_json();
            (
                json,
                mutating_cmd(|store, cfg| {
                    borrows::cmd(
                        store,
                        cfg,
                        &scope::resolve(scope, agent)?,
                        borrows::Request {
                            target,
                            check,
                            exclusive,
                            ttl,
                            wait: wait_lock,
                            command,
                            json,
                        },
                    )
                }),
            )
        }
        Cmd::Release {
            target,
            scope,
            agent,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, _| {
                borrows::release(store, &scope::resolve(scope, agent)?, &target)
            }),
        ),
        Cmd::Update { check, json } => {
            let json = json || env_json();
            (json, update::command(check, json).map(Some))
        }
        Cmd::Hook { harness, event } => {
            return match hooks::run(harness, event) {
                Ok(value) => {
                    if let Some(value) = value {
                        error::print_json(&value);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("taba hook: {}", e.message);
                    ExitCode::from(1)
                }
            };
        }
        Cmd::Move {
            target,
            scope,
            agent,
            to,
            take,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, cfg| {
                handoff::execute(
                    store,
                    cfg,
                    &scope::resolve(scope, agent)?,
                    handoff::Request {
                        target,
                        to,
                        take,
                        force,
                        reason,
                    },
                )
            }),
        ),
        Cmd::Claim {
            target,
            scope,
            agent,
            cancel,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, cfg| {
                claims::claim(store, cfg, &scope::resolve(scope, agent)?, &target, cancel)
            }),
        ),
        Cmd::Disown {
            target,
            scope,
            agent,
            json,
        } => {
            let bulk = scope.is_some();
            (
                json || env_json(),
                mutating_cmd(|store, cfg| {
                    claims::disown(store, cfg, &scope::resolve(scope, agent)?, target, bulk)
                }),
            )
        }
        Cmd::Ui => (env_json(), ui::command()),
        Cmd::Whoami { scope, agent, json } => {
            (json || env_json(), cmd_whoami(scope, agent).map(Some))
        }
        Cmd::Ps {
            scope,
            running,
            all,
            json,
            watch,
            interval,
        } => {
            let json = json || env_json();
            if watch {
                return ExitCode::from(watch::run(json, scope, running, interval));
            }
            let res = if all {
                listing::cmd_ps_all()
            } else {
                cmd_ps(scope, running)
            };
            (json, res.map(Some))
        }
        Cmd::Logs {
            target,
            follow,
            json,
        } => {
            // `-f` streams raw text; TABA_OUTPUT=json does not apply to it.
            let json = json || (env_json() && !follow);
            (json, cmd_logs(&target, follow, json))
        }
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
            json,
        } => (
            json || env_json(),
            cmd_rm(target, scope, agent, force, reason).map(Some),
        ),
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
    ttl: Option<config::Ttl>,
    service_ttl: Option<u64>,
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
    let cfg = config::Config::load(store.root())?;
    cfg.resolve_ttl(a.ttl, caller.is_human())?;
    gc::collect(&store, &cfg, false, 5, Some(&scope))?;
    let lock = store.lock()?;
    store.mark_version()?;
    let existing = store.get(&scope, &name)?;
    let mut delegation = None;
    if a.scope.is_some()
        && a.ttl.is_none()
        && let Some(ex) = &existing
        && !ex.is_running()
    {
        let borrower = scope::resolve(None, a.agent.clone())?;
        if borrower.scope.as_deref() != Some(scope.as_str()) {
            delegation = borrows::live(&store, ex)?
                .into_iter()
                .find(|b| b.exclusive && b.owned(&borrower));
        }
    }
    let touched = if delegation.is_some() {
        lease::Touched::default()
    } else {
        lease::touch(&store, &cfg, &caller, &scope, lease::Access::Create, a.ttl)?
    };
    let id = format!("{scope}/{name}");
    if let Some(op) = store.live_op(&scope, &name)? {
        return Err(busy(&id, &op));
    }
    store.clear_stale_op(&scope, &name)?;

    if let Some(ex) = &existing
        && !ex.is_running()
        && delegation.is_none()
    {
        borrows::authorise(&store, ex, &caller, false, false)?;
    }
    if let Some(ex) = &existing
        && ex.is_running()
    {
        if ex.template() == template.as_slice()
            && ex.cwd == caller.cwd
            && ex.port_request() == port_request
        {
            let mut ex = ex.clone();
            if let Some(ttl) = a.service_ttl.or(ex.service_ttl_secs) {
                ex.service_ttl_secs = Some(ttl);
                ex.service_expires_at = Some(util::rfc3339(util::now_secs().saturating_add(ttl)));
                store.put(&ex)?;
            }
            drop(lock);
            let mut out = finish_existing(ex, check, wait)?;
            out.warnings.extend(touched.warnings);
            return Ok(out);
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
    let mut warnings = touched.warnings;
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

    let mut cmd = Command::new(proc::resolve_program(&command[0]));
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
    let start_failed = |message: String| {
        TabaError::new("E_START_FAILED", message)
            .details(json!({
                "service": id, "log_path": log_path, "log_tail": log_tail(&log_path, 20),
            }))
            .next(Next::run("read the log", format!("taba logs {id}")))
    };
    // Detached: its own session / process group (Unix) or console and job
    // (Windows), so closing the launching terminal does not stop it.
    let spawned = proc::spawn_detached(cmd).map_err(|e| match e {
        proc::SpawnError::Start(e) => start_failed(format!("cannot start {:?}: {e}", command[0])),
        proc::SpawnError::Vanished => {
            start_failed(format!("{id} exited immediately; see taba logs {id}"))
        }
    })?;
    let pid = spawned.pid;
    let mut svc = Service {
        scope: scope.clone(),
        name: name.clone(),
        command,
        cwd: caller.cwd.clone(),
        port,
        pid,
        pgid: spawned.pgid,
        start_time: spawned.start_time,
        log_path: log_path.clone(),
        command_template: Some(template),
        port_request,
        instance: Some(format!("{pid}-{}", spawned.start_time)),
        started_by: Some(caller.agent.clone()),
        ready: None,
        service_ttl_secs: a.service_ttl,
        service_expires_at: a
            .service_ttl
            .map(|t| util::rfc3339(util::now_secs().saturating_add(t))),
        moves: Vec::new(),
        stop_requested_at: None,
    };
    store.put(&svc)?;
    if let Some(borrow) = &delegation {
        borrows::rebind(&store, borrow, &svc)?;
    }
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
    let cfg = config::Config::load(store.root())?;
    let prospective = lease::prospective_lease(&cfg, &caller)?;
    gc::collect(&store, &cfg, false, 5, caller.scope.as_deref())?;
    let touched = if let Some(s) = &caller.scope {
        let _lock = store.lock()?;
        lease::touch(&store, &cfg, &caller, s, lease::Access::Observe, None)?
    } else {
        lease::Touched::default()
    };
    let rec = touched.record.as_ref();
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
        "holder": rec.map(|r| &r.holder.agent),
        "holder_pid": rec.and_then(|r| r.holder.pid),
        "you_hold_scope": touched.you_hold,
        "lease": caller.scope.as_ref().map(|_| rec.map_or(prospective, |r| r.lease_json(util::now_secs()))),
        "takeover": touched.takeover,
        "next": if rec.is_some() && !touched.you_hold { vec![Next::just("wait for the live lease to end, or ask the coordinator or human").to_json()] } else { Vec::new() },
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
    if let Some(r) = rec {
        text.push_str(&format!(
            "\nholder    {} (TTL {})",
            r.holder.agent,
            r.ttl().display()
        ));
    }
    let mut out = Output::new(body, text);
    out.warnings = touched.warnings;
    Ok(out)
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

/// What `ps` lists: the services (sorted by scope and name), their statuses
/// and their `--json` rows. Building it only reads state.
pub(crate) struct Snapshot {
    pub services: Vec<Service>,
    pub statuses: Vec<&'static str>,
    pub rows: Vec<Value>,
    /// Per service: the scopes holding a live borrow of it (for `ui`).
    pub borrowers: Vec<Vec<String>>,
}

/// Reads the `ps` rows of every service, or of one scope, optionally only
/// the running ones. Shared by `ps`, `ps --watch` and `ui`; it never changes
/// state.
pub(crate) fn ps_snapshot(
    store: &Store,
    scope: Option<&str>,
    running_only: bool,
) -> Result<Snapshot> {
    let mut services = store.list(scope)?;
    if running_only {
        services.retain(|s| s.is_running());
    }
    let statuses: Vec<&'static str> = services.iter().map(|s| status_of(store, s)).collect();

    let mut rows = Vec::new();
    let mut borrowers = Vec::new();
    for (s, status) in services.iter().zip(&statuses) {
        let now = util::now_secs();
        let rec = store.get_scope(&s.scope)?;
        let queue = store.get_claims(&s.scope, &s.name)?;
        let borrows = borrows::live(store, s)?;
        let exclusive = borrows.iter().find(|b| b.exclusive);
        rows.push(json!({
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
            "holder": rec.as_ref().map(|r| &r.holder.agent),
            "lease": rec.as_ref().map(|r| r.lease_json(now)),
            "keep": rec.as_ref().is_some_and(|r| r.keep),
            "keep_reason": rec.as_ref().and_then(|r| r.keep_reason.as_deref()),
            "claims": claims::rows(store, &queue)?,
            "borrows": { "shared": borrows.iter().filter(|b| !b.exclusive).count(), "exclusive": usize::from(exclusive.is_some()) },
            "locked_by": exclusive.map(|b| &b.scope),
        }));
        borrowers.push(borrows.iter().map(|b| b.scope.clone()).collect());
    }
    Ok(Snapshot {
        services,
        statuses,
        rows,
        borrowers,
    })
}

fn cmd_ps(scope: Option<String>, running_only: bool) -> Result<Output> {
    if let Some(s) = &scope {
        check_ident("scope", s)?;
    }
    let store = Store::open()?;
    let caller = scope::resolve(None, None)?;
    let touched = peek_scope(&store, &caller)?;
    let Snapshot {
        services,
        statuses,
        rows,
        ..
    } = ps_snapshot(&store, scope.as_deref(), running_only)?;

    let mut table = vec![[
        "SERVICE".to_string(),
        "STATUS".to_string(),
        "URL".to_string(),
        "DIRECTORY".to_string(),
        "OWNER".to_string(),
        "NEXT".to_string(),
    ]];
    for ((s, status), row) in services.iter().zip(&statuses).zip(&rows) {
        table.push([
            s.id(),
            status.to_string(),
            s.url().unwrap_or_else(|| "-".into()),
            tilde(&s.cwd),
            row["holder"].as_str().map_or_else(
                || "-".into(),
                |a| {
                    if row["lease"]["state"] == "expired" {
                        format!("{a} (expired)")
                    } else {
                        a.into()
                    }
                },
            ),
            row["claims"]
                .as_array()
                .and_then(|q| q.iter().find(|e| e["alive"] == true))
                .and_then(|e| e["scope"].as_str())
                .unwrap_or("-")
                .into(),
        ]);
    }
    let mut body = json!({ "services": rows });
    if touched.takeover_available {
        body["takeover"] = json!("available");
    }
    let mut out = Output::new(body, util::render_table(&table));
    out.warnings = touched.warnings;
    Ok(out)
}

fn peek_scope(store: &Store, caller: &Caller) -> Result<lease::Touched> {
    let Some(scope) = &caller.scope else {
        return Ok(lease::Touched::default());
    };
    let cfg = config::Config::load(store.root())?;
    let _lock = store.lock()?;
    lease::touch(store, &cfg, caller, scope, lease::Access::Peek, None)
}

// ---------------------------------------------------------------- logs

fn cmd_logs(target: &str, follow: bool, json: bool) -> Result<Option<Output>> {
    let caller = scope::resolve(None, None)?;
    let (scope, name) = resolve_target(target, &caller)?;
    let store = Store::open()?;
    let svc = store
        .get(&scope, &name)?
        .ok_or_else(|| not_found(&scope, &name))?;
    let peek = peek_scope(&store, &caller)?;
    let mut file = File::open(&svc.log_path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", svc.log_path.display())))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if json {
        let body = json!({
            "service": svc.id(),
            "log_path": svc.log_path,
            "log": String::from_utf8_lossy(&buf),
        });
        let mut out = Output::new(body, String::new());
        out.warnings = peek.warnings;
        return Ok(Some(out));
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(&buf)?;
    stdout.flush()?;
    if !follow {
        return Ok(None);
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
                return Ok(None); // reader went away (e.g. `| head`)
            }
        } else if !running {
            return Ok(None);
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
    stop_services(
        &store,
        &caller,
        &targets,
        single,
        timeout,
        reason.as_deref(),
    )
}

/// Stops `targets` as `caller`: the body of `taba stop`, also used by
/// `taba ui`. With `single`, the first refusal is returned as the error;
/// otherwise each service is reported under `results`.
pub(crate) fn stop_services(
    store: &Store,
    caller: &Caller,
    targets: &[(String, String)],
    single: bool,
    timeout: Duration,
    reason: Option<&str>,
) -> Result<Output> {
    let mut done = Vec::new();
    let mut refused: Vec<(String, TabaError)> = Vec::new();
    for (s, n) in targets {
        match stop_one(store, caller, s, n, timeout, reason) {
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
            r["platform_action"] = json!(match sig {
                "KILL" => proc::KILL_ACTION,
                _ => proc::GRACEFUL_ACTION,
            });
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
            "killed" => format!("stopped ({} after timeout)", proc::KILL_SIGNAL),
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
    let (svc, preserve_exclusive) = {
        let _lock = store.lock()?;
        let svc = store
            .get(scope, name)?
            .ok_or_else(|| not_found(scope, name))?;
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        if owner && force_reason.is_none() {
            let cfg = config::Config::load(store.root())?;
            lease::touch(store, &cfg, caller, scope, lease::Access::Mutate, None)?;
        }
        let exclusive = !owner
            && borrows::live(store, &svc)?
                .iter()
                .any(|b| b.exclusive && b.owned(caller));
        if !owner && !exclusive && force_reason.is_none() {
            return Err(not_owner(caller, &svc, "stop"));
        }
        let preserve_exclusive =
            borrows::authorise(store, &svc, caller, force_reason.is_some(), true)?
                && force_reason.is_none();
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
        (svc, preserve_exclusive)
    };

    // Re-verify identity immediately before signalling (as v0.1 does).
    let res = if svc.is_running() {
        proc::stop_group(svc.pid, svc.pgid, svc.start_time, timeout).map(Some)
    } else {
        Ok(None)
    };
    {
        let _lock = store.lock()?;
        store.release_op(scope, name)?;
        if res.is_ok() && !svc.is_running() {
            borrows::break_all(store, &svc, preserve_exclusive)?;
            claims::discard(store, &svc)?;
        }
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
    let forced_scope = caller
        .scope
        .clone()
        .filter(|_| reason.is_some() && target.is_none() && scope_flag_given);
    let mut out = rm_services(&store, &caller, reason.clone(), |store| {
        let (mut list, single) = targets(store, target, scope_flag_given, &caller, "remove")?;
        if let Some(scope) = &forced_scope {
            // A forced scope-wide rm also removes the scope's unreadable records.
            list.extend(
                store
                    .unreadable_records(scope)?
                    .into_iter()
                    .map(|n| (scope.clone(), n)),
            );
        }
        Ok((list, single))
    })?;
    if let (Some(scope), Some(reason)) = (forced_scope, reason) {
        // An unreadable scope record blocks every command for the scope;
        // only an explicit forced rm removes it, never taba on its own.
        let _lock = store.lock()?;
        if let Err(e) = store.get_scope(&scope)
            && state::unreadable(&e).is_some()
        {
            store.append_event(&json!({
                "at": util::rfc3339_now(),
                "event": "force_rm_scope_record",
                "agent": caller.agent,
                "caller_scope": caller.scope,
                "cwd": caller.cwd,
                "target": scope,
                "reason": reason,
                "unreadable": true,
            }))?;
            store.remove_scope(&scope)?;
            out.body
                .insert("scope_record".into(), json!("removed_unreadable"));
            out.text.push_str(&format!(
                "
scope {scope}: removed unreadable scope record"
            ));
        }
    }
    Ok(out)
}

/// Removes the services `select` picks (under the global lock) as `caller`:
/// the body of `taba rm`, also used by `taba ui`. `select` returns the
/// targets and whether this is a single-target call, whose refusal is
/// returned as the error.
pub(crate) fn rm_services(
    store: &Store,
    caller: &Caller,
    reason: Option<String>,
    select: impl FnOnce(&Store) -> Result<(Vec<(String, String)>, bool)>,
) -> Result<Output> {
    let _lock = store.lock()?;
    let (targets, single) = select(store)?;

    let mut lines = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut refusals: Vec<TabaError> = Vec::new();
    for (s, n) in &targets {
        let svc = match store.get(s, n) {
            Ok(svc) => svc.ok_or_else(|| not_found(s, n))?,
            Err(e) if state::unreadable(&e).is_some() && reason.is_some() => {
                // An unreadable record: nothing in it can be verified, so only
                // a forced rm removes it, and only when no operation is live.
                let id = format!("{s}/{n}");
                if let Some(op) = store.live_op(s, n)? {
                    return Err(busy(&id, &op));
                }
                store.append_event(&json!({
                    "at": util::rfc3339_now(),
                    "event": "force_rm",
                    "agent": caller.agent,
                    "caller_scope": caller.scope,
                    "cwd": caller.cwd,
                    "target": id,
                    "reason": reason,
                    "unreadable": true,
                }))?;
                store.remove(s, n)?;
                lines.push(format!(
                    "{id}: removed unreadable record and its log (a process it started is no longer tracked)"
                ));
                results.push(json!({ "service": id, "result": "removed", "unreadable": true }));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        let lease_refusal = if owner && reason.is_none() {
            let cfg = config::Config::load(store.root())?;
            lease::touch(store, &cfg, caller, s, lease::Access::Mutate, None).err()
        } else {
            None
        };
        let refusal = if let Some(e) = lease_refusal {
            Some(e)
        } else if !owner && reason.is_none() {
            Some(not_owner(caller, &svc, "rm"))
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
            borrows::authorise(store, &svc, caller, reason.is_some(), false).err()
        };
        if let Some(e) = refusal {
            if single {
                return Err(e);
            }
            results.push(json!({
                "service": svc.id(), "result": "refused", "error": e.to_json(),
            }));
            refusals.push(e);
            continue;
        }
        if let Some(r) = &reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            record_force(store, "force_rm", caller, &svc, r, overrode)?;
        }
        borrows::break_all(store, &svc, false)?;
        store.remove(&svc.scope, &svc.name)?;
        lines.push(format!("{}: removed (record and log)", svc.id()));
        results.push(json!({ "service": svc.id(), "result": "removed" }));
    }
    if targets.is_empty() {
        lines.push(format!(
            "no services in scope {}",
            caller.scope.clone().unwrap_or_default()
        ));
    }
    let mut out = Output::new(json!({ "results": results }), lines.join("\n"));
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
