//! macOS process facts: `proc_pidinfo`, `KERN_PROCARGS2`, `proc_listallpids`
//! and `lsof` for listening sockets.

use std::mem;
use std::process::{Command, Stdio};

/// Kernel facts about one process.
#[derive(Debug, Clone, Copy)]
pub struct RawInfo {
    pub pid: i32,
    pub pgid: i32,
    /// Process start time in microseconds since the Unix epoch.
    pub start_time: u64,
    pub zombie: bool,
}

const SZOMB: u32 = 5;

/// Reads kernel information about `pid`. Returns `None` if no such process
/// is visible to the current user.
pub fn raw_info(pid: i32) -> Option<RawInfo> {
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
    Some(RawInfo {
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
pub fn all_pids() -> Vec<i32> {
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

/// Pids listening on TCP `port`, with their command names, via lsof.
pub fn listeners(port: u16) -> Vec<super::Listener> {
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

/// Every TCP socket in LISTEN state owned by the current user, via lsof.
pub fn all_listeners() -> Vec<super::Socket> {
    let uid = unsafe { libc::getuid() }.to_string();
    let out = Command::new("lsof")
        .args(["-nP", "-a", "-u", &uid, "-iTCP", "-sTCP:LISTEN", "-Fpn"])
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    let mut res = Vec::new();
    let mut cur: Option<i32> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            cur = p.parse().ok();
        } else if let (Some(n), Some(pid)) = (line.strip_prefix('n'), cur)
            && let Some((address, port)) = super::split_host_port(n)
        {
            res.push(super::Socket { port, address, pid });
        }
    }
    res
}

/// Current working directory of `pid`, via lsof.
pub fn cwd(pid: i32) -> Option<std::path::PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(std::path::PathBuf::from)
}
