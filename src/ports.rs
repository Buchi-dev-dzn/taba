//! Port requests, the free-port check and `--port auto` selection.

use crate::error::{Next, Result, TabaError};
use crate::proc;
use crate::state::{Service, Store};
use crate::util::{shell_join, tilde};
use serde_json::json;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

/// Default range of `--port auto`. It avoids the common defaults 3000, 5173,
/// 8000 and 8080 that humans and unmanaged tools use.
pub const AUTO_RANGE: (u16, u16) = (4100, 4999);
/// How many ports `--port auto:START` tries, starting at START.
pub const AUTO_START_SPAN: u16 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortSpec {
    Fixed(u16),
    Auto { first: u16, last: u16, text: String },
}

impl PortSpec {
    /// The request as recorded in `port_request` and compared by re-runs.
    pub fn request(&self) -> String {
        match self {
            PortSpec::Fixed(p) => p.to_string(),
            PortSpec::Auto { text, .. } => text.clone(),
        }
    }
}

/// clap value parser for `--port`: `P`, `auto` or `auto:START`.
pub fn parse_port_spec(s: &str) -> std::result::Result<PortSpec, String> {
    let port = |v: &str| -> std::result::Result<u16, String> {
        match v.parse::<u16>() {
            Ok(p) if p > 0 => Ok(p),
            _ => Err(format!("{v:?} is not a port number (1-65535)")),
        }
    };
    if s == "auto" {
        return Ok(PortSpec::Auto {
            first: AUTO_RANGE.0,
            last: AUTO_RANGE.1,
            text: "auto".into(),
        });
    }
    if let Some(start) = s.strip_prefix("auto:") {
        let first = port(start)?;
        let last = first.saturating_add(AUTO_START_SPAN - 1);
        return Ok(PortSpec::Auto {
            first,
            last,
            text: format!("auto:{first}"),
        });
    }
    port(s).map(PortSpec::Fixed)
}

/// Whether something listens on `port`: a loopback connect succeeds (this
/// also sees servers forwarded from WSL, a VM or a container), a wildcard
/// bind fails, or (Windows) the TCP table lists a listener.
pub fn port_in_use(port: u16) -> bool {
    // A refused loopback connect is immediate on macOS and Linux; Windows
    // retries it for about two seconds, so it gets a short timeout, and the
    // TCP table check comes first.
    #[cfg(windows)]
    if !proc::listeners(port).is_empty() {
        return true;
    }
    let timeout = Duration::from_millis(if cfg!(windows) { 150 } else { 300 });
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
    match TcpListener::bind(wildcard) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => true,
        // Windows refuses binds in reserved ranges (`netsh int ipv4 show
        // excludedportrange`, e.g. Hyper-V's) with WSAEACCES: no server can
        // use such a port either.
        Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => true,
        _ => false,
    }
}

/// Command line of `pid` for messages: the program's file name plus its
/// arguments, shortened in the middle so both ends stay readable.
pub fn command_line(pid: i32) -> Option<String> {
    const MAX: usize = 100;
    let mut args = proc::argv(pid)?;
    if let Some(base) = Path::new(&args[0]).file_name() {
        args[0] = base.to_string_lossy().into_owned();
    }
    let line = join_args(&args);
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX {
        return Some(line);
    }
    let head: String = chars[..MAX / 2].iter().collect();
    let tail: String = chars[chars.len() - (MAX / 2 - 3)..].iter().collect();
    Some(format!("{head} … {tail}"))
}

/// Full command line of `pid` (program path and arguments), for `ps --all`.
pub fn command_line_full(pid: i32) -> Option<String> {
    proc::argv(pid).map(|a| join_args(&a))
}

fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(char::is_whitespace) {
                format!("'{a}'")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `wslrelay.exe` listens on Windows for ports of servers inside WSL 2
/// (NAT networking with localhost forwarding).
fn is_wsl_relay(comm: &str) -> bool {
    comm.eq_ignore_ascii_case("wslrelay.exe") || comm.eq_ignore_ascii_case("wslrelay")
}

/// What the caller asked for, used to build `next` suggestions.
pub struct RunRequest<'a> {
    pub scope_flag: Option<&'a str>,
    pub name: &'a str,
    pub template: &'a [String],
}

impl RunRequest<'_> {
    /// `taba run ... --port auto --wait -- CMD`, with a literal port argument
    /// in CMD replaced by `{port}`.
    fn auto_command(&self, port: u16) -> String {
        let p = port.to_string();
        let cmd: Vec<String> = self
            .template
            .iter()
            .map(|a| {
                if *a == p {
                    "{port}".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        let scope = self
            .scope_flag
            .map(|s| format!(" --scope {s}"))
            .unwrap_or_default();
        format!(
            "taba run{scope} --name {} --port auto --wait -- {}",
            self.name,
            shell_join(&cmd)
        )
    }
}

/// Running (or starting) taba services, excluding `scope/name` itself.
fn reservations(store: &Store, scope: &str, name: &str) -> Result<Vec<Service>> {
    let mut out = Vec::new();
    for s in store.list(None)? {
        if s.scope == scope && s.name == name {
            continue;
        }
        if s.port.is_some() && (s.is_running() || store.live_op(&s.scope, &s.name)?.is_some()) {
            out.push(s);
        }
    }
    Ok(out)
}

/// Checks that `port` is neither reserved by another taba service nor in use.
pub fn check_port_free(store: &Store, port: u16, scope: &str, req: &RunRequest) -> Result<()> {
    let reserved = reservations(store, scope, req.name)?;
    if let Some(owner) = reserved.iter().find(|s| s.port == Some(port)) {
        return Err(TabaError::new(
            "E_PORT_CLAIMED",
            format!(
                "port {port} is already claimed by taba service {} ({}); nothing was started",
                owner.id(),
                tilde(&owner.cwd)
            ),
        )
        .details(json!({
            "port": port,
            "holder": { "kind": "taba", "service": owner.id(), "scope": owner.scope,
                        "pid": owner.pid, "directory": owner.cwd },
        }))
        .next(Next::run(
            "start on a free port instead",
            req.auto_command(port),
        ))
        .next(Next::just(&format!(
            "use the existing server read-only at http://localhost:{port}; do not stop it"
        ))));
    }
    if !port_in_use(port) {
        return Ok(());
    }
    let holders = proc::listeners(port);
    let running: Vec<Service> = store
        .list(None)?
        .into_iter()
        .filter(|s| s.is_running())
        .collect();
    for (pid, comm) in &holders {
        if let Some(owner) = running
            .iter()
            .find(|s| proc::in_group(*pid, s.pid, s.pgid, s.start_time))
        {
            return Err(TabaError::new(
                "E_PORT_IN_USE",
                format!(
                    "port {port} is in use by taba service {} (pid {pid} {comm}, {}); nothing was started",
                    owner.id(),
                    tilde(&owner.cwd)
                ),
            )
            .details(json!({
                "port": port,
                "holder": { "kind": "taba", "service": owner.id(), "scope": owner.scope,
                            "pid": pid, "directory": owner.cwd },
            }))
            .next(Next::run("start on a free port instead", req.auto_command(port)))
            .next(Next::just(&format!(
                "use the existing server read-only at http://localhost:{port}; do not stop it"
            ))));
        }
    }
    let (message, holder) = match holders.first() {
        Some((pid, comm)) if is_wsl_relay(comm) => (
            format!(
                "port {port} is in use by a server inside WSL, forwarded to Windows by {comm}                  (pid {pid}); taba on Windows cannot see or manage processes inside WSL
                 nothing was started"
            ),
            json!({ "kind": "unknown", "pid": pid, "command": comm, "outside": "wsl" }),
        ),
        Some((pid, comm)) => {
            let line = command_line(*pid).unwrap_or_else(|| comm.clone());
            (
                format!(
                    "port {port} is in use by pid {pid}, which is not managed by taba:\n  {line}\n\
                     nothing was started"
                ),
                json!({ "kind": "unmanaged", "pid": pid, "command": line }),
            )
        }
        None => (
            format!(
                "port {port} is in use by a process taba cannot identify (another user's, or one                  outside this OS, e.g. in WSL, a VM or a container); nothing was started"
            ),
            json!({ "kind": "unknown" }),
        ),
    };
    Err(TabaError::new("E_PORT_IN_USE", message)
        .details(json!({ "port": port, "holder": holder }))
        .next(Next::run("start on a free port instead", req.auto_command(port)))
        .next(Next::just(
            "ask the human: taba did not start that process and will not stop it; they can stop it themselves",
        )))
}

/// Picks the lowest free port in the spec's range. Call under the global
/// lock, and record the service before releasing it, so that concurrent
/// runs never pick the same port.
pub fn pick_auto(store: &Store, first: u16, last: u16, scope: &str, name: &str) -> Result<u16> {
    let reserved: Vec<u16> = reservations(store, scope, name)?
        .iter()
        .filter_map(|s| s.port)
        .collect();
    for port in first..=last {
        if !reserved.contains(&port) && !port_in_use(port) {
            return Ok(port);
        }
    }
    let next_start = if last < u16::MAX {
        last.saturating_add(1)
    } else {
        AUTO_RANGE.0
    };
    Err(TabaError::new(
        "E_NO_FREE_PORT",
        format!("no free port in {first}-{last}; nothing was started"),
    )
    .details(json!({ "range": [first, last] }))
    .next(Next::just(&format!(
        "retry with another range, for example --port auto:{next_start}"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_specs() {
        assert_eq!(parse_port_spec("5173"), Ok(PortSpec::Fixed(5173)));
        assert_eq!(
            parse_port_spec("auto"),
            Ok(PortSpec::Auto {
                first: 4100,
                last: 4999,
                text: "auto".into()
            })
        );
        assert_eq!(
            parse_port_spec("auto:8001"),
            Ok(PortSpec::Auto {
                first: 8001,
                last: 9000,
                text: "auto:8001".into()
            })
        );
        assert_eq!(
            parse_port_spec("auto:65000"),
            Ok(PortSpec::Auto {
                first: 65000,
                last: 65535,
                text: "auto:65000".into()
            })
        );
        assert!(parse_port_spec("0").is_err());
        assert!(parse_port_spec("auto:x").is_err());
        assert!(parse_port_spec("70000").is_err());
        assert!(parse_port_spec("").is_err());
    }
}
