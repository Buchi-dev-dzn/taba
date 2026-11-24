//! macOS process inspection and signalling helpers.

use std::io;
use std::mem;
use std::thread;
use std::time::{Duration, Instant};

/// Snapshot of the fields taba needs about one process.
#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    pub pid: i32,
    pub pgid: i32,
    /// Process start time in microseconds since the Unix epoch.
    pub start_time: u64,
    pub zombie: bool,
}

const SZOMB: u32 = 5;

/// Reads kernel information about `pid`. Returns `None` if no such process
/// is visible to the current user.
pub fn info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    let mut bsd: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut bsd as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret != size {
        return None;
    }
    Some(ProcInfo {
        pid: bsd.pbi_pid as i32,
        pgid: bsd.pbi_pgid as i32,
        start_time: bsd.pbi_start_tvsec * 1_000_000 + bsd.pbi_start_tvusec,
        zombie: bsd.pbi_status == SZOMB,
    })
}

/// Argument vector of `pid` (KERN_PROCARGS2), or `None` if it cannot be read.
pub fn argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    buf.truncate(size);
    // Layout: argc (i32), exec path, NUL padding, then argc NUL-terminated args.
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    let end = rest.iter().position(|b| *b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|b| *b != 0)?;
    rest = &rest[start..];
    let args: Vec<String> = rest
        .split(|b| *b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    if args.is_empty() { None } else { Some(args) }
}

/// Lists all pids currently visible.
fn all_pids() -> Vec<i32> {
    let mut cap = 4096usize;
    loop {
        let mut buf = vec![0i32; cap];
        let bytes = (cap * mem::size_of::<i32>()) as libc::c_int;
        let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr() as *mut libc::c_void, bytes) };
        if n < 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return buf.into_iter().filter(|p| *p > 0).collect();
        }
        cap *= 2;
    }
}

/// Live (non-zombie) members of process group `pgid`.
pub fn group_members(pgid: i32) -> Vec<ProcInfo> {
    all_pids()
        .into_iter()
        .filter_map(info)
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
    match info(pid) {
        Some(p) if !p.zombie => p.pid == pid && p.start_time == start_time && p.pgid == pgid,
        _ => {
            let members = group_members(pgid);
            !members.is_empty() && members.iter().all(|m| m.start_time >= start_time)
        }
    }
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

/// Outcome of stopping a process group.
#[derive(Debug, PartialEq, Eq)]
pub enum StopResult {
    Terminated,
    Killed,
}

/// Sends SIGTERM to the group, waits up to `timeout`, then SIGKILL.
pub fn stop_group(pgid: i32, timeout: Duration) -> io::Result<StopResult> {
    signal_group(pgid, libc::SIGTERM)?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !group_exists(pgid) {
            return Ok(StopResult::Terminated);
        }
        thread::sleep(Duration::from_millis(50));
    }
    signal_group(pgid, libc::SIGKILL)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && group_exists(pgid) {
        thread::sleep(Duration::from_millis(20));
    }
    Ok(StopResult::Killed)
}

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
