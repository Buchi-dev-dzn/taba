//! Process groups, sessions and signals, shared by macOS and Linux.

use super::os::{RawInfo, all_pids, raw_info};
use super::{ProcInfo, SpawnError, Spawned, StopResult, wait_for};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

/// What `stop` uses after the timeout (for messages).
pub const KILL_SIGNAL: &str = "SIGKILL";

/// `platform_action` values in `stop` results.
pub const GRACEFUL_ACTION: &str = "sigterm";
pub const KILL_ACTION: &str = "sigkill";

/// Process group of `pid`.
pub fn pgid(pid: i32) -> Option<i32> {
    raw_info(pid).map(|r| r.pgid)
}

/// Reads kernel information about `pid`. Returns `None` if no such process
/// is visible to the current user.
pub fn info(pid: i32) -> Option<ProcInfo> {
    raw_info(pid).map(|r| ProcInfo {
        start_time: r.start_time,
        zombie: r.zombie,
    })
}

/// Live (non-zombie) members of process group `pgid`.
fn group_members(pgid: i32) -> Vec<RawInfo> {
    all_pids()
        .into_iter()
        .filter_map(raw_info)
        .filter(|p| p.pgid == pgid && !p.zombie)
        .collect()
}

/// Whether the process group that taba recorded (leader `pid` == `pgid`,
/// started at `start_time`) is verifiably still the same group.
///
/// * If the leader is alive, its start time must match exactly.
/// * If the leader has exited but other members remain, the pgid cannot have
///   been reused (the kernel does not hand out a pid that is still in use as
///   a process group id), so the group is ours as long as every member
///   started no earlier than the recorded start time.
pub fn group_is_ours(pid: i32, pgid: i32, start_time: u64) -> bool {
    if pid <= 0 || pgid <= 1 {
        return false;
    }
    match raw_info(pid) {
        Some(p) if !p.zombie => p.pid == pid && p.start_time == start_time && p.pgid == pgid,
        _ => {
            let members = group_members(pgid);
            !members.is_empty() && members.iter().all(|m| m.start_time >= start_time)
        }
    }
}

/// Whether process `pid` belongs to the recorded service group.
pub fn in_group(pid: i32, _leader: i32, pgid: i32, _start_time: u64) -> bool {
    matches!(raw_info(pid), Some(p) if p.pgid == pgid)
}

fn group_exists(pgid: i32) -> bool {
    !group_members(pgid).is_empty()
}

fn signal_group(pgid: i32, sig: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(-pgid, sig) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

/// Sends SIGTERM to the group and waits up to `wait` for it to exit, without
/// escalating. Returns whether the group is gone.
pub fn request_stop(_pid: i32, pgid: i32, _start_time: u64, wait: Duration) -> io::Result<bool> {
    signal_group(pgid, libc::SIGTERM)?;
    Ok(wait_for(wait, Duration::from_millis(50), || {
        !group_exists(pgid)
    }))
}

/// Sends SIGTERM to the group, waits up to `timeout`, then SIGKILL.
pub fn stop_group(
    _pid: i32,
    pgid: i32,
    _start_time: u64,
    timeout: Duration,
) -> io::Result<StopResult> {
    signal_group(pgid, libc::SIGTERM)?;
    if wait_for(timeout, Duration::from_millis(50), || !group_exists(pgid)) {
        return Ok(StopResult::Terminated);
    }
    signal_group(pgid, libc::SIGKILL)?;
    wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        !group_exists(pgid)
    });
    Ok(StopResult::Killed)
}

/// The program to execute for `name`; `Command` already searches `PATH`.
pub fn resolve_program(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(name)
}

/// Spawns `cmd` detached: in a new session, so in a new process group with
/// pgid == pid and without a controlling terminal, so closing the launching
/// terminal does not SIGHUP it. The child is not waited on: once taba exits
/// it is re-parented to launchd / init, which reaps it.
pub fn spawn_detached(mut cmd: Command) -> Result<Spawned, SpawnError> {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(SpawnError::Start)?;
    let pid = child.id() as i32;
    let Some(info) = info(pid) else {
        // The direct child has not been reaped and its PID cannot have
        // been reused. Do not leave it running without an identity record.
        let _ = child.kill();
        let _ = child.wait();
        return Err(SpawnError::Vanished);
    };
    Ok(Spawned {
        pid,
        pgid: pid,
        start_time: info.start_time,
    })
}
