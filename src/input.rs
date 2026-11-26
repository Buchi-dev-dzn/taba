//! Interactive services (#68): `run --interactive`, `send`, `attach`,
//! `reload`, `signal`, and typing into a service from its log view.
//!
//! # How input reaches the service (Unix)
//!
//! Dev servers such as `flutter run` and `vite` only read their keys when
//! stdin is a terminal, so an interactive service gets a pseudo-terminal
//! (PTY) as its stdin. Only stdin: stdout and stderr still go to the log
//! file, so the log stays the same as for any other service.
//!
//! ```text
//!  taba send / attach ──write──▶ inputs/<scope>/<name>.fifo
//!                                      │ (read by)
//!                              holder: `taba __hold`
//!                                      │ write
//!                                 PTY master ══▶ PTY slave = service stdin
//! ```
//!
//! `run` creates the FIFO and the PTY (echo off, so typed keys do not show
//! up anywhere), starts the service with the PTY slave as stdin, then
//! starts a small holder process (`taba __hold`, one per interactive
//! service, in its own session). The holder keeps the PTY master open (a
//! closed master would end the service's stdin), copies the FIFO into it,
//! drains whatever the terminal side outputs, and exits once the service's
//! process group is gone. The holder also keeps the FIFO open for reading,
//! so writers never block on a missing reader and the service never sees
//! EOF between two `send`s.
//!
//! The PTY is not the service's controlling terminal: the service stays in
//! its own session as before, so a crashing holder does not SIGHUP it, and
//! Ctrl+C typed into it is not turned into SIGINT (use `taba stop`).
//!
//! # Who may type
//!
//! Viewing a log is free. Writing to a service's stdin, or signalling it,
//! mutates someone else's process, so the same rules as `stop` apply: the
//! owning scope (under its lease) or the holder of an exclusive borrow;
//! anyone else needs `--force --reason`, which is recorded in events.jsonl.
//! While another scope holds an exclusive borrow, the owner is refused with
//! `E_LOCKED`.
//!
//! Windows (ConPTY) is not supported yet (#79): `run --interactive` and
//! these commands fail there with `E_UNSUPPORTED`.

#![cfg_attr(windows, allow(dead_code, unused_imports, unused_variables))]

use crate::error::{Next, Output, Result, TabaError};
use crate::scope::{self, Caller};
use crate::state::{Input, Service, Store};
use crate::{borrows, config, lease};
use serde_json::json;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a write waits for a service that does not read its stdin.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The byte that leaves input mode (Ctrl+]).
const DETACH: u8 = 0x1d;
/// Terminal size the service sees (its output is a file, not this size).
const COLS: u16 = 120;
const ROWS: u16 = 40;

pub fn unsupported(what: &str) -> TabaError {
    TabaError::new(
        "E_UNSUPPORTED",
        format!("{what} is not supported on this platform yet (macOS and Linux only)"),
    )
    .details(json!({ "feature": what, "platform": std::env::consts::OS }))
    .next(Next::just(
        "start it without --interactive; Windows (ConPTY) support is tracked in \
         https://github.com/Buchi-dev-dzn/taba/issues/79",
    ))
}

// ------------------------------------------------------------ detection

/// Dev servers taba knows how to drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `flutter run`: `r` hot reload, `R` hot restart; also SIGUSR1/SIGUSR2.
    Flutter,
    /// `vite` (dev or preview): `r` + Enter restarts the server.
    Vite,
    Other,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Flutter => "flutter",
            Kind::Vite => "vite",
            Kind::Other => "other",
        }
    }
}

/// What `command` (run in `cwd`) is: `flutter run`, `vite`, or something
/// else. Looks through wrappers (`fvm flutter run`, `npx vite`,
/// `sh -c '...'`) and one level of package.json scripts (`npm run dev`,
/// `pnpm dev`, `yarn dev`, `bun run dev`).
pub fn detect(command: &[String], cwd: &Path) -> Kind {
    detect_words(&split(command), Some(cwd))
}

fn split(command: &[String]) -> Vec<String> {
    command
        .iter()
        .flat_map(|w| w.split_whitespace())
        .map(|w| w.trim_matches(|c| c == '"' || c == '\'').to_string())
        .collect()
}

/// The program name of `word`: lower case, without directory and without
/// `.exe`/`.bat`/`.cmd`/`.js`/`.mjs`.
fn base(word: &str) -> String {
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let file = file.to_ascii_lowercase();
    for ext in [".exe", ".bat", ".cmd", ".js", ".mjs"] {
        if let Some(stem) = file.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    file
}

fn first_arg(words: &[String]) -> Option<&str> {
    words
        .iter()
        .map(String::as_str)
        .find(|w| !w.starts_with('-'))
}

fn detect_words(words: &[String], cwd: Option<&Path>) -> Kind {
    for (i, w) in words.iter().enumerate() {
        let rest = &words[i + 1..];
        match base(w).as_str() {
            "flutter" if first_arg(rest) == Some("run") => return Kind::Flutter,
            "vite" if !matches!(first_arg(rest), Some("build" | "optimize")) => {
                return Kind::Vite;
            }
            pm @ ("npm" | "pnpm" | "yarn" | "bun") => {
                if let Some(cwd) = cwd
                    && let Some(script) = script_name(pm, rest)
                    && let Some(body) = package_script(cwd, script)
                {
                    let kind = detect_words(&split(&[body]), None);
                    if kind != Kind::Other {
                        return kind;
                    }
                }
            }
            _ => {}
        }
    }
    Kind::Other
}

/// The package.json script a package-manager call runs, if any.
fn script_name<'a>(pm: &str, rest: &'a [String]) -> Option<&'a str> {
    let mut args = rest
        .iter()
        .map(String::as_str)
        .filter(|w| !w.starts_with('-'));
    match (pm, args.next()?) {
        (_, "run" | "run-script") => args.next(),
        ("npm", "start") => Some("start"),
        ("npm", _) => None,
        (_, script) => Some(script),
    }
}

fn package_script(cwd: &Path, script: &str) -> Option<String> {
    let text = std::fs::read_to_string(cwd.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&text).ok()?;
    pkg["scripts"][script].as_str().map(str::to_string)
}

// ------------------------------------------------------------ setup (run)

/// The terminal of an interactive service being started.
pub struct Terminal {
    pub fifo: PathBuf,
    /// The PTY slave: the child's stdin.
    pub stdin: File,
    master: File,
    /// The FIFO, opened for reading (and writing, so it never reaches EOF).
    reader: File,
}

/// Creates the input FIFO of `scope/name` (replacing a stale one) and a PTY
/// with echo off.
#[cfg(unix)]
pub fn create(store: &Store, scope: &str, name: &str) -> Result<Terminal> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let path = store.input_path(scope, name);
    let os = |what: &str| {
        let e = std::io::Error::last_os_error();
        TabaError::state(format!("cannot {what} for {scope}/{name}: {e}"))
    };
    crate::state::create_private_dir_all(path.parent().expect("input has parent"))?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| TabaError::state("input path contains a NUL byte"))?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(os("create the input FIFO"));
    }
    // Read-write: opening does not wait for a writer, and the holder never
    // reads EOF while nobody is sending.
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", path.display())))?;

    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(os("open a pseudo-terminal"));
    }
    let master = unsafe { File::from_raw_fd(fd) };
    let m = master.as_raw_fd();
    unsafe {
        libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    if unsafe { libc::grantpt(m) } != 0 || unsafe { libc::unlockpt(m) } != 0 {
        return Err(os("set up the pseudo-terminal"));
    }
    // ptsname is not thread-safe; `run` has no other threads here.
    let name_ptr = unsafe { libc::ptsname(m) };
    if name_ptr.is_null() {
        return Err(os("name the pseudo-terminal"));
    }
    let slave_path = unsafe { std::ffi::CStr::from_ptr(name_ptr) }
        .to_string_lossy()
        .into_owned();
    let stdin = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&slave_path)
        .map_err(|e| TabaError::state(format!("cannot open {slave_path}: {e}")))?;
    let s = stdin.as_raw_fd();
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(s, &mut t) == 0 {
            t.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
            libc::tcsetattr(s, libc::TCSANOW, &t);
        }
        let ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(m, libc::TIOCSWINSZ, &ws);
    }
    Ok(Terminal {
        fifo: path,
        stdin,
        master,
        reader,
    })
}

#[cfg(windows)]
pub fn create(_store: &Store, _scope: &str, _name: &str) -> Result<Terminal> {
    Err(unsupported("run --interactive"))
}

/// Starts the holder of the service `spawned` (see the module docs);
/// `stderr` is the service's log.
#[cfg(unix)]
pub fn spawn_holder(t: Terminal, spawned: &crate::proc::Spawned, stderr: File) -> Result<Input> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()
        .map_err(|e| TabaError::state(format!("cannot find the taba executable: {e}")))?;
    let (m, r) = (t.master.as_raw_fd(), t.reader.as_raw_fd());
    let mut cmd = Command::new(exe);
    cmd.arg("__hold")
        .args([
            m.to_string(),
            r.to_string(),
            spawned.pid.to_string(),
            spawned.pgid.to_string(),
            spawned.start_time.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    unsafe {
        // Only the holder inherits the two descriptors.
        cmd.pre_exec(move || {
            for fd in [m, r] {
                if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let holder = crate::proc::spawn_detached(cmd).map_err(|e| {
        TabaError::new(
            "E_START_FAILED",
            format!("cannot start the input holder: {e:?}"),
        )
    })?;
    Ok(Input {
        fifo: t.fifo,
        holder_pid: holder.pid,
        holder_start_time: holder.start_time,
    })
}

#[cfg(windows)]
pub fn spawn_holder(_t: Terminal, _spawned: &crate::proc::Spawned, _stderr: File) -> Result<Input> {
    Err(unsupported("run --interactive"))
}

/// `taba __hold MASTER FIFO PID PGID START_TIME`: the holder process. Copies
/// the FIFO into the PTY master and drains the master until the service's
/// process group is gone.
#[cfg(unix)]
pub fn hold(master: i32, fifo: i32, pid: i32, pgid: i32, start_time: u64) -> u8 {
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::FromRawFd;
    for fd in [master, fifo] {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            eprintln!("taba __hold: descriptor {fd} is not open");
            return 1;
        }
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let mut drain = unsafe { File::from_raw_fd(master) };
    let mut fifo = unsafe { File::from_raw_fd(fifo) };
    let Ok(mut to_master) = drain.try_clone() else {
        eprintln!("taba __hold: cannot duplicate the terminal");
        return 1;
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match fifo.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if to_master.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    // With echo off this is usually silent; draining keeps a program that
    // writes to its terminal from blocking.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match drain.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    while crate::proc::group_is_ours(pid, pgid, start_time) {
        std::thread::sleep(Duration::from_millis(200));
    }
    0
}

// ------------------------------------------------------------ writing

/// Writes `bytes` to the service's input without blocking forever on a
/// service that does not read.
#[cfg(unix)]
pub fn write(svc: &Service, input: &Input, bytes: &[u8]) -> Result<()> {
    use std::io::{ErrorKind, Write};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    let path = &input.fifo;
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
            // No reader: the holder is gone.
            Some(libc::ENXIO) if svc.is_running() => holder_gone(svc, input),
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
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                return Err(holder_gone(svc, input));
            }
            Err(e) => return Err(TabaError::state(format!("cannot write input: {e}"))),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn write(_svc: &Service, _input: &Input, _bytes: &[u8]) -> Result<()> {
    Err(unsupported("service input"))
}

fn no_input(svc: &Service) -> TabaError {
    TabaError::new(
        "E_NO_INPUT",
        format!(
            "{} was not started with --interactive, so it takes no input",
            svc.id()
        ),
    )
    .details(json!({ "service": svc.id() }))
    .next(Next::run(
        "restart it with --interactive (stop it first)",
        format!("taba stop {}", svc.id()),
    ))
}

fn holder_gone(svc: &Service, input: &Input) -> TabaError {
    TabaError::new(
        "E_NO_INPUT",
        format!(
            "{}: its input holder (pid {}) is gone, so it takes no more input",
            svc.id(),
            input.holder_pid
        ),
    )
    .details(json!({ "service": svc.id(), "holder_pid": input.holder_pid }))
    .next(Next::run(
        "restart it (stop it first)",
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

// ------------------------------------------------------------ send

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
        // A terminal's Enter key sends CR; the PTY turns it into "\n" for
        // line-based readers (ICRNL) and raw-mode programs expect it.
        "enter" | "return" => Some(b"\r".to_vec()),
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
    if cfg!(windows) {
        return Err(unsupported("taba send"));
    }
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    let svc = authorise(&store, &caller, &a.target, "send", reason.as_deref(), true)?;
    let input = svc.input.clone().expect("checked");
    write(&svc, &input, &bytes)?;
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

// ------------------------------------------------------------ signal, reload

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

pub struct ReloadArgs {
    pub target: String,
    pub restart: bool,
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
}

/// `taba reload`: the most reliable reload for the kind of dev server.
/// `flutter run`: SIGUSR1 (hot reload) or SIGUSR2 (`--restart`, hot
/// restart), which needs no stdin. Vite: `r` + Enter on stdin (restarts the
/// dev server; open pages reload), so it needs `--interactive`.
pub fn cmd_reload(a: ReloadArgs) -> Result<Output> {
    let reason = crate::check_force(a.force, a.reason)?;
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    let (scope, name) = crate::resolve_target(&a.target, &caller)?;
    let peek = store
        .get(&scope, &name)?
        .ok_or_else(|| crate::not_found(&scope, &name))?;
    let kind = detect(peek.template(), &peek.cwd);
    if kind == Kind::Other {
        return Err(TabaError::new(
            "E_NO_RELOAD",
            format!(
                "{} is not a dev server taba knows how to reload (flutter run, vite)",
                peek.id()
            ),
        )
        .details(json!({ "service": peek.id(), "command": peek.template() }))
        .next(Next::run(
            "send its reload key yourself",
            format!("taba send {} --key r", peek.id()),
        )));
    }
    if cfg!(windows) {
        return Err(unsupported("taba reload"));
    }
    let id = peek.id();
    let svc = authorise(
        &store,
        &caller,
        &id,
        "reload",
        reason.as_deref(),
        kind == Kind::Vite,
    )?;
    let (method, text) = match kind {
        Kind::Flutter => {
            let sig = if a.restart {
                Signal::Usr2
            } else {
                Signal::Usr1
            };
            send_signal(&svc, sig)?;
            let what = if a.restart {
                "hot restart"
            } else {
                "hot reload"
            };
            (
                format!("SIG{}", sig.name()),
                format!(
                    "{id}: {what} requested (SIG{} to pid {})",
                    sig.name(),
                    svc.pid
                ),
            )
        }
        _ => {
            let input = svc.input.clone().expect("checked");
            write(&svc, &input, b"r\n")?;
            (
                "stdin".to_string(),
                format!("{id}: sent r (Vite restarts the dev server; open pages reload)"),
            )
        }
    };
    let mut body = json!({
        "service": id,
        "kind": kind.name(),
        "method": method,
        "restart": a.restart,
        "instance": svc.instance(),
    });
    if reason.is_some() {
        body["forced"] = json!(true);
    }
    Ok(Output::new(body, text))
}

// ------------------------------------------------------------ log view

pub struct AttachArgs {
    pub target: String,
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
}

/// `taba attach`: the log view in input mode (on a terminal), or forward
/// stdin until EOF (from a pipe).
pub fn cmd_attach(a: AttachArgs) -> Result<Option<Output>> {
    use std::io::IsTerminal;
    let reason = crate::check_force(a.force, a.reason)?;
    if cfg!(windows) {
        return Err(unsupported("taba attach"));
    }
    let caller = scope::resolve(a.scope, a.agent)?;
    let store = Store::open()?;
    let svc = authorise(
        &store,
        &caller,
        &a.target,
        "attach",
        reason.as_deref(),
        true,
    )?;
    let (log, pos) = open_tail(&svc)?;
    if std::io::stdin().is_terminal() {
        view(
            &store,
            &caller,
            &svc,
            log,
            pos,
            true,
            false,
            reason.as_deref(),
        )
    } else {
        forward_pipe(&svc, log, pos)
    }?;
    Ok(None)
}

/// `taba ui`: `i` on an interactive service. Input mode right away; Ctrl+]
/// returns to the TUI.
pub(crate) fn attach_from_ui(store: &Store, caller: &Caller, target: &str) -> Result<()> {
    let svc = authorise(store, caller, target, "attach", None, true)?;
    let (log, pos) = open_tail(&svc)?;
    view(store, caller, &svc, log, pos, true, true, None)
}

/// `taba logs -f` on a terminal, for an interactive service: read-only
/// until `i`. `log` is positioned after what was already printed.
pub(crate) fn follow(store: &Store, caller: &Caller, svc: &Service, log: File) -> Result<()> {
    use std::io::Seek;
    let mut log = log;
    let pos = log.stream_position()?;
    view(store, caller, svc, log, pos, false, false, None)
}

fn open_tail(svc: &Service) -> Result<(File, u64)> {
    let mut log = File::open(&svc.log_path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", svc.log_path.display())))?;
    let end = log.metadata()?.len();
    let pos = crate::log_tail_start(&mut log, 0, end, 20)?;
    Ok((log, pos))
}

/// Restores the terminal mode when the view ends, also on an error.
struct RawMode;
impl RawMode {
    fn enable() -> Result<RawMode> {
        ratatui::crossterm::terminal::enable_raw_mode()
            .map_err(|e| TabaError::state(format!("cannot set up the terminal: {e}")))?;
        Ok(RawMode)
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

/// One status line between log lines (raw mode: explicit `\r\n`).
fn status(text: &str) {
    eprint!("\r\n\x1b[7m taba \x1b[0m {text}\r\n");
}

fn banner(id: &str, typing: bool, leave_to_ui: bool) {
    if typing {
        let back = if leave_to_ui {
            "Ctrl+] returns to taba ui"
        } else {
            "Ctrl+] stops typing"
        };
        status(&format!("INPUT MODE: keys go to {id} · {back}"));
    } else {
        status(&format!(
            "read-only log of {id} · i types into it · q quits (the service keeps running)"
        ));
    }
}

/// The log view on a terminal: follows the log and, in input mode, sends
/// each key to the service. `i` enters input mode (if the caller may type),
/// Ctrl+] leaves it (or the view, with `leave_on_detach`), `q` / Ctrl+C
/// quits. The service keeps running.
#[allow(clippy::too_many_arguments)]
fn view(
    store: &Store,
    caller: &Caller,
    svc: &Service,
    mut log: File,
    mut pos: u64,
    mut typing: bool,
    leave_on_detach: bool,
    force_reason: Option<&str>,
) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let id = svc.id();
    let raw = RawMode::enable()?;
    banner(&id, typing, leave_on_detach);
    let mut stdout = std::io::stdout().lock();
    let mut chunk = Vec::new();
    loop {
        let running = svc.is_running();
        log.seek(SeekFrom::Start(pos))?;
        chunk.clear();
        log.read_to_end(&mut chunk)?;
        if !chunk.is_empty() {
            pos += chunk.len() as u64;
            if stdout
                .write_all(&crlf(&chunk))
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        } else if !running {
            status(&format!("{id} exited"));
            break;
        }
        let Some(bytes) = read_stdin(Duration::from_millis(50))? else {
            continue;
        };
        if bytes.is_empty() {
            break; // the terminal went away
        }
        if typing {
            let (send, detach) = match bytes.iter().position(|&b| b == DETACH) {
                Some(i) => (&bytes[..i], true),
                None => (&bytes[..], false),
            };
            if !send.is_empty() {
                let input = svc.input.as_ref().expect("interactive");
                if let Err(e) = write(svc, input, send) {
                    status(&e.message);
                }
            }
            if detach {
                if leave_on_detach {
                    break;
                }
                typing = false;
                banner(&id, typing, leave_on_detach);
            }
            continue;
        }
        for &b in &bytes {
            match b {
                b'i' => {
                    match authorise(store, caller, &id, "attach", force_reason, true) {
                        Ok(_) => {
                            typing = true;
                            banner(&id, typing, leave_on_detach);
                        }
                        Err(e) => status(&format!("cannot type into {id}: {}", e.message)),
                    }
                    break;
                }
                b'q' | 0x03 | 0x04 => {
                    drop(raw);
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    drop(raw);
    Ok(())
}

/// `taba attach` from a pipe: forwards stdin until EOF, then shows the log
/// a little longer so the answer to the last input is not cut off.
fn forward_pipe(svc: &Service, mut log: File, mut pos: u64) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let input = svc.input.clone().expect("interactive");
    eprintln!(
        "taba: attached to {} (EOF detaches; the service keeps running)",
        svc.id()
    );
    let mut stdout = std::io::stdout().lock();
    let mut chunk = Vec::new();
    let mut linger: Option<std::time::Instant> = None;
    let outcome = loop {
        let running = svc.is_running();
        log.seek(SeekFrom::Start(pos))?;
        chunk.clear();
        log.read_to_end(&mut chunk)?;
        if !chunk.is_empty() {
            pos += chunk.len() as u64;
            if stdout
                .write_all(&chunk)
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break "closed";
            }
        } else if !running {
            break "exited";
        }
        if let Some(t) = linger {
            if t.elapsed() >= Duration::from_millis(500) {
                break "detached";
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match read_stdin(Duration::from_millis(50))? {
            None => {}
            Some(bytes) if bytes.is_empty() => linger = Some(std::time::Instant::now()),
            Some(bytes) => write(svc, &input, &bytes)?,
        }
    };
    match outcome {
        "exited" => eprintln!("taba: {} exited", svc.id()),
        "detached" => eprintln!("taba: detached from {}; it keeps running", svc.id()),
        _ => {}
    }
    Ok(())
}

/// Waits up to `timeout` for stdin: `None` when nothing arrived, an empty
/// buffer at EOF, else the bytes read. Uses select(2), which (unlike poll)
/// works on terminals on macOS.
#[cfg(unix)]
fn read_stdin(timeout: Duration) -> Result<Option<Vec<u8>>> {
    let fd = libc::STDIN_FILENO;
    let mut tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let n = unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        )
    };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        // EBADF: stdin is closed.
        return Ok(Some(Vec::new()));
    }
    if n == 0 {
        return Ok(None);
    }
    let mut buf = vec![0u8; 4096];
    let got = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
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

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn keys() {
        assert_eq!(key_bytes("r").unwrap(), b"r");
        assert_eq!(key_bytes("R").unwrap(), b"R");
        assert_eq!(key_bytes("enter").unwrap(), b"\r");
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

    #[test]
    fn detects_flutter_run_and_vite() {
        let none = Path::new("/nonexistent");
        let d = |s: &str| detect(&words(s), none);
        assert_eq!(d("flutter run -d chrome"), Kind::Flutter);
        assert_eq!(d("fvm flutter run"), Kind::Flutter);
        assert_eq!(d("/opt/flutter/bin/flutter --verbose run"), Kind::Flutter);
        assert_eq!(d("flutter.bat run"), Kind::Flutter);
        assert_eq!(d("flutter build web"), Kind::Other);
        assert_eq!(d("flutter test"), Kind::Other);
        assert_eq!(d("vite"), Kind::Vite);
        assert_eq!(d("npx vite --port 5173"), Kind::Vite);
        assert_eq!(d("node node_modules/vite/bin/vite.js"), Kind::Vite);
        assert_eq!(d("vite preview"), Kind::Vite);
        assert_eq!(d("vite build"), Kind::Other);
        assert_eq!(d("python3 -m http.server"), Kind::Other);
        assert_eq!(d("node vite.config.ts"), Kind::Other);
        // A shell command line is split into words.
        assert_eq!(
            detect(
                &["sh".into(), "-c".into(), "cd app && flutter run".into()],
                none
            ),
            Kind::Flutter
        );
    }

    #[test]
    fn detects_vite_behind_package_scripts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite --host","build":"vite build","start":"vite","serve":"node server.js"}}"#,
        )
        .unwrap();
        let d = |s: &str| detect(&words(s), dir.path());
        assert_eq!(d("npm run dev"), Kind::Vite);
        assert_eq!(d("npm start"), Kind::Vite);
        assert_eq!(d("pnpm dev"), Kind::Vite);
        assert_eq!(d("yarn run dev"), Kind::Vite);
        assert_eq!(d("bun run dev"), Kind::Vite);
        assert_eq!(d("npm run build"), Kind::Other);
        assert_eq!(d("npm run serve"), Kind::Other);
        assert_eq!(d("npm install"), Kind::Other);
        assert_eq!(d("npm run missing"), Kind::Other);
    }
}
