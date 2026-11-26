//! Process inspection, detached spawning and stopping, per platform.
//!
//! The rest of taba sees one API, implemented per OS:
//!
//! * macOS (`macos.rs`) and Linux (`linux.rs`) read process facts from
//!   `proc_pidinfo` / `/proc`; both share `unix.rs` for process groups,
//!   `setsid` and signals.
//! * Windows (`windows.rs`) uses a Job Object per service in place of the
//!   process group, Ctrl+Break in place of SIGTERM and `TerminateJobObject`
//!   in place of SIGKILL.
//!
//! A service is identified by its leader `pid`, its `pgid` (== `pid` on
//! every platform) and the leader's `start_time`, so a reused pid is never
//! mistaken for the service.

use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(unix)]
pub use os::{all_listeners, all_pids, argv, cwd, listeners};
#[cfg(unix)]
pub use unix::{
    GRACEFUL_ACTION, KILL_ACTION, KILL_SIGNAL, group_is_ours, in_group, info, pgid, request_stop,
    resolve_program, spawn_detached, stop_group,
};

#[cfg(windows)]
pub use windows::{
    GRACEFUL_ACTION, KILL_ACTION, KILL_SIGNAL, all_listeners, all_pids, argv, ctrl_break_helper,
    cwd, group_is_ours, in_group, info, listeners, pgid, request_stop, resolve_program,
    spawn_detached, stop_group,
};

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
compile_error!("taba supports macOS, Linux and Windows");

/// The clock and the process operations that `taba stop` and cleanup use
/// on a service. [`System`] is the real implementation; unit tests pass a
/// fake so that timing and signal failures are deterministic and no real
/// process is signalled.
pub(crate) trait Ops {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64 {
        crate::util::now_secs()
    }
    /// Whether the service's process group is still the one taba started.
    fn is_running(&self, svc: &crate::state::Service) -> bool {
        svc.is_running()
    }
    /// [`stop_group`] for the service.
    fn stop_group(
        &self,
        svc: &crate::state::Service,
        timeout: Duration,
    ) -> std::io::Result<StopResult> {
        stop_group(svc.pid, svc.pgid, svc.start_time, timeout)
    }
    /// [`request_stop`] for the service.
    fn request_stop(&self, svc: &crate::state::Service, wait: Duration) -> std::io::Result<bool> {
        request_stop(svc.pid, svc.pgid, svc.start_time, wait)
    }
}

/// The real clock and processes.
pub(crate) struct System;
impl Ops for System {}

/// Snapshot of the fields taba needs about one process.
#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    /// Process start time in microseconds since the Unix epoch.
    pub start_time: u64,
    /// Exited but not yet reaped (Unix), or exited with handles still open
    /// (Windows).
    pub zombie: bool,
}

/// A process started by [`spawn_detached`].
#[derive(Debug, Clone, Copy)]
pub struct Spawned {
    pub pid: i32,
    pub pgid: i32,
    pub start_time: u64,
}

/// Why [`spawn_detached`] failed.
#[derive(Debug)]
pub enum SpawnError {
    /// The program could not be started.
    Start(std::io::Error),
    /// It started but was gone before taba could read its identity.
    #[cfg_attr(windows, allow(dead_code))]
    Vanished,
}

/// Outcome of stopping a service's processes.
#[derive(Debug, PartialEq, Eq)]
pub enum StopResult {
    /// Exited after the graceful request (SIGTERM / Ctrl+Break).
    Terminated,
    /// Still running at the timeout and killed (SIGKILL / TerminateJobObject).
    Killed,
}

/// A TCP socket in LISTEN state and the process holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub port: u16,
    /// Bound address: `127.0.0.1`, `::1`, or `*` for any address.
    pub address: String,
    pub pid: i32,
}

/// `*` for an unspecified address, else the address itself.
pub fn format_ip(ip: std::net::IpAddr) -> String {
    if ip.is_unspecified() {
        "*".to_string()
    } else {
        ip.to_string()
    }
}

/// Splits lsof's `127.0.0.1:4113`, `*:5173` or `[::1]:4113`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn split_host_port(s: &str) -> Option<(String, u16)> {
    let (host, port) = s.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => format_ip(ip),
        Err(_) => host.to_string(),
    };
    Some((host, port.parse().ok()?))
}

/// A listening TCP socket's owner: pid and short command name.
pub type Listener = (i32, String);

/// Whether process `pid` exists, is not a zombie, and started at
/// `start_time` (so the pid was not reused).
pub fn is_alive(pid: i32, start_time: u64) -> bool {
    matches!(info(pid), Some(p) if !p.zombie && p.start_time == start_time)
}

/// The current process's pid and start time.
pub fn self_identity() -> (i32, u64) {
    let pid = std::process::id() as i32;
    (pid, info(pid).map_or(0, |p| p.start_time))
}

/// Polls `done` every `step` until it returns true or `timeout` passes.
pub(crate) fn wait_for(timeout: Duration, step: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(step);
    }
}

/// A fake [`Ops`] for unit tests: a settable clock and a service that is
/// "running" until a stop succeeds. It never touches a real process.
#[cfg(test)]
pub(crate) mod fake {
    use super::{Ops, StopResult};
    use crate::state::Service;
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::time::Duration;

    pub(crate) struct FakeOps {
        pub now: Cell<u64>,
        pub running: Cell<bool>,
        /// `stop_group` and `request_stop` fail with this for the service
        /// named `fail_name` (any service when `None`).
        pub fail: Cell<Option<io::ErrorKind>>,
        pub fail_name: RefCell<Option<String>>,
        /// `stop_group` reports `Killed` instead of `Terminated`.
        pub kill: Cell<bool>,
        /// The service exits within `request_stop`'s wait.
        pub exits_on_request: Cell<bool>,
        /// Every call, as `stop NAME MS` or `request NAME MS`.
        pub calls: RefCell<Vec<String>>,
    }

    impl FakeOps {
        pub fn at(now: u64) -> FakeOps {
            FakeOps {
                now: Cell::new(now),
                running: Cell::new(true),
                fail: Cell::new(None),
                fail_name: RefCell::new(None),
                kill: Cell::new(false),
                exits_on_request: Cell::new(false),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow_mut().drain(..).collect()
        }

        fn failure(&self, svc: &Service) -> io::Result<()> {
            let applies = self
                .fail_name
                .borrow()
                .as_deref()
                .is_none_or(|n| n == svc.name);
            match self.fail.get() {
                Some(kind) if applies => Err(io::Error::new(kind, "fake signal failure")),
                _ => Ok(()),
            }
        }
    }

    impl Ops for FakeOps {
        fn now(&self) -> u64 {
            self.now.get()
        }
        fn is_running(&self, _svc: &Service) -> bool {
            self.running.get()
        }
        fn stop_group(&self, svc: &Service, timeout: Duration) -> io::Result<StopResult> {
            self.calls
                .borrow_mut()
                .push(format!("stop {} {}", svc.name, timeout.as_millis()));
            self.failure(svc)?;
            self.running.set(false);
            Ok(if self.kill.get() {
                StopResult::Killed
            } else {
                StopResult::Terminated
            })
        }
        fn request_stop(&self, svc: &Service, wait: Duration) -> io::Result<bool> {
            self.calls
                .borrow_mut()
                .push(format!("request {} {}", svc.name, wait.as_millis()));
            self.failure(svc)?;
            if self.exits_on_request.get() {
                self.running.set(false);
            }
            Ok(!self.running.get())
        }
    }
}
