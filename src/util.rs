//! Small helpers: paths for display, timestamps, shell quoting, log tails.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The user's home directory: `$HOME`, else (Windows) `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|h| !h.is_empty());
    let home = var("HOME");
    #[cfg(windows)]
    let home = home.or_else(|| var("USERPROFILE"));
    home.map(PathBuf::from)
}

/// `std::fs::canonicalize`, without the `\\?\` prefix Windows adds, so paths
/// read like the ones users type (`C:\src\app`) and match `%USERPROFILE%`.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let p = path.canonicalize()?;
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(p)
}

/// `path` with the home directory shown as `~`, for human-readable output.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = home_dir() {
        let canon = canonicalize(&home).ok();
        for h in std::iter::once(&home).chain(canon.as_ref()) {
            if h.parent().is_none() {
                continue; // `/` or a bare drive
            }
            if let Ok(rest) = path.strip_prefix(h) {
                return if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
                };
            }
        }
    }
    path.display().to_string()
}

pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Current local time as `YYYY-MM-DD HH:MM:SS +HHMM`.
#[cfg(windows)]
pub fn local_timestamp() -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::SystemInformation::GetSystemTime;
    use windows_sys::Win32::System::Time::{SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime};
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetSystemTime(&mut utc) };
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return format!("unix time {}", now_micros() / 1_000_000);
    }
    let ticks = |st: &SYSTEMTIME| -> i64 {
        let mut ft = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        unsafe { SystemTimeToFileTime(st, &mut ft) };
        (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime)
    };
    // 100 ns ticks to whole minutes.
    let off = (ticks(&local) - ticks(&utc) + 300_000_000).div_euclid(600_000_000);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}{:02}{:02}",
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
        local.wSecond,
        if off < 0 { '-' } else { '+' },
        off.abs() / 60,
        off.abs() % 60
    )
}

/// Current local time as `YYYY-MM-DD HH:MM:SS +HHMM`.
#[cfg(unix)]
pub fn local_timestamp() -> String {
    let now = (now_micros() / 1_000_000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return format!("unix time {now}");
    }
    let off = tm.tm_gmtoff / 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        if off < 0 { '-' } else { '+' },
        off.abs() / 60,
        off.abs() % 60
    )
}

/// Current UTC time as RFC 3339 (`2026-11-24T10:02:11Z`).
pub fn rfc3339_now() -> String {
    rfc3339(now_micros() / 1_000_000)
}

pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Quotes `arg` for a POSIX shell if it needs it.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '_' | '.' | '/' | ':' | '=' | '{' | '}' | ',' | '@' | '%' | '+'
                )
        });
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The last `n` lines of the file at `path` (empty if unreadable).
pub fn log_tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_502_011), "2026-11-24T06:33:31Z");
    }

    #[test]
    fn shell_quote_only_when_needed() {
        assert_eq!(shell_quote("--port"), "--port");
        assert_eq!(shell_quote("{port}"), "{port}");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
