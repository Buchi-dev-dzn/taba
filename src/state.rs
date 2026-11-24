//! On-disk state: one JSON record per service plus one log file per service.
//!
//! Layout under `$TABA_HOME` (default `~/.taba`):
//!
//! ```text
//! services/<scope>/<name>.json   service records (v0.1 fields + optional v0.2 fields)
//! logs/<scope>/<name>.log         service output
//! ops/<scope>/<name>.json         in-flight mutation of one service (operation lock)
//! events.jsonl                    append-only audit log
//! version                         state schema version
//! lock                            global lock (flock / LockFileEx)
//! ```
//!
//! Every field added in v0.2 is optional when reading, so v0.1 records load
//! unchanged.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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

    // ---- v0.2 fields; absent in v0.1 records.
    /// The command as given, before `{port}` substitution. Idempotent re-runs
    /// compare this instead of `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_template: Option<Vec<String>>,
    /// `"auto"`, `"auto:START"` or the explicit port as given; `None` when
    /// the service has no port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_request: Option<String>,
    /// `pid-start_time`, the identity of this running instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Agent that started this instance (`human` when TABA_AGENT is unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    /// Set once a `--wait` ready check has passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<Ready>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ready {
    pub check: String,
    /// Microseconds since the Unix epoch.
    pub ready_at: u64,
}

/// An in-flight mutation of one service (`stop`, `start`, `replace`), held by
/// a taba process. It is stale once that process is gone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpLock {
    pub op: String,
    pub pid: i32,
    pub pid_start_time: u64,
    pub agent: String,
    pub started_at: String,
}

impl OpLock {
    /// A lock for operation `op` held by the current process.
    pub fn mine(op: &str, agent: &str) -> OpLock {
        let (pid, pid_start_time) = crate::proc::self_identity();
        OpLock {
            op: op.to_string(),
            pid,
            pid_start_time,
            agent: agent.to_string(),
            started_at: crate::util::rfc3339_now(),
        }
    }

    pub fn is_alive(&self) -> bool {
        crate::proc::is_alive(self.pid, self.pid_start_time)
    }

    pub fn is_mine(&self) -> bool {
        (self.pid, self.pid_start_time) == crate::proc::self_identity()
    }
}

/// Current state schema version, written to `$TABA_HOME/version`.
pub const SCHEMA_VERSION: u32 = 2;

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

    /// `pid-start_time`; computed for v0.1 records that lack the field.
    pub fn instance(&self) -> String {
        self.instance
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.pid, self.start_time))
    }

    /// The command before `{port}` substitution (v0.1 records: the command).
    pub fn template(&self) -> &[String] {
        self.command_template.as_deref().unwrap_or(&self.command)
    }

    /// The port request as given (v0.1 records: the explicit port).
    pub fn port_request(&self) -> Option<String> {
        self.port_request
            .clone()
            .or_else(|| self.port.map(|p| p.to_string()))
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
                    crate::util::home_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
                home.join(".taba")
            }
        };
        fs::create_dir_all(root.join("services"))?;
        fs::create_dir_all(root.join("logs"))?;
        let store = Store { root };
        store.check_version()?;
        Ok(store)
    }

    /// Fails if the state was written by a newer schema than this binary's.
    fn check_version(&self) -> io::Result<()> {
        match fs::read_to_string(self.root.join("version")) {
            Ok(text) => match text.trim().parse::<u32>() {
                Ok(v) if v <= SCHEMA_VERSION => Ok(()),
                _ => Err(io::Error::other(format!(
                    "state in {} has schema version {:?}, newer than this taba supports ({SCHEMA_VERSION}); upgrade taba",
                    self.root.display(),
                    text.trim()
                ))),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Records the schema version. Called by mutating commands under the lock.
    pub fn mark_version(&self) -> io::Result<()> {
        let path = self.root.join("version");
        if path.exists() {
            return Ok(());
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{SCHEMA_VERSION}\n"))?;
        fs::rename(tmp, path)
    }

    pub fn lock(&self) -> io::Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("lock"))?;
        // flock(LOCK_EX) on Unix, LockFileEx on Windows; released when the
        // file is closed, also if taba dies.
        file.lock()?;
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

    fn op_path(&self, scope: &str, name: &str) -> PathBuf {
        self.root
            .join("ops")
            .join(scope)
            .join(format!("{name}.json"))
    }

    pub fn get(&self, scope: &str, name: &str) -> io::Result<Option<Service>> {
        read_json(&self.record_path(scope, name))
    }

    pub fn put(&self, svc: &Service) -> io::Result<()> {
        write_json(&self.record_path(&svc.scope, &svc.name), svc)
    }

    /// The operation lock of `scope/name`, if a record exists (it may be stale).
    pub fn get_op(&self, scope: &str, name: &str) -> io::Result<Option<OpLock>> {
        read_json(&self.op_path(scope, name))
    }

    /// The operation lock of `scope/name` if its holder is still alive.
    /// Stale records are left for the caller to overwrite or remove.
    pub fn live_op(&self, scope: &str, name: &str) -> io::Result<Option<OpLock>> {
        Ok(self.get_op(scope, name)?.filter(OpLock::is_alive))
    }

    /// Writes the operation lock. Call under the global lock, after checking
    /// that no live lock exists.
    pub fn put_op(&self, scope: &str, name: &str, op: &OpLock) -> io::Result<()> {
        write_json(&self.op_path(scope, name), op)
    }

    /// Removes the operation lock of `scope/name` if the current process
    /// holds it. Call under the global lock.
    pub fn release_op(&self, scope: &str, name: &str) -> io::Result<()> {
        if let Some(op) = self.get_op(scope, name)?
            && op.is_mine()
        {
            remove_file_and_empty_parent(&self.op_path(scope, name))?;
        }
        Ok(())
    }

    /// Removes a stale (dead holder) operation lock. Call under the global lock.
    pub fn clear_stale_op(&self, scope: &str, name: &str) -> io::Result<()> {
        if let Some(op) = self.get_op(scope, name)?
            && !op.is_alive()
        {
            remove_file_and_empty_parent(&self.op_path(scope, name))?;
        }
        Ok(())
    }

    /// Appends one event to `events.jsonl` as a single line written with one
    /// `write`. Call under the global lock.
    pub fn append_event(&self, event: &serde_json::Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events.jsonl"))?;
        f.write_all(&line)
    }

    /// Deletes the record and log of `scope/name`. Both paths are derived
    /// from validated identifiers under the state root, never from the
    /// record's contents, so nothing outside `$TABA_HOME` can be touched.
    /// Empty per-scope directories are removed too.
    pub fn remove(&self, scope: &str, name: &str) -> io::Result<()> {
        validate_ident("scope", scope).map_err(io::Error::other)?;
        validate_ident("name", name).map_err(io::Error::other)?;
        for path in [
            self.record_path(scope, name),
            self.log_path(scope, name),
            self.op_path(scope, name),
        ] {
            remove_file_and_empty_parent(&path)?;
        }
        Ok(())
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::other(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes to a temporary file and renames it, so lock-free readers see the
/// old or the new content, never a partial one.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    fs::create_dir_all(path.parent().expect("record has parent"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)
}

fn remove_file_and_empty_parent(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir); // only succeeds when empty
    }
    Ok(())
}
