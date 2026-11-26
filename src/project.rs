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
                    if var.is_empty() || var.contains('=') || var.contains('\0') {
                        return Err(err(path, &vkey, "is not a valid variable name"));
                    }
                    if matches!(var.as_str(), "PORT" | "TABA_SCOPE" | "TABA_SERVICE") {
                        return Err(err(path, &vkey, "is set by taba and cannot be overridden"));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
