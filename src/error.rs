//! Structured errors and the JSON envelope shared by all commands.
//!
//! Every error has a stable code (`E_...`), a one-line message, optional
//! details and a list of next steps. The exit code follows from the code
//! (design v0.2, section 4, "Exit codes").

use serde_json::{Map, Value, json};
use std::io;

#[derive(Debug, Clone)]
pub struct TabaError {
    pub code: &'static str,
    pub message: String,
    pub details: Option<Box<Value>>,
    pub next: Vec<Next>,
}

/// One suggested next action. `command` is set when there is one to run.
#[derive(Debug, Clone)]
pub struct Next {
    pub what: String,
    pub command: Option<String>,
}

impl Next {
    pub fn run(what: &str, command: impl Into<String>) -> Next {
        Next {
            what: what.to_string(),
            command: Some(command.into()),
        }
    }

    pub fn just(what: &str) -> Next {
        Next {
            what: what.to_string(),
            command: None,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("do".into(), json!(self.what));
        if let Some(c) = &self.command {
            m.insert("command".into(), json!(c));
        }
        Value::Object(m)
    }
}

pub type Result<T> = std::result::Result<T, TabaError>;

impl TabaError {
    pub fn new(code: &'static str, message: impl Into<String>) -> TabaError {
        TabaError {
            code,
            message: message.into(),
            details: None,
            next: Vec::new(),
        }
    }

    pub fn details(mut self, details: Value) -> TabaError {
        self.details = Some(Box::new(details));
        self
    }

    pub fn next(mut self, next: Next) -> TabaError {
        self.next.push(next);
        self
    }

    pub fn usage(message: impl Into<String>) -> TabaError {
        TabaError::new("E_USAGE", message)
    }

    pub fn state(message: impl Into<String>) -> TabaError {
        TabaError::new("E_STATE", message)
    }

    /// Exit status for this error's code.
    pub fn exit_code(&self) -> u8 {
        exit_code(self.code)
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code));
        m.insert("message".into(), json!(self.message));
        if let Some(d) = &self.details {
            m.insert("details".into(), (**d).clone());
        }
        m.insert(
            "next".into(),
            Value::Array(self.next.iter().map(Next::to_json).collect()),
        );
        Value::Object(m)
    }
}

impl From<io::Error> for TabaError {
    fn from(e: io::Error) -> TabaError {
        use crate::state::StateFile;
        let Some(u) = crate::state::unreadable(&e) else {
            return TabaError::state(e.to_string()).next(Next::just(
                "check that $TABA_HOME is readable and writable and has free space, then retry",
            ));
        };
        let (what, next) = match &u.file {
            StateFile::Service { scope, name } => (
                format!("service record {scope}/{name}"),
                Next::run(
                    "remove the unreadable record and its log (a process it started is no longer tracked)",
                    format!("taba rm {scope}/{name} --force --reason 'unreadable record'"),
                ),
            ),
            StateFile::Scope(scope) => (
                format!("scope record {scope}"),
                Next::run(
                    "remove the unreadable scope record (its lease is lost)",
                    format!("taba rm --scope {scope} --force --reason 'unreadable scope record'"),
                ),
            ),
            StateFile::Other => (
                "state file".to_string(),
                Next::just("inspect or move the file aside; taba does not replace damaged state"),
            ),
        };
        TabaError::state(format!("unreadable {what}: {u}"))
            .details(json!({ "path": u.path, "reason": "unreadable", "cause": u.error }))
            .next(next)
    }
}

/// `W_STATE_SKIPPED` for each unreadable state file a listing skipped.
pub fn skipped_warnings() -> Vec<Warning> {
    crate::state::take_skipped()
        .into_iter()
        .map(|message| Warning {
            code: "W_STATE_SKIPPED",
            message,
        })
        .collect()
}

pub fn exit_code(code: &str) -> u8 {
    match code {
        "E_USAGE" | "E_NO_SCOPE" | "E_INVALID_IDENT" | "E_FORCE_NEEDS_REASON" => 2,
        "E_NOT_OWNER" | "E_SCOPE_HELD" | "E_LEASE_LIVE" | "E_LOCKED" | "E_BUSY" | "E_BORROWED"
        | "E_NO_CLAIMANT" | "E_CONFLICT" | "E_STILL_RUNNING" => 3,
        "E_PORT_IN_USE" | "E_PORT_CLAIMED" | "E_NO_FREE_PORT" | "E_START_FAILED"
        | "E_NOT_READY" => 4,
        "E_NOT_FOUND" | "E_DANGLING" => 5,
        _ => 1,
    }
}

/// A non-fatal warning (`W_...`), shown in `warnings[]` or on stderr.
#[derive(Debug, Clone)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
}

impl Warning {
    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

/// What a command produced. `body` holds the command's JSON fields (without
/// `ok`, `warnings` and `error`); `text` is the human-readable form.
/// `failure` is set when the command did part of its work but must still
/// report an error (for example `stop --scope` with one refused service).
pub struct Output {
    pub body: Map<String, Value>,
    pub text: String,
    pub warnings: Vec<Warning>,
    pub failure: Option<TabaError>,
}

impl Output {
    pub fn new(body: Value, text: String) -> Output {
        let body = match body {
            Value::Object(m) => m,
            other => {
                let mut m = Map::new();
                m.insert("result".into(), other);
                m
            }
        };
        Output {
            body,
            text,
            warnings: Vec::new(),
            failure: None,
        }
    }
}

/// Prints a finished command and returns its exit status.
pub fn emit(json_mode: bool, res: Result<Output>) -> u8 {
    let skipped = skipped_warnings();
    match res {
        Ok(mut out) => {
            out.warnings.extend(skipped);
            let code = out.failure.as_ref().map_or(0, TabaError::exit_code);
            if json_mode {
                let mut m = Map::new();
                m.insert("ok".into(), json!(out.failure.is_none()));
                m.extend(out.body);
                if let Some(f) = &out.failure {
                    m.insert("error".into(), f.to_json());
                }
                m.insert(
                    "warnings".into(),
                    Value::Array(out.warnings.iter().map(Warning::to_json).collect()),
                );
                print_json(&Value::Object(m));
            } else {
                if !out.text.is_empty() {
                    println!("{}", out.text);
                }
                for w in &out.warnings {
                    eprintln!("taba: warning: {}", w.message);
                }
                if let Some(f) = &out.failure {
                    eprintln!("taba: {}", f.message);
                }
            }
            code
        }
        Err(e) => {
            if json_mode {
                print_json(&json!({
                    "ok": false,
                    "error": e.to_json(),
                    "warnings": skipped.iter().map(Warning::to_json).collect::<Vec<_>>(),
                }));
            } else {
                for w in &skipped {
                    eprintln!("taba: warning: {}", w.message);
                }
                eprintln!("taba: {}", e.message);
                for n in &e.next {
                    match &n.command {
                        Some(c) => eprintln!("  next: {}: {c}", n.what),
                        None => eprintln!("  next: {}", n.what),
                    }
                }
            }
            e.exit_code()
        }
    }
}

pub fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("JSON values always serialize")
    );
}
