//! `taba ps --watch --json`: a stream of `ps --json` snapshots, one JSON
//! document per line (NDJSON).
//!
//! The first line is printed at once; after that a new full snapshot is
//! printed only when the `services` rows changed. State is re-read every
//! interval (default 1 s, minimum 250 ms). The stream ends with exit 0 on
//! SIGINT / Ctrl+C (Windows: also Ctrl+Break) and when stdout is closed.
//! Watching never changes state: unlike `ps`, it does not renew the
//! caller's lease.

use crate::error::{Next, Result, TabaError, emit};
use crate::scope::check_ident;
use crate::state::Store;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Parses `--interval`: seconds (`2`, `0.5`), or a number with `ms` or `s`.
pub fn parse_interval(s: &str) -> std::result::Result<Duration, String> {
    let t = s.trim();
    let (num, divisor) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1000.0)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1.0)
    } else {
        (t, 1.0)
    };
    let bad = || format!("invalid interval {s:?}: use SECS, or a number with ms or s (500ms, 2s)");
    let v: f64 = num.trim().parse().map_err(|_| bad())?;
    let d = Duration::try_from_secs_f64(v / divisor).map_err(|_| bad())?;
    if d < MIN_INTERVAL {
        return Err(format!("interval {s:?} is below the minimum of 250ms"));
    }
    Ok(d)
}

/// Runs `taba ps --watch`. Returns the exit code.
pub fn run(json: bool, scope: Option<String>, running: bool, interval: Option<Duration>) -> u8 {
    if !json {
        let err =
            TabaError::usage("--watch needs --json: it prints one JSON snapshot per line (NDJSON)")
                .next(Next::run(
                    "stream the services as JSON",
                    "taba ps --watch --json",
                ));
        return emit(false, Err(err));
    }
    let interval = interval.unwrap_or(DEFAULT_INTERVAL);
    let store = match open(scope.as_deref()) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    install_interrupt_handler();
    let mut last: Option<Value> = None;
    loop {
        let services = match crate::ps_snapshot(&store, scope.as_deref(), running) {
            Ok(snap) => Value::Array(snap.rows),
            Err(e) => return fail(&e),
        };
        if last.as_ref() != Some(&services) {
            let line = json!({ "ok": true, "services": &services, "warnings": [] });
            if write_line(&line).is_err() {
                return 0; // the consumer went away
            }
            last = Some(services);
        }
        if wait_or_stop(interval) {
            return 0;
        }
    }
}

fn open(scope: Option<&str>) -> Result<Store> {
    if let Some(s) = scope {
        check_ident("scope", s)?;
    }
    Ok(Store::open()?)
}

/// Prints the error as one compact JSON line and returns its exit code.
fn fail(e: &TabaError) -> u8 {
    let _ = write_line(&json!({ "ok": false, "error": e.to_json(), "warnings": [] }));
    e.exit_code()
}

fn write_line(v: &Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, v)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Sleeps for `interval`, waking early to stop. True when the stream should
/// end: an interrupt arrived or stdout was closed.
fn wait_or_stop(interval: Duration) -> bool {
    let end = Instant::now() + interval;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) || stdout_closed() {
            return true;
        }
        let now = Instant::now();
        if now >= end {
            return false;
        }
        thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_interrupt_handler() {
    extern "C" fn on_sigint(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
    let handler: extern "C" fn(libc::c_int) = on_sigint;
    // SAFETY: the handler only stores to an atomic, which is
    // async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

#[cfg(windows)]
fn install_interrupt_handler() {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }
    // SAFETY: registers a handler that only stores to an atomic.
    unsafe {
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

/// Whether stdout is a pipe (or socket) whose reader has gone away, so an
/// idle stream ends within a tick instead of at its next write. Terminals
/// and files are never "closed" here; a failed write still ends the stream.
#[cfg(unix)]
fn stdout_closed() -> bool {
    // SAFETY: fstat on fd 1 with a pointer to a local value.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(1, &mut st) != 0 {
            return false;
        }
        let kind = st.st_mode & libc::S_IFMT;
        if kind != libc::S_IFIFO && kind != libc::S_IFSOCK {
            return false;
        }
    }
    reader_gone()
}

/// Linux: `poll` reports `POLLERR` (pipe) or `POLLHUP` (socket) on the
/// write end once the reader is gone.
#[cfg(target_os = "linux")]
fn reader_gone() -> bool {
    let mut fd = libc::pollfd {
        fd: 1,
        events: 0,
        revents: 0,
    };
    // SAFETY: poll on one valid, local pollfd.
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLERR | libc::POLLHUP) != 0
}

/// macOS: `poll` does not report a vanished pipe reader, but a kqueue
/// `EVFILT_WRITE` filter does, with `EV_EOF`.
#[cfg(target_os = "macos")]
fn reader_gone() -> bool {
    // SAFETY: a private kqueue, one change and one event in local memory,
    // a zero timeout, and the queue closed again.
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return false;
        }
        let mut change: libc::kevent = std::mem::zeroed();
        change.ident = 1;
        change.filter = libc::EVFILT_WRITE;
        change.flags = libc::EV_ADD;
        let mut event: libc::kevent = std::mem::zeroed();
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let n = libc::kevent(kq, &change, 1, &mut event, 1, &zero);
        libc::close(kq);
        n > 0 && event.flags & libc::EV_EOF != 0
    }
}

/// Windows: the pipe's state turns to "closing" once the reader's end is
/// closed (`NtQueryInformationFile`, `FilePipeLocalInformation`); querying
/// it writes nothing to the pipe.
#[cfg(windows)]
fn stdout_closed() -> bool {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_PIPE_CLOSING_STATE, FILE_PIPE_LOCAL_INFORMATION, FilePipeLocalInformation,
        NtQueryInformationFile,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    // SAFETY: queries the process's own stdout handle into local values.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE || GetFileType(h) != FILE_TYPE_PIPE {
            return false;
        }
        let mut iosb: IO_STATUS_BLOCK = std::mem::zeroed();
        let mut info: FILE_PIPE_LOCAL_INFORMATION = std::mem::zeroed();
        let status = NtQueryInformationFile(
            h,
            &mut iosb,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_PIPE_LOCAL_INFORMATION>() as u32,
            FilePipeLocalInformation,
        );
        status >= 0 && info.NamedPipeState == FILE_PIPE_CLOSING_STATE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(parse_interval("1"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_interval("0.25"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_interval("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_interval("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse_interval(" 1.5s "), Ok(Duration::from_millis(1500)));
        for bad in ["", "ms", "abc", "-1", "NaN", "inf", "1e400", "1m"] {
            assert!(parse_interval(bad).is_err(), "{bad}");
        }
        for low in ["0", "249ms", "0.2", "0.1s"] {
            let e = parse_interval(low).unwrap_err();
            assert!(e.contains("minimum"), "{low}: {e}");
        }
    }
}
