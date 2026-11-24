//! taba: start, list, inspect and stop local processes, grouped by work scope.

mod proc;
mod state;

use clap::{Parser, Subcommand};
use state::{Service, Store, validate_ident};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        /// Work scope that owns the service (default: $TABA_SCOPE)
        #[arg(long)]
        scope: Option<String>,
        /// Service name, unique within the scope
        #[arg(long)]
        name: String,
        /// Port the service will listen on (tracked only; pass it to the command yourself)
        #[arg(long)]
        port: Option<u16>,
        /// Command to run, after `--`
        #[arg(last = true, required = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// List services
    Ps {
        /// Only show this scope
        #[arg(long)]
        scope: Option<String>,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Print a service's log
    Logs {
        /// SCOPE/NAME, or NAME with $TABA_SCOPE
        target: String,
        /// Keep printing new output until the service exits
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop a service (SCOPE/NAME) or every service in a scope (--scope)
    Stop {
        /// SCOPE/NAME, or NAME with --scope / $TABA_SCOPE
        target: Option<String>,
        /// Scope to stop (all its services when no target is given)
        #[arg(long)]
        scope: Option<String>,
        /// Seconds to wait after SIGTERM before sending SIGKILL
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
    },
}

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Run {
            scope,
            name,
            port,
            command,
        } => cmd_run(scope, name, port, command),
        Cmd::Ps { scope, json } => cmd_ps(scope, json),
        Cmd::Logs { target, follow } => cmd_logs(&target, follow),
        Cmd::Stop {
            target,
            scope,
            timeout,
        } => cmd_stop(target, scope, timeout),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("taba: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn io_err(e: io::Error) -> String {
    e.to_string()
}

fn resolve_scope(flag: Option<String>) -> Result<String> {
    let scope = match flag {
        Some(s) => s,
        None => match std::env::var("TABA_SCOPE") {
            Ok(s) if !s.is_empty() => s,
            _ => return Err("no scope given: pass --scope or set TABA_SCOPE".into()),
        },
    };
    validate_ident("scope", &scope)?;
    Ok(scope)
}

/// Parses `SCOPE/NAME`, or `NAME` with the scope from the flag / $TABA_SCOPE.
fn resolve_target(target: &str, scope_flag: Option<String>) -> Result<(String, String)> {
    let (scope, name) = match target.split_once('/') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => (resolve_scope(scope_flag)?, target.to_string()),
    };
    validate_ident("scope", &scope)?;
    validate_ident("name", &name)?;
    Ok((scope, name))
}

fn describe(svc: &Service) -> String {
    let mut s = format!("{}  pid {}  dir {}", svc.id(), svc.pid, svc.cwd.display());
    if let Some(url) = svc.url() {
        s.push_str(&format!("  url {url}"));
    }
    s
}

// ---------------------------------------------------------------- run

fn cmd_run(
    scope: Option<String>,
    name: String,
    port: Option<u16>,
    command: Vec<String>,
) -> Result<()> {
    let scope = resolve_scope(scope)?;
    validate_ident("name", &name)?;
    let cwd = std::env::current_dir()
        .and_then(|d| d.canonicalize())
        .map_err(|e| format!("cannot resolve current directory: {e}"))?;

    let store = Store::open().map_err(io_err)?;
    let _lock = store.lock().map_err(io_err)?;

    if let Some(existing) = store.get(&scope, &name).map_err(io_err)?
        && existing.is_running()
    {
        if existing.command == command && existing.cwd == cwd && existing.port == port {
            println!("already running: {}", describe(&existing));
            return Ok(());
        }
        return Err(format!(
            "{} is already running with a different command, directory or port \
                 (command {:?}, dir {}, port {}); stop it first",
            existing.id(),
            existing.command,
            existing.cwd.display(),
            existing.port.map_or("-".to_string(), |p| p.to_string()),
        ));
    }
    // An exited record is simply replaced.

    if let Some(port) = port {
        check_port_free(&store, port, &scope, &name)?;
    }

    let log_path = store.log_path(&scope, &name);
    std::fs::create_dir_all(log_path.parent().expect("log has parent")).map_err(io_err)?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(
        log,
        "--- taba: starting {:?} in {} (unix time {now}) ---",
        command,
        cwd.display()
    )
    .map_err(io_err)?;
    let log_err = log.try_clone().map_err(io_err)?;

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(&cwd)
        .env("TABA_SCOPE", &scope)
        .env("TABA_SERVICE", &name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
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
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {:?}: {e}", command[0]))?;
    let pid = child.id() as i32;
    // The child is intentionally not waited on: once taba exits it is
    // re-parented to launchd, which reaps it.
    drop(child);

    let info = proc::info(pid).ok_or_else(|| {
        format!(
            "{scope}/{name} exited immediately; see {}",
            log_path.display()
        )
    })?;
    let svc = Service {
        scope,
        name,
        command,
        cwd,
        port,
        pid,
        pgid: pid,
        start_time: info.start_time,
        log_path,
    };
    store.put(&svc).map_err(io_err)?;

    thread::sleep(Duration::from_millis(200));
    if svc.is_running() {
        println!("started: {}", describe(&svc));
        println!("logs:    {}", svc.log_path.display());
        Ok(())
    } else {
        Err(format!(
            "{} exited right after starting; see {}",
            svc.id(),
            svc.log_path.display()
        ))
    }
}

fn port_in_use(port: u16) -> bool {
    let timeout = Duration::from_millis(300);
    let loopbacks: [SocketAddr; 2] = [
        ([127, 0, 0, 1], port).into(),
        (std::net::Ipv6Addr::LOCALHOST, port).into(),
    ];
    if loopbacks
        .iter()
        .any(|a| TcpStream::connect_timeout(a, timeout).is_ok())
    {
        return true;
    }
    let wildcard: SocketAddr = ([0, 0, 0, 0], port).into();
    matches!(TcpListener::bind(wildcard), Err(e) if e.kind() == io::ErrorKind::AddrInUse)
}

/// Pids listening on `port`, with their command names, via lsof.
fn listeners(port: u16) -> Vec<(i32, String)> {
    let out = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    let mut res = Vec::new();
    let mut cur: Option<i32> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            cur = p.parse().ok();
        } else if let (Some(c), Some(pid)) = (line.strip_prefix('c'), cur) {
            res.push((pid, c.to_string()));
        }
    }
    res
}

fn check_port_free(store: &Store, port: u16, scope: &str, name: &str) -> Result<()> {
    let running: Vec<Service> = store
        .list(None)
        .map_err(io_err)?
        .into_iter()
        .filter(|s| s.is_running())
        .collect();
    if let Some(owner) = running
        .iter()
        .find(|s| s.port == Some(port) && !(s.scope == scope && s.name == name))
    {
        return Err(format!(
            "port {port} is already claimed by taba service {} ({})",
            owner.id(),
            owner.cwd.display()
        ));
    }
    if !port_in_use(port) {
        return Ok(());
    }
    let holders = listeners(port);
    for (pid, comm) in &holders {
        if let Some(pi) = proc::info(*pid)
            && let Some(owner) = running.iter().find(|s| s.pgid == pi.pgid)
        {
            return Err(format!(
                "port {port} is in use by taba service {} (pid {pid} {comm}, {})",
                owner.id(),
                owner.cwd.display()
            ));
        }
    }
    match holders.first() {
        Some((pid, comm)) => Err(format!(
            "port {port} is in use by pid {pid} ({comm}), which is not managed by taba"
        )),
        None => Err(format!(
            "port {port} is in use by a process taba cannot identify"
        )),
    }
}

// ---------------------------------------------------------------- ps

fn cmd_ps(scope: Option<String>, json: bool) -> Result<()> {
    if let Some(s) = &scope {
        validate_ident("scope", s)?;
    }
    let store = Store::open().map_err(io_err)?;
    let services = store.list(scope.as_deref()).map_err(io_err)?;

    if json {
        let rows: Vec<serde_json::Value> = services
            .iter()
            .map(|s| {
                serde_json::json!({
                    "service": s.id(),
                    "scope": s.scope,
                    "name": s.name,
                    "status": if s.is_running() { "running" } else { "exited" },
                    "url": s.url(),
                    "port": s.port,
                    "directory": s.cwd,
                    "command": s.command,
                    "pid": s.pid,
                    "pgid": s.pgid,
                    "start_time": s.start_time,
                    "log_path": s.log_path,
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?;
        println!("{out}");
        return Ok(());
    }

    let mut rows = vec![[
        "SERVICE".to_string(),
        "STATUS".to_string(),
        "URL".to_string(),
        "DIRECTORY".to_string(),
    ]];
    for s in &services {
        rows.push([
            s.id(),
            if s.is_running() { "running" } else { "exited" }.to_string(),
            s.url().unwrap_or_else(|| "-".into()),
            s.cwd.display().to_string(),
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
    let (scope, name) = resolve_target(target, None)?;
    let store = Store::open().map_err(io_err)?;
    let svc = store
        .get(&scope, &name)
        .map_err(io_err)?
        .ok_or_else(|| format!("no such service: {scope}/{name}"))?;
    let mut file = File::open(&svc.log_path)
        .map_err(|e| format!("cannot open {}: {e}", svc.log_path.display()))?;
    let mut stdout = io::stdout().lock();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(io_err)?;
    stdout.write_all(&buf).map_err(io_err)?;
    stdout.flush().map_err(io_err)?;
    if !follow {
        return Ok(());
    }
    let mut pos = file.stream_position().map_err(io_err)?;
    loop {
        let running = svc.is_running();
        file.seek(SeekFrom::Start(pos)).map_err(io_err)?;
        buf.clear();
        file.read_to_end(&mut buf).map_err(io_err)?;
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

fn cmd_stop(target: Option<String>, scope: Option<String>, timeout: f64) -> Result<()> {
    if !timeout.is_finite() || timeout < 0.0 {
        return Err("--timeout must be a non-negative number of seconds".into());
    }
    let timeout = Duration::from_secs_f64(timeout);
    let store = Store::open().map_err(io_err)?;

    let services: Vec<Service> = match target {
        Some(t) => {
            let (s, n) = resolve_target(&t, scope)?;
            let _lock = store.lock().map_err(io_err)?;
            vec![
                store
                    .get(&s, &n)
                    .map_err(io_err)?
                    .ok_or_else(|| format!("no such service: {s}/{n}"))?,
            ]
        }
        None => {
            if scope.is_none() {
                return Err("nothing to stop: give SCOPE/NAME or --scope SCOPE".into());
            }
            let s = resolve_scope(scope)?;
            let _lock = store.lock().map_err(io_err)?;
            let list = store.list(Some(&s)).map_err(io_err)?;
            if list.is_empty() {
                println!("no services in scope {s}");
            }
            list
        }
    };

    let mut failures = 0;
    for svc in services {
        if !svc.is_running() {
            println!("{}: not running (exited)", svc.id());
            continue;
        }
        match proc::stop_group(svc.pgid, timeout) {
            Ok(proc::StopResult::Terminated) => println!("{}: stopped", svc.id()),
            Ok(proc::StopResult::Killed) => {
                println!("{}: stopped (SIGKILL after timeout)", svc.id())
            }
            Err(e) => {
                eprintln!(
                    "taba: {}: cannot signal process group {}: {e}",
                    svc.id(),
                    svc.pgid
                );
                failures += 1;
            }
        }
    }
    if failures > 0 {
        Err(format!("{failures} service(s) could not be stopped"))
    } else {
        Ok(())
    }
}
