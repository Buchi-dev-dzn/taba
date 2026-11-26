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
//! lock.holder                     pid of the last process that took the global lock
//! btime/                          Linux boot-time cache and recovery copies
//! ```
//!
//! On Unix the directories are created 0700 and the files 0600 (see
//! `create_private_dir_all` and `private_file`).
//!
//! Every field added in v0.2 is optional when reading, so v0.1 records load
//! unchanged.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which state file could not be read, for the error's next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateFile {
    Service { scope: String, name: String },
    Scope(String),
    Other,
}

/// A state file that exists but can't be parsed: empty or damaged. A missing
/// file is not this; it means "no record". taba never replaces or rewrites
/// such a file on its own: commands that need it fail with `E_STATE`, and
/// listings skip it with `W_STATE_SKIPPED`.
#[derive(Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub file: StateFile,
    pub error: String,
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

impl std::error::Error for Unreadable {}

/// The `Unreadable` behind an I/O error, if that is what it is.
pub fn unreadable(e: &io::Error) -> Option<&Unreadable> {
    e.get_ref()?.downcast_ref()
}

/// Names the file an `Unreadable` error is about.
fn tag(e: io::Error, file: StateFile) -> io::Error {
    if unreadable(&e).is_none() {
        return e;
    }
    match e.into_inner().map(|i| i.downcast::<Unreadable>()) {
        Some(Ok(mut u)) => {
            u.file = file;
            io::Error::new(io::ErrorKind::InvalidData, *u)
        }
        _ => io::Error::other("unreadable state file"),
    }
}

/// Unreadable files skipped by listings in this process, reported as
/// `W_STATE_SKIPPED` warnings instead of on stderr.
static SKIPPED: Mutex<std::collections::BTreeSet<String>> =
    Mutex::new(std::collections::BTreeSet::new());

fn note_skipped(u: &Unreadable) {
    let what = match &u.file {
        StateFile::Service { scope, name } => format!("service record {scope}/{name}"),
        StateFile::Scope(scope) => format!("scope record {scope}"),
        StateFile::Other => "state file".into(),
    };
    if let Ok(mut set) = SKIPPED.lock() {
        set.insert(format!("skipped unreadable {what} ({u})"));
    }
}

/// Drains the skipped-file messages collected so far.
pub fn take_skipped() -> Vec<String> {
    SKIPPED
        .lock()
        .map(|mut set| std::mem::take(&mut *set).into_iter().collect())
        .unwrap_or_default()
}

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
    /// `run --service-ttl`: the service is dropped by `gc` once
    /// `service_expires_at` has passed (renewed by an `already_running` run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_expires_at: Option<String>,
    /// Hand-offs of this service between scopes (`service_moved`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<serde_json::Value>,
    /// When cleanup sent this instance its graceful stop request (Unix
    /// seconds); a later cleanup forces it once `stop_timeout` has passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<u64>,
    /// Names (never values) of the variables `run --env` or taba.toml set,
    /// sorted; a re-run with other names is a different service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
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

/// Default wait for the global lock before `E_BUSY`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait before saying so on a terminal.
const LOCK_NOTICE_AFTER: Duration = Duration::from_millis(500);
const LOCK_POLL: Duration = Duration::from_millis(25);

/// The global lock stayed busy for the whole timeout.
#[derive(Debug)]
pub struct LockBusy {
    pub path: PathBuf,
    /// The pid that last took the lock, if it recorded one.
    pub holder_pid: Option<u32>,
    pub waited: Duration,
}

impl std::fmt::Display for LockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the state lock is held by another taba process")?;
        if let Some(pid) = self.holder_pid {
            write!(f, " (pid {pid})")?;
        }
        write!(f, "; gave up after {:.1}s", self.waited.as_secs_f64())
    }
}

impl std::error::Error for LockBusy {}

/// The `LockBusy` behind an I/O error, if that is what it is.
pub fn lock_busy(e: &io::Error) -> Option<&LockBusy> {
    e.get_ref()?.downcast_ref()
}

/// `$TABA_LOCK_TIMEOUT` in seconds (0 = try once), or the default.
fn lock_timeout() -> io::Result<Duration> {
    let Some(v) = std::env::var_os("TABA_LOCK_TIMEOUT").filter(|v| !v.is_empty()) else {
        return Ok(LOCK_TIMEOUT);
    };
    v.to_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && (0.0..=86400.0).contains(s))
        .map(Duration::from_secs_f64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                BadLockTimeout(v.to_string_lossy().into_owned()),
            )
        })
}

/// `$TABA_LOCK_TIMEOUT` is not a number of seconds in range (`E_CONFIG`).
#[derive(Debug)]
pub struct BadLockTimeout(pub String);

impl std::fmt::Display for BadLockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TABA_LOCK_TIMEOUT: invalid value {:?}: use seconds between 0 and 86400",
            self.0
        )
    }
}

impl std::error::Error for BadLockTimeout {}

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

pub(crate) fn state_dir() -> Option<PathBuf> {
    match std::env::var_os("TABA_HOME") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => crate::util::home_dir().map(|home| home.join(".taba")),
    }
}

impl Store {
    pub fn open() -> io::Result<Store> {
        let root = state_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
        create_private_dir_all(&root.join("services"))?;
        create_private_dir_all(&root.join("logs"))?;
        make_private(&root);
        let store = Store { root };
        store.check_version()?;
        Ok(store)
    }

    /// A store at `root` (tests; the binary uses `TABA_HOME` via `open`).
    #[cfg(test)]
    pub fn at(root: PathBuf) -> Store {
        create_private_dir_all(&root.join("services")).unwrap();
        create_private_dir_all(&root.join("logs")).unwrap();
        Store { root }
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
        private_file()
            .write(true)
            .truncate(true)
            .open(&tmp)?
            .write_all(format!("{SCHEMA_VERSION}\n").as_bytes())?;
        fs::rename(tmp, path)
    }

    /// Takes the global lock, retrying until `$TABA_LOCK_TIMEOUT` (default
    /// 10 seconds) passes. On a terminal, says once that it is waiting. A
    /// timeout is an error carrying `LockBusy` (`E_BUSY`).
    pub fn lock(&self) -> io::Result<Lock> {
        let timeout = lock_timeout()?;
        let start = Instant::now();
        let mut told = false;
        loop {
            if let Some(lock) = self.try_lock()? {
                return Ok(lock);
            }
            let waited = start.elapsed();
            if waited >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    LockBusy {
                        path: self.root.join("lock"),
                        holder_pid: self.lock_holder(),
                        waited,
                    },
                ));
            }
            if !told && waited >= LOCK_NOTICE_AFTER && io::stderr().is_terminal() {
                let holder = self
                    .lock_holder()
                    .map(|p| format!(" (held by pid {p})"))
                    .unwrap_or_default();
                eprintln!(
                    "taba: waiting for state lock{holder}, up to {}s",
                    timeout.as_secs_f64()
                );
                told = true;
            }
            std::thread::sleep(LOCK_POLL.min(timeout - waited));
        }
    }

    /// Takes the global lock if it is free right now; `None` if another
    /// process holds it.
    pub fn try_lock(&self) -> io::Result<Option<Lock>> {
        let file = private_file()
            .truncate(false)
            .write(true)
            .open(self.root.join("lock"))?;
        // flock(LOCK_EX) on Unix, LockFileEx on Windows; released when the
        // file is closed, also if taba dies.
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
        // Best effort, for the `E_BUSY` of whoever waits next. A separate
        // file, because Windows does not let others read a locked range.
        let _ = private_file()
            .write(true)
            .truncate(true)
            .open(self.root.join("lock.holder"))
            .and_then(|mut holder| writeln!(holder, "{}", std::process::id()));
        Ok(Some(Lock { _file: file }))
    }

    /// The pid recorded by the last process that took the global lock.
    fn lock_holder(&self) -> Option<u32> {
        fs::read_to_string(self.root.join("lock.holder"))
            .ok()?
            .trim()
            .parse()
            .ok()
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
        read_json(&self.record_path(scope, name)).map_err(|e| {
            tag(
                e,
                StateFile::Service {
                    scope: scope.into(),
                    name: name.into(),
                },
            )
        })
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
        let mut f = private_file()
            .append(true)
            .open(self.root.join("events.jsonl"))?;
        f.write_all(&line)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- scope records (`scopes/<scope>.json`)

    fn scope_path(&self, scope: &str) -> PathBuf {
        self.root.join("scopes").join(format!("{scope}.json"))
    }

    pub fn get_scope(&self, scope: &str) -> io::Result<Option<crate::lease::ScopeRecord>> {
        read_json(&self.scope_path(scope)).map_err(|e| tag(e, StateFile::Scope(scope.into())))
    }

    pub fn put_scope(&self, rec: &crate::lease::ScopeRecord) -> io::Result<()> {
        write_json(&self.scope_path(&rec.scope), rec)
    }

    pub fn remove_scope(&self, scope: &str) -> io::Result<()> {
        validate_ident("scope", scope).map_err(io::Error::other)?;
        remove_file_and_empty_parent(&self.scope_path(scope))
    }

    /// Every scope record, sorted by scope.
    pub fn list_scopes(&self) -> io::Result<Vec<crate::lease::ScopeRecord>> {
        let mut out = Vec::new();
        let dir = match fs::read_dir(self.root.join("scopes")) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for e in dir {
            let path = e?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let scope = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match read_json(&path) {
                Ok(Some(rec)) => out.push(rec),
                Ok(None) => {}
                Err(e) => {
                    if let Some(u) = unreadable(&tag(e, StateFile::Scope(scope))) {
                        note_skipped(u);
                    }
                }
            }
        }
        out.sort_by(|a: &crate::lease::ScopeRecord, b| a.scope.cmp(&b.scope));
        Ok(out)
    }

    // ---- claim queues (`claims/<scope>/<name>.json`)

    fn claims_path(&self, scope: &str, name: &str) -> PathBuf {
        self.root
            .join("claims")
            .join(scope)
            .join(format!("{name}.json"))
    }

    pub fn get_claims(&self, scope: &str, name: &str) -> io::Result<crate::claims::ClaimQueue> {
        Ok(
            read_json(&self.claims_path(scope, name))?.unwrap_or_else(|| {
                crate::claims::ClaimQueue {
                    service: format!("{scope}/{name}"),
                    queue: Vec::new(),
                }
            }),
        )
    }

    /// Writes the queue, or removes the file when it is empty.
    pub fn put_claims(
        &self,
        scope: &str,
        name: &str,
        q: &crate::claims::ClaimQueue,
    ) -> io::Result<()> {
        let path = self.claims_path(scope, name);
        if q.queue.is_empty() {
            remove_file_and_empty_parent(&path)
        } else {
            write_json(&path, q)
        }
    }

    /// Moves the record (rewritten as `svc`, already renamed) and the log of
    /// `from_scope/name` to `svc.scope/name`. The running process keeps its
    /// open log file, so logging continues in the renamed file.
    pub fn move_service(&self, from_scope: &str, svc: &Service) -> io::Result<()> {
        let old_log = self.log_path(from_scope, &svc.name);
        let new_log = self.log_path(&svc.scope, &svc.name);
        create_private_dir_all(new_log.parent().expect("log has parent"))?;
        if old_log.exists() {
            fs::rename(&old_log, &new_log)?;
        }
        self.put(svc)?;
        remove_file_and_empty_parent(&self.record_path(from_scope, &svc.name))?;
        if let Some(dir) = old_log.parent() {
            let _ = fs::remove_dir(dir);
        }
        let old_claims = self.claims_path(from_scope, &svc.name);
        if old_claims.exists() {
            let new_claims = self.claims_path(&svc.scope, &svc.name);
            create_private_dir_all(new_claims.parent().expect("claims have parent"))?;
            fs::rename(&old_claims, &new_claims)?;
            if let Some(dir) = old_claims.parent() {
                let _ = fs::remove_dir(dir);
            }
        }
        Ok(())
    }

    /// Logs without a service record (of dropped services), with their
    /// scope directory.
    pub fn orphan_logs(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let Ok(scopes) = fs::read_dir(self.root.join("logs")) else {
            return Ok(out);
        };
        for s in scopes.flatten() {
            let scope = s.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(s.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".log"))
                else {
                    continue;
                };
                if !self.record_path(&scope, name).exists() {
                    out.push(path);
                }
            }
        }
        Ok(out)
    }

    /// Deletes one log file under `logs/` and its scope directory if empty.
    pub fn remove_log_file(&self, path: &Path) -> io::Result<()> {
        if !path.starts_with(self.root.join("logs")) {
            return Err(io::Error::other("not a taba log"));
        }
        remove_file_and_empty_parent(path)
    }

    /// Deletes the record of `scope/name` (and its operation lock and claim
    /// queue), keeping the log: `gc` drops services this way and deletes
    /// their logs after `log_retention`.
    pub fn remove_record_keep_log(&self, scope: &str, name: &str) -> io::Result<()> {
        validate_ident("scope", scope).map_err(io::Error::other)?;
        validate_ident("name", name).map_err(io::Error::other)?;
        for path in [
            self.record_path(scope, name),
            self.op_path(scope, name),
            self.claims_path(scope, name),
        ] {
            remove_file_and_empty_parent(&path)?;
        }
        Ok(())
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
            self.claims_path(scope, name),
        ] {
            remove_file_and_empty_parent(&path)?;
        }
        Ok(())
    }

    /// All records, optionally limited to one scope, sorted by scope/name.
    /// Unreadable records are skipped and reported as `W_STATE_SKIPPED`.
    pub fn list(&self, scope: Option<&str>) -> io::Result<Vec<Service>> {
        let (out, skipped) = self.scan(scope)?;
        skipped.iter().for_each(note_skipped);
        Ok(out)
    }

    /// Names of the unreadable records in `scope`.
    pub fn unreadable_records(&self, scope: &str) -> io::Result<Vec<String>> {
        let (_, skipped) = self.scan(Some(scope))?;
        Ok(skipped
            .into_iter()
            .filter_map(|u| match u.file {
                StateFile::Service { name, .. } => Some(name),
                _ => None,
            })
            .collect())
    }

    fn scan(&self, scope: Option<&str>) -> io::Result<(Vec<Service>, Vec<Unreadable>)> {
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
        let mut skipped = Vec::new();
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
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match read_json::<Service>(&path) {
                    Ok(Some(svc)) => out.push(svc),
                    Ok(None) => {}
                    Err(e) => {
                        let file = StateFile::Service {
                            scope: s.clone(),
                            name,
                        };
                        match tag(e, file)
                            .into_inner()
                            .map(|i| i.downcast::<Unreadable>())
                        {
                            Some(Ok(u)) => skipped.push(*u),
                            Some(Err(inner)) => return Err(io::Error::other(inner)),
                            None => return Err(io::Error::other("cannot read a service record")),
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
        Ok((out, skipped))
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            let error = if bytes.iter().all(u8::is_ascii_whitespace) {
                "empty file".to_string()
            } else {
                e.to_string()
            };
            io::Error::new(
                io::ErrorKind::InvalidData,
                Unreadable {
                    path: path.to_path_buf(),
                    file: StateFile::Other,
                    error,
                },
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Creates `dir` and any missing parents. On Unix, directories taba creates
/// are owner-only (0700): records hold command lines and working
/// directories, logs hold service output that often includes secrets. On
/// Windows they inherit the ACL of the parent (the user profile).
pub(crate) fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Open options that create a missing file owner-only (0600 on Unix; the
/// parent's ACL on Windows). Add `write`/`append` and `truncate` as needed.
pub(crate) fn private_file() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts
}

/// Removes group and other access from an existing state directory (one
/// created by an older taba, or by hand). Best effort: a directory owned
/// by someone else is left as is, and the files taba creates in it are
/// still owner-only.
fn make_private(_root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(_root)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(_root, fs::Permissions::from_mode(0o700));
        }
    }
}

/// Writes to a temporary file and renames it, so lock-free readers see the
/// old or the new content, never a partial one.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    create_private_dir_all(path.parent().expect("record has parent"))?;
    let tmp = path.with_extension("json.tmp");
    let mut file = private_file().write(true).truncate(true).open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    // Durable before it replaces the old record, so a crash or power loss
    // leaves the old or the new content, not an empty file.
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn remove_file_and_empty_parent(path: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_lock_is_none_while_held_and_records_the_holder() {
        let dir = std::env::temp_dir().join(format!("taba-lock-{}", std::process::id()));
        let store = Store::at(dir.clone());
        let held = store.try_lock().unwrap().expect("free lock");
        assert_eq!(store.lock_holder(), Some(std::process::id()));
        assert!(store.try_lock().unwrap().is_none());
        drop(held);
        // Another test's fork can briefly share the descriptor before exec.
        assert!(crate::proc::wait_for(
            Duration::from_secs(5),
            Duration::from_millis(10),
            || store.try_lock().unwrap().is_some()
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_is_none_but_empty_or_damaged_is_unreadable() {
        let dir = std::env::temp_dir().join(format!("taba-unreadable-{}", std::process::id()));
        let store = Store::at(dir.clone());
        assert!(store.get("s", "n").unwrap().is_none());
        assert!(store.get_scope("s").unwrap().is_none());
        for (bytes, cause) in [
            (&b""[..], "empty file"),
            (b"  \n", "empty file"),
            (b"{\"scope\":", "EOF"),
        ] {
            let record = dir.join("services").join("s").join("n.json");
            fs::create_dir_all(record.parent().unwrap()).unwrap();
            fs::write(&record, bytes).unwrap();
            let e = store.get("s", "n").unwrap_err();
            let u = unreadable(&e).expect("typed error");
            assert_eq!(
                u.file,
                StateFile::Service {
                    scope: "s".into(),
                    name: "n".into()
                }
            );
            assert!(u.error.contains(cause), "{}", u.error);
            let (services, skipped) = store.scan(None).unwrap();
            assert!(services.is_empty());
            assert_eq!(skipped.len(), 1);
            assert_eq!(fs::read(&record).unwrap(), bytes, "left as it was");
        }
        let scope = dir.join("scopes").join("z.json");
        fs::create_dir_all(scope.parent().unwrap()).unwrap();
        fs::write(&scope, b"").unwrap();
        let e = store.get_scope("z").unwrap_err();
        assert_eq!(unreadable(&e).unwrap().file, StateFile::Scope("z".into()));
        assert!(store.list_scopes().unwrap().is_empty());
        assert!(take_skipped().iter().any(|m| m.contains("scope record z")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_json_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!("taba-write-{}", std::process::id()));
        let path = dir.join("a").join("b.json");
        write_json(&path, &serde_json::json!({"x": 1})).unwrap();
        write_json(&path, &serde_json::json!({"x": 2})).unwrap();
        let v: serde_json::Value = read_json(&path).unwrap().unwrap();
        assert_eq!(v["x"], 2);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
