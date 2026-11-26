//! `taba.toml`: a project's named services, read by `taba up` and
//! `taba down`. The file only supplies `run` arguments; scope, leases and
//! stop rules are those of `run` and `stop`. See docs/design-project-file.md.

use crate::config::parse_service_ttl_flag;
use crate::error::{Next, Result, TabaError};
use crate::ports::{PortSpec, parse_port_spec};
use crate::ready::{ReadySpec, parse_ready_spec};
use crate::state::validate_ident;
use serde_json::json;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "taba.toml";

/// Seconds `up` waits for a service's ready check when it sets no `wait`.
pub const DEFAULT_WAIT: f64 = 30.0;

/// One `[services.NAME]` table.
#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub command: Vec<String>,
    pub port: Option<PortSpec>,
    pub ready: Option<ReadySpec>,
    pub wait: f64,
    pub env: Vec<(String, String)>,
    /// Absolute: `cwd` resolved against the file's directory.
    pub cwd: PathBuf,
    pub service_ttl: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub path: PathBuf,
    pub services: Vec<ServiceDef>,
}

impl Project {
    /// The services `names` selects, in the order given, or every service
    /// in file order when `names` is empty. An unknown name is `E_USAGE`.
    pub fn select(&self, names: &[String]) -> Result<Vec<&ServiceDef>> {
        if names.is_empty() {
            return Ok(self.services.iter().collect());
        }
        let mut out: Vec<&ServiceDef> = Vec::new();
        for n in names {
            let Some(def) = self.services.iter().find(|s| &s.name == n) else {
                let known: Vec<&str> = self.services.iter().map(|s| s.name.as_str()).collect();
                return Err(TabaError::usage(format!(
                    "{} declares no service {n:?}; it declares: {}",
                    self.path.display(),
                    known.join(", ")
                ))
                .details(json!({ "path": self.path, "name": n, "services": known })));
            };
            if !out.iter().any(|d| d.name == def.name) {
                out.push(def);
            }
        }
        Ok(out)
    }
}

/// The nearest `taba.toml` in `start` or its ancestors.
pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(FILE_NAME))
        .find(|p| p.is_file())
}

pub fn no_project(start: &Path) -> TabaError {
    TabaError::new(
        "E_NO_PROJECT",
        format!(
            "no {FILE_NAME} in {} or any parent directory",
            start.display()
        ),
    )
    .details(json!({ "directory": start }))
    .next(Next::just(&format!(
        "create {FILE_NAME} at the project root with a [services.NAME] table per service"
    )))
    .next(Next::just("or start the service directly with taba run"))
}

/// Finds and reads the nearest `taba.toml` from `start`.
pub fn load_from(start: &Path) -> Result<Project> {
    let path = find(start).ok_or_else(|| no_project(start))?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        TabaError::new("E_CONFIG", format!("cannot read {}: {e}", path.display()))
            .details(json!({ "source": "project", "path": path }))
    })?;
    parse(&path, &text)
}

fn err(path: &Path, key: &str, message: impl Into<String>) -> TabaError {
    TabaError::new(
        "E_CONFIG",
        format!("{}: {key}: {}", path.display(), message.into()),
    )
    .details(json!({ "source": "project", "path": path, "key": key }))
}

/// Parses the text of `path` (a `taba.toml`).
pub fn parse(path: &Path, text: &str) -> Result<Project> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        let msg = e.message().to_string();
        let line = e
            .span()
            .map(|s| text[..s.start.min(text.len())].lines().count().max(1));
        let key = line.map_or("syntax".to_string(), |l| format!("line {l}"));
        err(path, &key, msg)
    })?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut services = Vec::new();
    for (key, value) in &table {
        if key != "services" {
            return Err(err(
                path,
                key,
                "unknown top-level key (only [services.NAME] tables are read)",
            ));
        }
        let toml::Value::Table(list) = value else {
            return Err(err(path, key, "must be a table of [services.NAME] tables"));
        };
        for (name, def) in list {
            services.push(parse_service(path, dir, name, def)?);
        }
    }
    if services.is_empty() {
        return Err(err(path, "services", "declares no services"));
    }
    Ok(Project {
        path: path.to_path_buf(),
        services,
    })
}

fn parse_service(path: &Path, dir: &Path, name: &str, value: &toml::Value) -> Result<ServiceDef> {
    let at = |k: &str| format!("services.{name}.{k}");
    validate_ident("name", name).map_err(|m| err(path, &format!("services.{name}"), m))?;
    let toml::Value::Table(t) = value else {
        return Err(err(path, &format!("services.{name}"), "must be a table"));
    };
    let mut command = None;
    let mut port = None;
    let mut ready = None;
    let mut wait = DEFAULT_WAIT;
    let mut env = Vec::new();
    let mut cwd = crate::util::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut service_ttl = None;
    for (k, v) in t {
        let key = at(k);
        let string = |v: &toml::Value| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| err(path, &key, "must be a string"))
        };
        match k.as_str() {
            "cmd" => command = Some(parse_cmd(path, &key, v)?),
            "port" => {
                let text = match v {
                    toml::Value::Integer(i) => i.to_string(),
                    toml::Value::String(s) => s.clone(),
                    _ => {
                        return Err(err(
                            path,
                            &key,
                            "must be a port number, \"auto\" or \"auto:START\"",
                        ));
                    }
                };
                port = Some(parse_port_spec(&text).map_err(|m| err(path, &key, m))?);
            }
            "ready" => ready = Some(parse_ready_spec(&string(v)?).map_err(|m| err(path, &key, m))?),
            "wait" => {
                let secs = match v {
                    toml::Value::Integer(i) => *i as f64,
                    toml::Value::Float(f) => *f,
                    _ => return Err(err(path, &key, "must be a number of seconds")),
                };
                if !(secs.is_finite() && secs > 0.0) {
                    return Err(err(path, &key, "must be a positive number of seconds"));
                }
                wait = secs;
            }
            "env" => {
                let toml::Value::Table(vars) = v else {
                    return Err(err(path, &key, "must be a table of NAME = \"value\""));
                };
                for (var, val) in vars {
                    let vkey = format!("{key}.{var}");
                    check_env_key(var).map_err(|m| err(path, &vkey, m))?;
                    let val = val
                        .as_str()
                        .ok_or_else(|| err(path, &vkey, "must be a string"))?;
                    env.push((var.clone(), val.to_string()));
                }
            }
            "cwd" => {
                let rel = string(v)?;
                let joined = dir.join(&rel);
                cwd = crate::util::canonicalize(&joined).map_err(|e| {
                    err(
                        path,
                        &key,
                        format!("{} is not a directory: {e}", joined.display()),
                    )
                })?;
                if !cwd.is_dir() {
                    return Err(err(
                        path,
                        &key,
                        format!("{} is not a directory", cwd.display()),
                    ));
                }
            }
            "service_ttl" => {
                service_ttl =
                    Some(parse_service_ttl_flag(&string(v)?).map_err(|m| err(path, &key, m))?)
            }
            _ => {
                return Err(err(
                    path,
                    &key,
                    "unknown key (known: cmd, port, ready, wait, env, cwd, service_ttl)",
                ));
            }
        }
    }
    let command = command.ok_or_else(|| err(path, &at("cmd"), "is required"))?;
    if let Some(r) = &ready
        && r.needs_port()
        && port.is_none()
    {
        return Err(err(
            path,
            &at("ready"),
            format!(
                "{} needs a port (use log:REGEX for a service without one)",
                r.name()
            ),
        ));
    }
    Ok(ServiceDef {
        name: name.to_string(),
        command,
        port,
        ready,
        wait,
        env,
        cwd,
        service_ttl,
    })
}

/// `cmd`: an array of strings (argv), or a string split on whitespace. A
/// string with quotes or shell syntax is refused: taba runs no shell.
fn parse_cmd(path: &Path, key: &str, v: &toml::Value) -> Result<Vec<String>> {
    let argv: Vec<String> = match v {
        toml::Value::Array(items) => items
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| err(path, key, "must be an array of strings"))
            })
            .collect::<Result<_>>()?,
        toml::Value::String(s) => {
            if s.chars().any(|c| "\"'`$|&;<>()\\*?".contains(c)) {
                return Err(err(
                    path,
                    key,
                    "has quotes or shell syntax; taba runs no shell, so write it as an array, \
                     such as [\"sh\", \"-c\", \"...\"]",
                ));
            }
            s.split_whitespace().map(str::to_string).collect()
        }
        _ => return Err(err(path, key, "must be an array of strings or a string")),
    };
    if argv.first().is_none_or(|p| p.is_empty()) {
        return Err(err(path, key, "is empty"));
    }
    Ok(argv)
}

/// Checks an environment variable name for `run --env` and taba.toml `env`.
pub fn check_env_key(key: &str) -> std::result::Result<(), String> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Err(format!("{key:?} is not a valid variable name"));
    }
    if matches!(key, "PORT" | "TABA_SCOPE" | "TABA_SERVICE") {
        return Err(format!("{key} is set by taba and cannot be overridden"));
    }
    Ok(())
}

/// Splits `run --env KEY=VALUE` arguments. Done here rather than by clap,
/// whose errors quote the rejected argument: an error names at most the
/// variable, never a value.
pub fn parse_env_args(args: &[String]) -> Result<Vec<(String, String)>> {
    args.iter()
        .enumerate()
        .map(|(i, s)| {
            let (k, v) = s.split_once('=').ok_or_else(|| {
                TabaError::usage(format!(
                    "--env takes KEY=VALUE; argument {} of --env has no '='",
                    i + 1
                ))
            })?;
            check_env_key(k).map_err(|m| TabaError::usage(format!("--env: {m}")))?;
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
}

/// The text form of a ready check, as `--ready` and taba.toml take it.
pub fn ready_text(r: &ReadySpec) -> String {
    match r {
        ReadySpec::Tcp => "tcp".into(),
        ReadySpec::Http(p) => format!("http:{p}"),
        ReadySpec::Log(re) => format!("log:{}", re.as_str()),
    }
}

/// What `run --save` asks to write. `wait`, `ready` and `service_ttl` are
/// `None` when the run did not give them: the file's value is kept.
#[derive(Clone)]
pub struct SaveRequest {
    pub name: String,
    pub command: Vec<String>,
    pub port: Option<PortSpec>,
    pub ready: Option<ReadySpec>,
    pub wait: Option<f64>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub service_ttl: Option<u64>,
}

/// A checked `run --save`, written by `write_save` once the run succeeded.
pub struct SavePlan {
    pub path: PathBuf,
    request: SaveRequest,
    force: bool,
    pub def: ServiceDef,
    /// `created` (new file), `added`, `updated` or `unchanged`.
    pub result: &'static str,
}

/// Where `run --save` writes: the taba.toml `taba up` would read from
/// `cwd`, else a new one at the worktree root, else in `cwd`.
pub fn save_path(cwd: &Path, worktree: Option<&Path>) -> PathBuf {
    find(cwd).unwrap_or_else(|| worktree.unwrap_or(cwd).join(FILE_NAME))
}

fn same_def(a: &ServiceDef, b: &ServiceDef) -> bool {
    let mut ae = a.env.clone();
    let mut be = b.env.clone();
    ae.sort();
    be.sort();
    a.command == b.command
        && a.port.as_ref().map(PortSpec::request) == b.port.as_ref().map(PortSpec::request)
        && a.ready.as_ref().map(ready_text) == b.ready.as_ref().map(ready_text)
        && a.wait == b.wait
        && ae == be
        && a.cwd == b.cwd
        && a.service_ttl == b.service_ttl
}

/// Checks a `run --save` before anything starts: reads the file (an invalid
/// one is `E_CONFIG`), and refuses to replace a different definition of the
/// same name without `force` (`E_CONFLICT`).
pub fn plan_save(path: PathBuf, req: SaveRequest, force: bool) -> Result<SavePlan> {
    let existing = if path.is_file() {
        let text = std::fs::read_to_string(&path).map_err(|e| {
            TabaError::new("E_CONFIG", format!("cannot read {}: {e}", path.display()))
                .details(json!({ "source": "project", "path": path }))
        })?;
        // An empty file is a valid place to add the first service.
        if text.trim().is_empty() {
            Some(None)
        } else {
            Some(
                parse(&path, &text)?
                    .services
                    .into_iter()
                    .find(|s| s.name == req.name),
            )
        }
    } else {
        None
    };
    let old = existing.as_ref().and_then(|e| e.as_ref());
    let request = req.clone();
    let def = ServiceDef {
        name: req.name,
        command: req.command,
        port: req.port,
        ready: req.ready.or_else(|| old.and_then(|o| o.ready.clone())),
        wait: req.wait.or(old.map(|o| o.wait)).unwrap_or(DEFAULT_WAIT),
        env: req.env,
        cwd: req.cwd,
        service_ttl: req.service_ttl.or(old.and_then(|o| o.service_ttl)),
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let result = match old {
        None if existing.is_none() => "created",
        None => "added",
        Some(o) if same_def(o, &def) => "unchanged",
        Some(o) if !force => {
            return Err(TabaError::new(
                "E_CONFLICT",
                format!(
                    "{} already defines services.{} differently; run again with --force to \
                     replace it",
                    path.display(),
                    def.name
                ),
            )
            .details(json!({
                "path": path,
                "name": def.name,
                "existing": render(o, dir, true),
                "requested": render(&def, dir, true),
            }))
            .next(Next::just("add --force to replace the saved definition"))
            .next(Next::just("or drop --save, or use another --name")));
        }
        Some(_) => "updated",
    };
    Ok(SavePlan {
        request,
        force,
        path,
        def,
        result,
    })
}

fn def_table(def: &ServiceDef, dir: &Path, hide_values: bool) -> toml_edit::Table {
    use toml_edit::{Array, InlineTable, Item, Table, value};
    let mut t = Table::new();
    let mut cmd = Array::new();
    for a in &def.command {
        cmd.push(a.as_str());
    }
    t.insert("cmd", value(cmd));
    match &def.port {
        Some(PortSpec::Fixed(p)) => {
            t.insert("port", value(i64::from(*p)));
        }
        Some(p) => {
            t.insert("port", value(p.request()));
        }
        None => {}
    }
    if let Some(r) = &def.ready {
        t.insert("ready", value(ready_text(r)));
    }
    if def.wait != DEFAULT_WAIT {
        if def.wait.fract() == 0.0 && def.wait < i64::MAX as f64 {
            t.insert("wait", value(def.wait as i64));
        } else {
            t.insert("wait", value(def.wait));
        }
    }
    if !def.env.is_empty() {
        let mut env = InlineTable::new();
        for (k, v) in &def.env {
            env.insert(
                k,
                (if hide_values { "<hidden>" } else { v.as_str() }).into(),
            );
        }
        t.insert("env", Item::Value(env.into()));
    }
    let base = crate::util::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if def.cwd != base {
        let rel = match def.cwd.strip_prefix(&base) {
            Ok(r) => r
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => def.cwd.to_string_lossy().into_owned(),
        };
        t.insert("cwd", value(rel));
    }
    if let Some(ttl) = def.service_ttl {
        t.insert("service_ttl", value(crate::config::format_duration(ttl)));
    }
    t
}

/// The `[services.NAME]` table as TOML text; env values hidden when asked.
pub fn render(def: &ServiceDef, dir: &Path, hide_values: bool) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    let mut services = toml_edit::Table::new();
    services.set_implicit(true);
    services.insert(
        &def.name,
        toml_edit::Item::Table(def_table(def, dir, hide_values)),
    );
    doc.insert("services", toml_edit::Item::Table(services));
    doc.to_string()
}

/// Writes a checked plan, keeping the rest of the file (comments included).
pub fn write_save(plan: &SavePlan) -> Result<SavePlan> {
    use std::io::Write;

    let path = &plan.path;
    let io_err = |e: std::io::Error| {
        TabaError::new("E_CONFIG", format!("cannot write {}: {e}", path.display()))
            .details(json!({ "source": "project", "path": path }))
    };
    // Lock a stable sibling, not the config inode (which rename replaces).
    // Keep the lock file: unlinking it would let concurrent writers lock
    // different inodes. This also serializes callers with different TABA_HOME.
    let lock = crate::state::private_file()
        .write(true)
        .truncate(false)
        .open(path.with_extension("toml.taba-lock"))
        .map_err(io_err)?;
    lock.lock().map_err(io_err)?;
    // A service can take time to start. Recheck conflicts and inherited
    // defaults against the latest file, even for a formerly unchanged plan.
    let plan = plan_save(path.clone(), plan.request.clone(), plan.force)?;
    if plan.result == "unchanged" {
        return Ok(plan);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err(e)),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        // Only the message: the full text quotes the line, which may hold
        // an env value.
        .map_err(|e: toml_edit::TomlError| err(path, "syntax", e.message().to_string()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let services = doc
        .entry("services")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(|| {
            err(
                path,
                "services",
                "must be a table of [services.NAME] tables",
            )
        })?;
    let mut table = def_table(&plan.def, dir, false);
    if let Some(old) = services.get(&plan.def.name).and_then(|i| i.as_table()) {
        // Keep the comment above the replaced table.
        table.decor_mut().clone_from(old.decor());
        table.set_position(old.position().unwrap_or(0));
    }
    services.insert(&plan.def.name, toml_edit::Item::Table(table));
    // NamedTempFile uses exclusive creation and owner-only Unix permissions;
    // never truncate a predictable path that could be a preexisting symlink.
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io_err)?;
    match std::fs::metadata(path) {
        Ok(meta) => tmp
            .as_file()
            .set_permissions(meta.permissions())
            .map_err(io_err)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(e)),
    }
    tmp.write_all(doc.to_string().as_bytes()).map_err(io_err)?;
    tmp.as_file().sync_all().map_err(io_err)?;
    tmp.persist(path).map_err(|e| io_err(e.error))?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(dir: &Path, name: &str) -> SaveRequest {
        SaveRequest {
            name: name.into(),
            command: vec!["true".into()],
            port: None,
            ready: None,
            wait: None,
            env: vec![("TOKEN".into(), "dummy-save-secret".into())],
            cwd: crate::util::canonicalize(dir).unwrap(),
            service_ttl: None,
        }
    }

    #[test]
    fn stale_new_file_plans_merge_and_report_latest_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let a = plan_save(path.clone(), request(dir.path(), "a"), false).unwrap();
        let b = plan_save(path.clone(), request(dir.path(), "b"), false).unwrap();
        assert_eq!(write_save(&a).unwrap().result, "created");
        assert_eq!(write_save(&b).unwrap().result, "added");
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(p(&text).unwrap().services.len(), 2);
    }

    #[test]
    fn concurrent_saves_preserve_every_service() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let plans: Vec<_> = (0..8)
            .map(|i| {
                plan_save(path.clone(), request(dir.path(), &format!("svc{i}")), false).unwrap()
            })
            .collect();
        let barrier = std::sync::Barrier::new(plans.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = plans
                .iter()
                .map(|plan| {
                    scope.spawn(|| {
                        barrier.wait();
                        write_save(plan).unwrap();
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(p(&text).unwrap().services.len(), plans.len());
    }

    #[test]
    fn save_rechecks_conflicts_invalid_files_and_unchanged_plans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let initial = plan_save(path.clone(), request(dir.path(), "a"), false).unwrap();
        write_save(&initial).unwrap();
        let unchanged = plan_save(path.clone(), request(dir.path(), "a"), false).unwrap();
        assert_eq!(unchanged.result, "unchanged");
        std::fs::write(&path, "[services.a]\ncmd = [\"different\"]\n").unwrap();
        assert_eq!(write_save(&unchanged).err().unwrap().code, "E_CONFLICT");
        assert_eq!(write_save(&initial).err().unwrap().code, "E_CONFLICT");
        std::fs::write(&path, "[services.a]\ncmd = 123\n").unwrap();
        assert_eq!(write_save(&initial).err().unwrap().code, "E_CONFIG");
    }

    #[test]
    fn save_keeps_latest_omitted_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let plan = plan_save(path.clone(), request(dir.path(), "a"), true).unwrap();
        std::fs::write(&path, "[services.a]\ncmd = [\"true\"]\nwait = 99\nready = \"log:ready\"\nservice_ttl = \"1h\"\n").unwrap();
        let saved = write_save(&plan).unwrap();
        assert_eq!(saved.def.wait, 99.0);
        assert_eq!(saved.def.service_ttl, Some(3600));
        assert_eq!(
            saved.def.ready.as_ref().map(ready_text).as_deref(),
            Some("log:ready")
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_private_file_preserves_mode_and_ignores_predictable_temp() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let victim = dir.path().join("untouched");
        std::fs::write(&victim, "do not overwrite").unwrap();
        symlink(&victim, path.with_extension("toml.taba-tmp")).unwrap();
        let plan = plan_save(path.clone(), request(dir.path(), "a"), false).unwrap();
        write_save(&plan).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let plan = plan_save(path.clone(), request(dir.path(), "b"), false).unwrap();
        write_save(&plan).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let plan = plan_save(path.clone(), request(dir.path(), "c"), false).unwrap();
        write_save(&plan).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "do not overwrite");
    }

    fn p(text: &str) -> Result<Project> {
        let dir = std::env::temp_dir();
        parse(&dir.join(FILE_NAME), text)
    }

    #[test]
    fn parses_services_in_file_order() {
        let pr = p(r#"
[services.web]
cmd = ["npm", "run", "dev", "--", "--port", "{port}"]
port = "auto"
ready = "http:/"
wait = 60
env = { NODE_ENV = "development" }

[services.db]
cmd = "postgres -D data"
port = 5432

[services.worker]
cmd = ["python3", "worker.py"]
ready = "log:started"
service_ttl = "2h"
"#)
        .unwrap();
        let names: Vec<_> = pr.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["web", "db", "worker"]);
        let web = &pr.services[0];
        assert_eq!(web.wait, 60.0);
        assert_eq!(
            web.env,
            [("NODE_ENV".to_string(), "development".to_string())]
        );
        assert!(matches!(web.port, Some(PortSpec::Auto { .. })));
        assert_eq!(pr.services[1].command, ["postgres", "-D", "data"]);
        assert_eq!(pr.services[1].port, Some(PortSpec::Fixed(5432)));
        assert_eq!(pr.services[1].wait, DEFAULT_WAIT);
        assert_eq!(pr.services[2].service_ttl, Some(7200));
        let sel = pr
            .select(&["worker".into(), "web".into(), "worker".into()])
            .unwrap();
        let names: Vec<_> = sel.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["worker", "web"]);
        assert_eq!(pr.select(&["nope".into()]).unwrap_err().code, "E_USAGE");
    }

    #[test]
    fn rejects_bad_files() {
        let cases = [
            ("x = 1", "unknown top-level"),
            ("[services.web]\nport = 1", "cmd: is required"),
            ("[services.web]\ncmd = \"a | b\"", "shell syntax"),
            ("[services.web]\ncmd = []", "is empty"),
            (
                "[services.web]\ncmd = \"a\"\nready = \"tcp\"",
                "needs a port",
            ),
            ("[services.web]\ncmd = \"a\"\nport = \"soon\"", "port"),
            ("[services.web]\ncmd = \"a\"\nwait = 0", "positive"),
            (
                "[services.web]\ncmd = \"a\"\nenv = { PORT = \"1\" }",
                "set by taba",
            ),
            ("[services.web]\ncmd = \"a\"\nrestart = true", "unknown key"),
            (
                "[services.web]\ncmd = \"a\"\ncwd = \"no-such-dir-xyz\"",
                "not a directory",
            ),
            ("[services.\"a b\"]\ncmd = \"a\"", "invalid name"),
            ("[services]", "declares no services"),
            ("[services.web\ncmd = 1", "line 1"),
        ];
        for (text, want) in cases {
            let e = p(text).unwrap_err();
            assert_eq!(e.code, "E_CONFIG", "{text}");
            assert!(e.message.contains(want), "{text}: {}", e.message);
        }
    }

    #[test]
    fn finds_nearest_file_upwards() {
        let root = std::env::temp_dir().join(format!("taba-proj-{}", std::process::id()));
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(find(&deep).is_none_or(|f| !f.starts_with(&root)));
        std::fs::write(root.join(FILE_NAME), "[services.x]\ncmd = \"true\"\n").unwrap();
        assert_eq!(find(&deep), Some(root.join(FILE_NAME)));
        std::fs::write(root.join("a").join(FILE_NAME), "").unwrap();
        assert_eq!(find(&deep), Some(root.join("a").join(FILE_NAME)));
        let _ = std::fs::remove_dir_all(&root);
    }
}
