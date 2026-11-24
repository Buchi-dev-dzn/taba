//! Linux process facts from `/proc`: `/proc/PID/stat` for the process group,
//! state and start time, `/proc/PID/cmdline` for argv, and
//! `/proc/net/tcp{,6}` plus `/proc/PID/fd` for listening sockets.

use std::fs;
use std::sync::OnceLock;

/// Kernel facts about one process.
#[derive(Debug, Clone, Copy)]
pub struct RawInfo {
    pub pid: i32,
    pub pgid: i32,
    /// Process start time in microseconds since the Unix epoch.
    pub start_time: u64,
    pub zombie: bool,
}

/// Boot time (seconds since the epoch) and clock ticks per second.
///
/// The kernel derives `btime` from the current wall clock, so it moves when
/// the clock is stepped (NTP, or WSL catching up after the host slept), and
/// start times computed from it would no longer match recorded ones. The
/// first value read in a boot is therefore kept in a per-user file keyed by
/// the boot id, and every later taba process of that boot uses it.
fn clock() -> Option<(u64, u64)> {
    static CLOCK: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *CLOCK.get_or_init(|| {
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        Some((boot_time()?, hz as u64))
    })
}

fn boot_time() -> Option<u64> {
    let read_btime = || -> Option<u64> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|l| l.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()
    };
    let Some(boot_id) = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
    else {
        return read_btime();
    };
    let uid = unsafe { libc::getuid() };
    let path = std::env::temp_dir().join(format!("taba-btime-{uid}-{boot_id}"));
    let cached = || fs::read_to_string(&path).ok()?.trim().parse::<u64>().ok();
    if let Some(b) = cached() {
        return Some(b);
    }
    let btime = read_btime()?;
    // First writer wins; everyone else reads its value.
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return cached().or(Some(btime)),
    };
    let _ = std::io::Write::write_all(&mut f, format!("{btime}\n").as_bytes());
    Some(btime)
}

/// Reads `/proc/PID/stat`. Returns `None` if no such process is visible.
pub fn raw_info(pid: i32) -> Option<RawInfo> {
    if pid <= 0 {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid pgrp ...": comm may contain spaces and ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is field 3 (state); starttime is field 22.
    let state = fields.first()?;
    let pgid: i32 = fields.get(2)?.parse().ok()?;
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    let (btime, hz) = clock()?;
    Some(RawInfo {
        pid,
        pgid,
        start_time: btime * 1_000_000 + ticks * 1_000_000 / hz,
        zombie: matches!(*state, "Z" | "X"),
    })
}

/// Argument vector of `pid`, or `None` if it cannot be read.
pub fn argv(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let args: Vec<String> = match args.split_last() {
        Some((last, init)) if last.is_empty() => init.to_vec(),
        _ => args,
    };
    if args.is_empty() { None } else { Some(args) }
}

/// Lists all pids currently visible.
pub fn all_pids() -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|p| *p > 0)
        .collect()
}

/// Socket inodes listening on TCP `port` (IPv4 and IPv6).
fn listening_inodes(port: u16) -> Vec<u64> {
    let mut out = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let local_port = f[1]
                .rsplit(':')
                .next()
                .and_then(|p| u16::from_str_radix(p, 16).ok());
            if local_port == Some(port)
                && let Ok(inode) = f[9].parse::<u64>()
                && inode != 0
            {
                out.push(inode);
            }
        }
    }
    out
}

/// Pids listening on TCP `port`, with their command names. Only processes
/// whose `/proc/PID/fd` this user can read are found (as with lsof).
pub fn listeners(port: u16) -> Vec<super::Listener> {
    let inodes = listening_inodes(port);
    if inodes.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
    let mut res = Vec::new();
    for pid in all_pids() {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            fs::read_link(fd.path())
                .is_ok_and(|l| wanted.iter().any(|w| l.as_os_str() == w.as_str()))
        });
        if holds {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|c| c.trim_end().to_string())
                .unwrap_or_default();
            res.push((pid, comm));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_process() {
        let me = std::process::id() as i32;
        let info = raw_info(me).expect("own stat");
        assert_eq!(info.pid, me);
        assert!(!info.zombie);
        assert!(info.start_time > 1_500_000_000_000_000);
        assert_eq!(raw_info(me).unwrap().start_time, info.start_time);
        assert!(argv(me).is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn finds_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        assert!(listeners(port).iter().any(|(p, _)| *p == me));
    }
}
