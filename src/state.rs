//! On-disk state: one JSON record per service plus one log file per service.
//!
//! Layout under `$TABA_HOME` (default `~/.taba`):
//!
//! ```text
//! services/<scope>/<name>.json
//! logs/<scope>/<name>.log
//! lock
//! ```

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub scope: String,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub port: Option<u16>,
    pub pid: i32,
    pub pgid: i32,
    /// Process start time in microseconds since the Unix epoch.
    pub start_time: u64,
    pub log_path: PathBuf,
}

impl Service {
    pub fn id(&self) -> String {
        format!("{}/{}", self.scope, self.name)
    }

    pub fn url(&self) -> Option<String> {
        self.port.map(|p| format!("http://localhost:{p}"))
    }

    pub fn is_running(&self) -> bool {
        crate::proc::group_is_ours(self.pid, self.pgid, self.start_time)
    }
}

pub struct Store {
    root: PathBuf,
}

/// Exclusive advisory lock on the state directory; released on drop.
pub struct Lock {
    _file: File,
}

pub fn validate_ident(kind: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid {kind} {s:?}: use letters, digits, '-', '_' or '.' (not starting with '.')"
        ))
    }
}

impl Store {
    pub fn open() -> io::Result<Store> {
        let root = match std::env::var_os("TABA_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => {
                let home =
                    std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
                Path::new(&home).join(".taba")
            }
        };
        fs::create_dir_all(root.join("services"))?;
        fs::create_dir_all(root.join("logs"))?;
        Ok(Store { root })
    }

    pub fn lock(&self) -> io::Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Lock { _file: file })
    }

    fn record_path(&self, scope: &str, name: &str) -> PathBuf {
        self.root
            .join("services")
            .join(scope)
            .join(format!("{name}.json"))
    }

    pub fn log_path(&self, scope: &str, name: &str) -> PathBuf {
        self.root
            .join("logs")
            .join(scope)
            .join(format!("{name}.log"))
    }

    pub fn get(&self, scope: &str, name: &str) -> io::Result<Option<Service>> {
        let path = self.record_path(scope, name);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::other(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn put(&self, svc: &Service) -> io::Result<()> {
        let path = self.record_path(&svc.scope, &svc.name);
        fs::create_dir_all(path.parent().expect("record has parent"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(svc)?)?;
        fs::rename(tmp, path)
    }

    /// All records, optionally limited to one scope, sorted by scope/name.
    pub fn list(&self, scope: Option<&str>) -> io::Result<Vec<Service>> {
        let services_dir = self.root.join("services");
        let scopes: Vec<String> = match scope {
            Some(s) => vec![s.to_string()],
            None => {
                let mut v = Vec::new();
                for e in fs::read_dir(&services_dir)? {
                    let e = e?;
                    if e.file_type()?.is_dir() {
                        v.push(e.file_name().to_string_lossy().into_owned());
                    }
                }
                v
            }
        };
        let mut out = Vec::new();
        for s in scopes {
            let dir = services_dir.join(&s);
            let entries = match fs::read_dir(&dir) {
                Ok(it) => it,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for e in entries {
                let path = e?.path();
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let bytes = fs::read(&path)?;
                match serde_json::from_slice::<Service>(&bytes) {
                    Ok(svc) => out.push(svc),
                    Err(err) => {
                        eprintln!("taba: skipping unreadable record {}: {err}", path.display())
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
        Ok(out)
    }
}
