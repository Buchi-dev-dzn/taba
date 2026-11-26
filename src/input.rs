//! Interactive input for services (#68): `run --interactive`, `send`, `attach`
//! and `signal`.
//!
//! A service started with `run --interactive` gets a FIFO in
//! `$TABA_HOME/inputs/<scope>/<name>.fifo` as its stdin, opened read-only.
//! The service also inherits a write end of the same FIFO as an extra file
//! descriptor, so it never reads EOF while nobody is sending, and no taba
//! process stays behind. (Stdin itself is not opened read-write: the Dart VM,
//! and so `flutter run`, receives nothing from a read-write FIFO on macOS.)
//! `send` and `attach` open the FIFO for writing, write and close it.
//!
//! Writing to a service's stdin, or signalling it, mutates someone else's
//! process, so the same rules as `stop` apply: the owning scope (under its
//! lease) or the holder of an exclusive borrow; anyone else needs
//! `--force --reason`, which is recorded in events.jsonl. While another scope
//! holds an exclusive borrow, the owner is refused with `E_LOCKED`.
//!
//! Windows (named pipes) is not supported yet: these commands fail there
//! with `E_UNSUPPORTED`.

use crate::error::{Next, Output, Result, TabaError};
use crate::scope::{self, Caller};
use crate::state::{Service, Store};
use crate::{borrows, config, lease};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a write waits for a service that does not read its stdin.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The byte that ends `attach` (Ctrl+]).
const DETACH: u8 = 0x1d;

pub fn unsupported(what: &str) -> TabaError {
    TabaError::new(
        "E_UNSUPPORTED",
        format!("{what} is not supported on this platform yet (Unix only)"),
    )
    .details(json!({ "feature": what, "platform": std::env::consts::OS }))
}

/// The input FIFO of a service being started: `stdin` for the child, and
/// `keep`, a write end without close-on-exec that the child inherits so its
/// stdin never reaches EOF. taba drops both after the spawn.
pub struct Fifo {
    pub path: PathBuf,
    pub stdin: std::fs::File,
    #[cfg(unix)]
    pub _keep: std::os::fd::OwnedFd,
}

/// Creates the input FIFO of `scope/name` (replacing a stale one) and opens
/// both ends for the child.
#[cfg(unix)]
pub fn create(store: &Store, scope: &str, name: &str) -> Result<Fifo> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let path = store.input_path(scope, name);
    let fail = |what: &str, e: std::io::Error| {
        TabaError::state(format!("cannot {what} input FIFO {}: {e}", path.display()))
    };
    crate::state::create_private_dir_all(path.parent().expect("input has parent"))?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| TabaError::state("input path contains a NUL byte"))?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(fail("create", std::io::Error::last_os_error()));
    }
    // Non-blocking, as no writer exists yet; cleared below.
    let stdin = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| fail("open", e))?;
    // Without O_CLOEXEC, so the child inherits it. taba is single-threaded
    // here and spawns nothing else before dropping it.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        return Err(fail("open", std::io::Error::last_os_error()));
    }
    let keep = unsafe { OwnedFd::from_raw_fd(fd) };
    let raw = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(fail("set up", std::io::Error::last_os_error()));
    }
    Ok(Fifo {
        path,
        stdin,
        _keep: keep,
    })
}

#[cfg(windows)]
pub fn create(_store: &Store, _scope: &str, _name: &str) -> Result<Fifo> {
    Err(unsupported("run --interactive").next(Next::just(
        "start it without --interactive; see https://github.com/Buchi-dev-dzn/taba/issues/68",
    )))
}

/// Writes `bytes` to the service's input FIFO without blocking forever on a
/// service that does not read.
#[cfg(unix)]
pub fn write(svc: &Service, path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::{ErrorKind, Write};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    let is_fifo = path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_fifo());
    if !is_fifo {
        return Err(no_input(svc).details(json!({ "service": svc.id(), "path": path })));
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            // No reader: the service (and every process holding its stdin) is gone.
            Some(libc::ENXIO) => not_running(svc),
            _ => TabaError::state(format!("cannot open {}: {e}", path.display())),
        })?;
    let deadline = std::time::Instant::now() + WRITE_TIMEOUT;
    let mut rest = bytes;
    while !rest.is_empty() {
        match f.write(rest) {
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(TabaError::new(
                        "E_INPUT_BLOCKED",
                        format!(
                            "{} is not reading its input; {} of {} bytes were not written",
                            svc.id(),
                            rest.len(),
                            bytes.len()
                        ),
                    )
                    .details(json!({ "service": svc.id(), "unwritten": rest.len() }))
                    .next(Next::run(
                        "check the service",
                        format!("taba logs {}", svc.id()),
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => return Err(not_running(svc)),
            Err(e) => return Err(TabaError::state(format!("cannot write input: {e}"))),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn write(_svc: &Service, _path: &Path, _bytes: &[u8]) -> Result<()> {
    Err(unsupported("service input"))
}

fn no_input(svc: &Service) -> TabaError {
    TabaError::new(
        "E_NO_INPUT",
        format!(
            "{} was not started with --interactive, so it has no stdin to write to",
            svc.id()
        ),
    )
    .details(json!({ "service": svc.id() }))
    .next(Next::run(
        "restart it with --interactive (stop it first)",
        format!("taba stop {}", svc.id()),
    ))
}

fn not_running(svc: &Service) -> TabaError {
    TabaError::new("E_NOT_RUNNING", format!("{} is not running", svc.id()))
        .details(json!({ "service": svc.id(), "instance": svc.instance() }))
        .next(Next::run("read its log", format!("taba logs {}", svc.id())))
}

/// The service `target` names, if `caller` may write to or signal it (the
/// `stop` rules, see the module docs). Records a `--force` override.
pub(crate) fn authorise(
    store: &Store,
    caller: &Caller,
    target: &str,
    verb: &str,
    force_reason: Option<&str>,
    needs_input: bool,
) -> Result<Service> {
    let (scope, name) = crate::resolve_target(target, caller)?;
    let _lock = store.lock()?;
    let svc = store
        .get(&scope, &name)?
        .ok_or_else(|| crate::not_found(&scope, &name))?;
    let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
    if owner && force_reason.is_none() {
        let cfg = config::Config::load(store.root())?;
        lease::touch(store, &cfg, caller, &scope, lease::Access::Mutate, None)?;
    }
    let live = borrows::live(store, &svc)?;
    let exclusive = live.iter().find(|b| b.exclusive);
    let holds_exclusive = exclusive.is_some_and(|b| b.owned(caller));
    if force_reason.is_none() {
        if !owner && !holds_exclusive {
            return Err(crate::not_owner(caller, &svc, verb));
        }
        if let Some(b) = exclusive.filter(|_| !holds_exclusive) {
            return Err(borrows::locked(&svc, b));
        }
    }
    if let Some(op) = store.live_op(&scope, &name)? {
        return Err(crate::busy(&svc.id(), &op));
    }
    if !svc.is_running() {
        return Err(not_running(&svc));
    }
    if needs_input && svc.input.is_none() {
        return Err(no_input(&svc));
    }
    if let Some(reason) = force_reason {
        let overrode: &[&str] = if owner || holds_exclusive {
            &[]
        } else {
            &["E_NOT_OWNER"]
        };
        crate::record_force(
            store,
            &format!("force_{verb}"),
            caller,
            &svc,
            reason,
            overrode,
        )?;
    }
    Ok(svc)
}

/// The bytes of one `--key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key(pub Vec<u8>);

/// The bytes `--key` names: one character, or `enter`, `esc`, `tab`,
/// `space`, `backspace` or `ctrl-X`.
pub fn parse_key(s: &str) -> std::result::Result<Key, String> {
    key_bytes(s).map(Key)
}

fn key_bytes(s: &str) -> std::result::Result<Vec<u8>, String> {
    let named = match s.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(b"\n".to_vec()),
        "esc" | "escape" => Some(vec![0x1b]),
        "tab" => Some(b"\t".to_vec()),
        "space" => Some(b" ".to_vec()),
        "backspace" => Some(vec![0x7f]),
        _ => None,
    };
    if let Some(b) = named {
        return Ok(b);
    }
    let lower = s.to_ascii_lowercase();
    if let Some(c) = lower.strip_prefix("ctrl-").or(lower.strip_prefix("ctrl+"))
        && let [c @ b'a'..=b'z'] = c.as_bytes()
    {
        return Ok(vec![c - b'a' + 1]);
    }
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c.to_string().into_bytes()),
        _ => Err(format!(
            "`{s}` is not a key: use one character (r, R, q, ...), enter, esc, tab, space, \
             backspace or ctrl-X"
        )),
    }
}

pub struct SendArgs {
    pub target: String,
    pub text: Option<String>,
    pub key: Option<Key>,
    pub no_newline: bool,
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
}

pub fn cmd_send(a: SendArgs) -> Result<Output> {
    let reason = crate::check_force(a.force, a.reason)?;
    let bytes = match (a.text, a.key) {
        (Some(t), None) if a.no_newline => t.into_bytes(),
        (Some(t), None) => format!("{t}\n").into_bytes(),
        (None, Some(Key(k))) => k,
        _ => return Err(TabaError::usage("give TEXT or --key KEY")),
    };
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    let svc = authorise(&store, &caller, &a.target, "send", reason.as_deref(), true)?;
    let path = svc.input.clone().expect("checked");
    write(&svc, &path, &bytes)?;
    let mut body = json!({
        "service": svc.id(),
        "bytes": bytes.len(),
        "instance": svc.instance(),
    });
    if reason.is_some() {
        body["forced"] = json!(true);
    }
    Ok(Output::new(
        body,
        format!("{}: sent {} byte(s)", svc.id(), bytes.len()),
    ))
}

/// `taba signal`: the signals `signal` accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Signal {
    #[value(name = "USR1", alias = "usr1", alias = "SIGUSR1")]
    Usr1,
    #[value(name = "USR2", alias = "usr2", alias = "SIGUSR2")]
    Usr2,
    #[value(name = "HUP", alias = "hup", alias = "SIGHUP")]
    Hup,
}

impl Signal {
    fn name(self) -> &'static str {
        match self {
            Signal::Usr1 => "USR1",
            Signal::Usr2 => "USR2",
            Signal::Hup => "HUP",
        }
    }
}

pub struct SignalArgs {
    pub target: String,
    pub signal: Signal,
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
}

pub fn cmd_signal(a: SignalArgs) -> Result<Output> {
    let reason = crate::check_force(a.force, a.reason)?;
    if cfg!(windows) {
        return Err(unsupported("taba signal"));
    }
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    let svc = authorise(
        &store,
        &caller,
        &a.target,
        "signal",
        reason.as_deref(),
        false,
    )?;
    send_signal(&svc, a.signal)?;
    let mut body = json!({
        "service": svc.id(),
        "signal": a.signal.name(),
        "pid": svc.pid,
        "instance": svc.instance(),
    });
    if reason.is_some() {
        body["forced"] = json!(true);
    }
    Ok(Output::new(
        body,
        format!(
            "{}: sent SIG{} to pid {}",
            svc.id(),
            a.signal.name(),
            svc.pid
        ),
    ))
}

/// Signals the service's main process only: the default action of SIGUSR1
/// and SIGUSR2 is to terminate, so the rest of the group is left alone.
#[cfg(unix)]
fn send_signal(svc: &Service, sig: Signal) -> Result<()> {
    let n = match sig {
        Signal::Usr1 => libc::SIGUSR1,
        Signal::Usr2 => libc::SIGUSR2,
        Signal::Hup => libc::SIGHUP,
    };
    // Re-verify the identity right before signalling, as `stop` does.
    if !crate::proc::is_alive(svc.pid, svc.start_time) {
        return Err(not_running(svc));
    }
    if unsafe { libc::kill(svc.pid, n) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(TabaError::new(
            "E_SIGNAL_FAILED",
            format!("{}: cannot signal pid {}: {e}", svc.id(), svc.pid),
        )
        .details(json!({ "service": svc.id(), "pid": svc.pid })));
    }
    Ok(())
}

#[cfg(windows)]
fn send_signal(_svc: &Service, _sig: Signal) -> Result<()> {
    Err(unsupported("taba signal"))
}

pub struct AttachArgs {
    pub target: String,
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
}

pub fn cmd_attach(a: AttachArgs) -> Result<Option<Output>> {
    let reason = crate::check_force(a.force, a.reason)?;
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    attach(&store, &caller, &a.target, reason.as_deref())?;
    Ok(None)
}

/// Restores the terminal mode when `attach` ends, also on an error.
struct RawMode(bool);
impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Shows the service's log and forwards the caller's input to its stdin
/// until Ctrl+] (on a terminal) or EOF (on a pipe), or until the service
/// exits. The service keeps running after a detach. Also used by `taba ui`.
pub(crate) fn attach(
    store: &Store,
    caller: &Caller,
    target: &str,
    force_reason: Option<&str>,
) -> Result<()> {
    use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};

    let svc = authorise(store, caller, target, "attach", force_reason, true)?;
    let path = svc.input.clone().expect("checked");
    let tty = std::io::stdin().is_terminal();
    let mut log = std::fs::File::open(&svc.log_path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", svc.log_path.display())))?;
    let end = log.metadata()?.len();
    let mut pos = crate::log_tail_start(&mut log, 0, end, 20)?;

    let how = if tty {
        "Ctrl+] detaches"
    } else {
        "EOF detaches"
    };
    eprintln!(
        "taba: attached to {} ({how}; the service keeps running)",
        svc.id()
    );
    let raw = RawMode(
        tty && ratatui::crossterm::terminal::enable_raw_mode()
            .map_err(|e| TabaError::state(format!("cannot set up the terminal: {e}")))
            .is_ok(),
    );

    let mut stdout = std::io::stdout().lock();
    let mut chunk = Vec::new();
    // After stdin ends (a pipe), the log is shown a little longer so the
    // service's answer to the last input is not cut off.
    let mut linger: Option<std::time::Instant> = None;
    let outcome = loop {
        let running = svc.is_running();
        log.seek(SeekFrom::Start(pos))?;
        chunk.clear();
        log.read_to_end(&mut chunk)?;
        if !chunk.is_empty() {
            pos += chunk.len() as u64;
            let out = if raw.0 { crlf(&chunk) } else { chunk.clone() };
            if stdout.write_all(&out).and_then(|_| stdout.flush()).is_err() {
                break "closed";
            }
        } else if !running {
            break "exited";
        }
        if linger.is_some_and(|t| t.elapsed() >= Duration::from_millis(500)) {
            break "detached";
        }
        if linger.is_some() {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // Waiting for input also paces the loop. No reader thread is left
        // behind, so `taba ui` gets every key back after a detach.
        match read_stdin(Duration::from_millis(50))? {
            None => {}
            Some(bytes) if bytes.is_empty() => {
                linger = Some(std::time::Instant::now());
            }
            Some(bytes) if raw.0 => {
                let (send, stop) = match bytes.iter().position(|&b| b == DETACH) {
                    Some(i) => (&bytes[..i], true),
                    None => (&bytes[..], false),
                };
                if !send.is_empty() {
                    write(&svc, &path, send)?;
                }
                if stop {
                    break "detached";
                }
            }
            Some(bytes) => write(&svc, &path, &bytes)?,
        }
    };
    drop(raw);
    match outcome {
        "exited" => eprintln!("\ntaba: {} exited", svc.id()),
        "detached" => eprintln!("\ntaba: detached from {}; it keeps running", svc.id()),
        _ => {}
    }
    Ok(())
}

/// Waits up to `timeout` for stdin: `None` when nothing arrived, an empty
/// buffer at EOF, else the bytes read.
#[cfg(unix)]
fn read_stdin(timeout: Duration) -> Result<Option<Vec<u8>>> {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let n = unsafe { libc::poll(&mut fds, 1, ms) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(TabaError::state(format!("cannot read stdin: {e}")));
    }
    if n == 0 {
        return Ok(None);
    }
    if fds.revents & libc::POLLNVAL != 0 {
        return Ok(Some(Vec::new()));
    }
    let mut buf = vec![0u8; 4096];
    let got = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if got < 0 {
        let e = std::io::Error::last_os_error();
        if matches!(
            e.kind(),
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
        ) {
            return Ok(None);
        }
        return Err(TabaError::state(format!("cannot read stdin: {e}")));
    }
    buf.truncate(got as usize);
    Ok(Some(buf))
}

#[cfg(windows)]
fn read_stdin(_timeout: Duration) -> Result<Option<Vec<u8>>> {
    Err(unsupported("taba attach"))
}

/// `\n` → `\r\n` for a terminal in raw mode.
fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_bytes("r").unwrap(), b"r");
        assert_eq!(key_bytes("R").unwrap(), b"R");
        assert_eq!(key_bytes("enter").unwrap(), b"\n");
        assert_eq!(key_bytes("ctrl-c").unwrap(), vec![3]);
        assert_eq!(key_bytes("Ctrl+D").unwrap(), vec![4]);
        assert_eq!(key_bytes("esc").unwrap(), vec![0x1b]);
        assert!(key_bytes("rr").is_err());
        assert!(key_bytes("").is_err());
    }

    #[test]
    fn crlf_adds_carriage_returns_once() {
        assert_eq!(crlf(b"a\nb\r\nc"), b"a\r\nb\r\nc");
    }
}
