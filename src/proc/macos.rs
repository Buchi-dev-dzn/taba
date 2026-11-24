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
    if rc != 0 {
        return None;
    }
    parse_argv(buf.get(..size)?)
}

fn parse_argv(buf: &[u8]) -> Option<Vec<String>> {
    let (argc, strings) = buf.split_first_chunk::<4>()?;
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).ok()?;
    let path_end = strings.iter().position(|&byte| byte == 0)?;
    // XNU pads the terminated exec path to the target's pointer width.
    // Supported macOS processes are 64-bit, including under Rosetta.
    // Skipping every NUL would discard empty leading args and then read envp.
    let argv_start = (path_end + 1).next_multiple_of(8);
    if strings
        .get(path_end..argv_start)?
        .iter()
        .any(|&byte| byte != 0)
    {
        return None;
    }
    let mut rest = strings.get(argv_start..)?;
    if argc == 0 || argc > rest.len() {
        return None;
    }
    let mut args = Vec::new();
    for _ in 0..argc {
        let end = rest.iter().position(|&byte| byte == 0)?;
        args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    Some(args)
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

#[cfg(test)]
mod tests {
    use super::parse_argv;

    #[test]
    fn parses_empty_argv_entries_without_reading_environment() {
        let mut buf = 4_i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/x\0\0\0\0\0\0\0two words\0\0last\0ENV=ignored\0");
        assert_eq!(parse_argv(&buf).unwrap(), ["", "two words", "", "last"]);
    }

    #[test]
    fn rejects_incomplete_or_invalid_argv() {
        let mut buf = 2_i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0sh\0unterminated");
        assert_eq!(parse_argv(&buf), None);
        buf[..4].copy_from_slice(&(-1_i32).to_ne_bytes());
        assert_eq!(parse_argv(&buf), None);
    }
}
