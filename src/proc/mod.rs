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
pub use os::{all_listeners, argv, cwd, listeners};
#[cfg(unix)]
pub use unix::{
    GRACEFUL_ACTION, KILL_ACTION, KILL_SIGNAL, group_is_ours, in_group, info, pgid,
    resolve_program, spawn_detached, stop_group,
};

#[cfg(windows)]
pub use windows::{
    GRACEFUL_ACTION, KILL_ACTION, KILL_SIGNAL, all_listeners, argv, ctrl_break_helper, cwd,
    group_is_ours, in_group, info, listeners, pgid, resolve_program, spawn_detached, stop_group,
};

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
compile_error!("taba supports macOS, Linux and Windows");

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
