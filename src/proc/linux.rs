//! Linux process facts from `/proc`: `/proc/PID/stat` for the process group,
//! state and start time, `/proc/PID/cmdline` for argv, and
//! `/proc/net/tcp{,6}` plus `/proc/PID/fd` for listening sockets.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
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
/// first value read in a boot is kept in the state directory by boot ID.
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
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    let boot_id = boot_id.trim();
    if boot_id.is_empty() || !boot_id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
        return read_btime();
    }
    let uid = unsafe { libc::geteuid() };
    let initial = || {
        let legacy_uid = unsafe { libc::getuid() };
        let legacy = std::env::temp_dir().join(format!("taba-btime-{legacy_uid}-{boot_id}"));
        // Older versions created this file with the user's umask.
        read_cached_btime(&legacy, uid, |mode| mode & 0o7022 == 0).or_else(read_btime)
    };
    crate::state::state_dir()
        .and_then(|root| cached_boot_time(&root.join("btime"), boot_id, uid, initial))
        .or_else(initial)
}

fn read_btime() -> Option<u64> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

fn read_cached_btime(path: &Path, uid: u32, valid_mode: impl FnOnce(u32) -> bool) -> Option<u64> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.uid() != uid || !valid_mode(metadata.mode() & 0o7777) {
        return None;
    }
    let mut text = String::new();
    file.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

fn write_cached_btime(path: &Path, dir: &Path, btime: u64) -> std::io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    writeln!(temp, "{btime}")?;
    temp.persist(path)?;
    Ok(())
}

fn cached_boot_time(
    dir: &Path,
    boot_id: &str,
    uid: u32,
    initial: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .ok()?;
    let metadata = directory.metadata().ok()?;
    if metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return None;
    }
    // Serialize first initialization and repairs independently of the state lock.
    directory.lock().ok()?;
    let cache = dir.join(boot_id);
    let recovery = dir.join(format!("{boot_id}.recovery"));
    let cached = read_cached_btime(&cache, uid, |mode| mode == 0o600);
    let saved = read_cached_btime(&recovery, uid, |mode| mode == 0o600);
    let btime = saved.or(cached).or_else(initial)?;
    // Keep a recovery copy so deleting the cache mid-boot does not change identities.
    if saved.is_none() {
        let _ = write_cached_btime(&recovery, dir, btime);
    }
    if cached != Some(btime) {
        let _ = write_cached_btime(&cache, dir, btime);
    }
    Some(btime)
}

/// Reads `/proc/PID/stat`. Returns `None` if no such process is visible.
pub fn raw_info(pid: i32) -> Option<RawInfo> {
    raw_info_with_clock(pid, clock)
}

fn raw_info_with_clock(pid: i32, clock: impl FnOnce() -> Option<(u64, u64)>) -> Option<RawInfo> {
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
        start_time: btime
            .checked_mul(1_000_000)?
            .checked_add(ticks.checked_mul(1_000_000)? / hz)?,
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

/// Decodes a `/proc/net/tcp{,6}` address (`0100007F` or 32 hex digits, each
/// 32-bit word in host byte order).
fn decode_addr(hex: &str) -> Option<String> {
    let words: Vec<u32> = (0..hex.len() / 8)
        .map(|i| u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok())
        .collect::<Option<_>>()?;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
    Some(match bytes.len() {
        4 => super::format_ip(std::net::IpAddr::from(
            <[u8; 4]>::try_from(&bytes[..]).ok()?,
        )),
        16 => super::format_ip(std::net::IpAddr::from(
            <[u8; 16]>::try_from(&bytes[..]).ok()?,
        )),
        _ => return None,
    })
}

/// Listening sockets as (inode, address, port), IPv4 and IPv6.
fn listening_sockets() -> Vec<(u64, String, u16)> {
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
            let Some((addr, port)) = f[1].split_once(':') else {
                continue;
            };
            let (Ok(port), Some(addr), Ok(inode)) = (
                u16::from_str_radix(port, 16),
                decode_addr(addr),
                f[9].parse::<u64>(),
            ) else {
                continue;
            };
            if inode != 0 {
                out.push((inode, addr, port));
            }
        }
    }
    out
}

/// Maps socket inodes to the pids holding them. Only processes whose
/// `/proc/PID/fd` this user can read are found (as with lsof).
fn socket_owners(inodes: &[u64]) -> Vec<(u64, i32)> {
    let mut res = Vec::new();
    if inodes.is_empty() {
        return res;
    }
    for pid in all_pids() {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            let link = link.to_string_lossy();
            if let Some(inode) = link
                .strip_prefix("socket:[")
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|i| i.parse::<u64>().ok())
                && inodes.contains(&inode)
                && !res.contains(&(inode, pid))
            {
                res.push((inode, pid));
            }
        }
    }
    res
}

fn comm(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|c| c.trim_end().to_string())
        .unwrap_or_default()
}

/// Pids listening on TCP `port`, with their command names.
pub fn listeners(port: u16) -> Vec<super::Listener> {
    let inodes: Vec<u64> = listening_sockets()
        .into_iter()
        .filter(|s| s.2 == port)
        .map(|s| s.0)
        .collect();
    let mut pids: Vec<i32> = socket_owners(&inodes).into_iter().map(|o| o.1).collect();
    pids.dedup();
    pids.into_iter().map(|p| (p, comm(p))).collect()
}

/// Every TCP socket in LISTEN state held by a process of the current user.
pub fn all_listeners() -> Vec<super::Socket> {
    let sockets = listening_sockets();
    let inodes: Vec<u64> = sockets.iter().map(|s| s.0).collect();
    let owners = socket_owners(&inodes);
    let mut res = Vec::new();
    for (inode, address, port) in sockets {
        for (_, pid) in owners.iter().filter(|o| o.0 == inode) {
            res.push(super::Socket {
                port,
                address: address.clone(),
                pid: *pid,
            });
        }
    }
    res
}

/// Current working directory of `pid`.
pub fn cwd(pid: i32) -> Option<std::path::PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn cache_deletion_preserves_live_process_identity() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("btimes");
        let boot = "abc";
        let uid = unsafe { libc::geteuid() };
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        let pid = std::process::id() as i32;
        let read_process = |btime| {
            raw_info_with_clock(pid, || {
                Some((cached_boot_time(&dir, boot, uid, || Some(btime))?, hz))
            })
            .unwrap()
        };
        let original = read_process(1_700_000_000);

        for name in [boot, "abc.recovery"] {
            fs::remove_file(dir.join(name)).unwrap();
            let after_clock_step = read_process(1_700_000_005);
            assert_eq!(after_clock_step.start_time, original.start_time);
            assert!(!after_clock_step.zombie);
            assert!(dir.join(name).is_file());
        }
        assert_eq!(
            cached_boot_time(&dir, "def", uid, || Some(1_800_000_000)),
            Some(1_800_000_000)
        );
    }

    #[test]
    fn ignores_insecure_cache_entries() {
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let uid = unsafe { libc::geteuid() };
        let path = dir.path().join("abc");
        let target = dir.path().join("other");
        write_cached_btime(&target, dir.path(), 100).unwrap();
        assert_eq!(read_cached_btime(&target, uid ^ 1, |_| true), None);

        symlink(&target, &path).unwrap();
        assert_eq!(read_cached_btime(&path, uid, |_| true), None);
        fs::remove_file(&path).unwrap();
        fs::write(&path, "100\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            cached_boot_time(dir.path(), "abc", uid, || Some(200)),
            Some(200)
        );
        assert_eq!(
            read_cached_btime(&path, uid, |mode| mode == 0o600),
            Some(200)
        );
        assert_eq!(
            read_cached_btime(&target, uid, |mode| mode == 0o600),
            Some(100)
        );
    }

    #[test]
    fn concurrent_initializers_use_the_same_btime() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("btimes");
        let uid = unsafe { libc::geteuid() };
        let barrier = std::sync::Barrier::new(2);
        let initialize = |btime| {
            barrier.wait();
            cached_boot_time(&dir, "abc", uid, || Some(btime)).unwrap()
        };
        std::thread::scope(|scope| {
            let first = scope.spawn(|| initialize(100));
            let second = initialize(200);
            assert_eq!(first.join().unwrap(), second);
        });
    }

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
