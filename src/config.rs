//! Durations, the optional `$TABA_HOME/config.toml` and the lease TTL
//! precedence (design v0.2, section 3, "Lease TTL configuration").

use crate::error::{Result, TabaError};
use serde_json::json;
use std::path::Path;

/// A lease TTL: a number of seconds, or `none` (no expiry by time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    Secs(u64),
    None,
}

impl Ttl {
    pub fn secs(self) -> Option<u64> {
        match self {
            Ttl::Secs(s) => Some(s),
            Ttl::None => None,
        }
    }

    pub fn from_secs(s: Option<u64>) -> Ttl {
        s.map_or(Ttl::None, Ttl::Secs)
    }

    /// `4h`, `30m`, `90s`, `2d` or `none`: the largest unit that divides it.
    pub fn display(self) -> String {
        match self {
            Ttl::None => "none".into(),
            Ttl::Secs(s) => format_duration(s),
        }
    }
}

pub fn format_duration(s: u64) -> String {
    for (unit, n) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if s > 0 && s.is_multiple_of(n) {
            return format!("{}{unit}", s / n);
        }
    }
    format!("{s}s")
}

/// Parses `90s`, `30m`, `4h` or `2d` into seconds.
pub fn parse_duration(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let bad = || format!("{s:?} is not a duration (use for example 90s, 30m, 4h or 2d)");
    let unit_at = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (num, unit) = s.split_at(unit_at);
    let n: u64 = num.parse().map_err(|_| bad())?;
    let mul = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(bad()),
    };
    n.checked_mul(mul).ok_or_else(bad)
}

/// Parses a lease TTL: a duration or `none`.
pub fn parse_ttl(s: &str) -> std::result::Result<Ttl, String> {
    if s.trim() == "none" {
        return Ok(Ttl::None);
    }
    parse_duration(s).map(Ttl::Secs).and_then(|t| match t {
        Ttl::Secs(0) => Err("a lease TTL must be longer than 0s (or none)".into()),
        t => Ok(t),
    })
}

/// clap value parser for `--ttl`.
pub fn parse_ttl_flag(s: &str) -> std::result::Result<Ttl, String> {
    parse_ttl(s)
}

/// clap value parser for `--service-ttl`.
pub fn parse_service_ttl_flag(s: &str) -> std::result::Result<u64, String> {
    match parse_duration(s)? {
        0 => Err("--service-ttl must be longer than 0s".into()),
        n => Ok(n),
    }
}

/// Where a lease's TTL came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlSource {
    Flag,
    Env,
    Config,
    Default,
}

impl TtlSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TtlSource::Flag => "flag",
            TtlSource::Env => "env",
            TtlSource::Config => "config",
            TtlSource::Default => "default",
        }
    }
}

/// Built-in defaults (decision 2 and 8).
pub const DEFAULT_AGENT_TTL: u64 = 4 * 3_600;
pub const DEFAULT_CLAIM_GRACE: u64 = 2 * 60;
pub const DEFAULT_GC_GRACE: u64 = 10 * 60;
pub const DEFAULT_LOG_RETENTION: u64 = 24 * 3_600;

/// The settings of `config.toml` (all optional) with defaults applied.
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether `config.toml` exists.
    pub present: bool,
    pub lease_ttl: Option<Ttl>,
    pub claim_grace: u64,
    pub gc_grace: u64,
    pub log_retention: u64,
    /// Unknown keys, reported as a warning.
    pub unknown: Vec<String>,
}

fn config_error(path: &Path, key: &str, message: String) -> TabaError {
    TabaError::new("E_CONFIG", format!("{}: {key}: {message}", path.display()))
        .details(json!({ "source": "config", "path": path, "key": key }))
}

impl Config {
    /// Reads `root/config.toml`. A missing file is not an error; a malformed
    /// line or value is `E_CONFIG` naming the key.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join("config.toml");
        let mut cfg = Config {
            present: false,
            lease_ttl: None,
            claim_grace: DEFAULT_CLAIM_GRACE,
            gc_grace: DEFAULT_GC_GRACE,
            log_retention: DEFAULT_LOG_RETENTION,
            unknown: Vec::new(),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cfg),
            Err(e) => {
                return Err(TabaError::new(
                    "E_CONFIG",
                    format!("cannot read {}: {e}", path.display()),
                )
                .details(json!({ "source": "config", "path": path })));
            }
        };
        cfg.present = true;
        for (i, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(config_error(
                    &path,
                    &format!("line {}", i + 1),
                    "expected key = \"value\"".into(),
                ));
            };
            let key = key.trim();
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .ok_or_else(|| {
                    config_error(&path, key, format!("{value} is not a quoted string"))
                })?;
            let dur = |v: &str| parse_duration(v).map_err(|m| config_error(&path, key, m));
            match key {
                "lease_ttl" => {
                    cfg.lease_ttl = Some(parse_ttl(value).map_err(|m| config_error(&path, key, m))?)
                }
                "claim_grace" => cfg.claim_grace = dur(value)?,
                "gc_grace" => cfg.gc_grace = dur(value)?,
                "log_retention" => cfg.log_retention = dur(value)?,
                other => cfg.unknown.push(other.to_string()),
            }
        }
        Ok(cfg)
    }

    /// The effective lease TTL for a new or taken-over lease: `--ttl` >
    /// `$TABA_TTL` > `lease_ttl` > 4 h for agents, `none` for `human`.
    pub fn resolve_ttl(&self, flag: Option<Ttl>, human: bool) -> Result<(Ttl, TtlSource)> {
        if let Some(t) = flag {
            return Ok((t, TtlSource::Flag));
        }
        if let Some(v) = std::env::var("TABA_TTL").ok().filter(|v| !v.is_empty()) {
            let t = parse_ttl(&v).map_err(|m| {
                TabaError::new("E_CONFIG", format!("TABA_TTL: {m}"))
                    .details(json!({ "source": "env", "key": "TABA_TTL", "value": v }))
            })?;
            return Ok((t, TtlSource::Env));
        }
        if let Some(t) = self.lease_ttl {
            return Ok((t, TtlSource::Config));
        }
        let t = if human {
            Ttl::None
        } else {
            Ttl::Secs(DEFAULT_AGENT_TTL)
        };
        Ok((t, TtlSource::Default))
    }
}

fn strip_comment(line: &str) -> &str {
    // `#` outside a quoted string starts a comment.
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("4h"), Ok(14_400));
        assert_eq!(parse_duration("2d"), Ok(172_800));
        assert_eq!(parse_duration("0s"), Ok(0));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("4").is_err());
        assert!(parse_duration("h").is_err());
        assert_eq!(parse_ttl("none"), Ok(Ttl::None));
        assert!(parse_ttl("0s").is_err());
        assert_eq!(Ttl::Secs(14_400).display(), "4h");
        assert_eq!(Ttl::Secs(90).display(), "90s");
        assert_eq!(Ttl::Secs(1800).display(), "30m");
    }

    #[test]
    fn config_file() {
        let dir = std::env::temp_dir().join(format!("taba-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "# comment\nlease_ttl = \"1h\"  # trailing\ngc_grace=\"0s\"\nother = \"x\"\n",
        )
        .unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.lease_ttl, Some(Ttl::Secs(3600)));
        assert_eq!(c.gc_grace, 0);
        assert_eq!(c.claim_grace, DEFAULT_CLAIM_GRACE);
        assert_eq!(c.unknown, ["other"]);
        std::fs::write(dir.join("config.toml"), "lease_ttl = \"soon\"\n").unwrap();
        let e = Config::load(&dir).unwrap_err();
        assert_eq!(e.code, "E_CONFIG");
        assert!(e.message.contains("lease_ttl"));
        let _ = std::fs::remove_file(dir.join("config.toml"));
        let _ = std::fs::remove_dir(&dir);
    }
}
