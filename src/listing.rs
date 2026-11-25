//! `taba ps --all`: every listening TCP port of this user, managed or not.
//!
//! It sends no signals and never changes service records or leases. On
//! Linux, process inspection may initialize or repair the boot-time cache.

use crate::error::{Output, Result, Warning};
use crate::proc;
use crate::scope;
use crate::state::{Service, Store};
use crate::util::{render_table, tilde};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

struct Row {
    port: u16,
    address: String,
    pid: i32,
    pgid: Option<i32>,
    command: Option<String>,
    directory: Option<PathBuf>,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    service: Option<Service>,
}

/// Command line, working directory and managing service of one pid.
type PidFacts = (Option<String>, Option<PathBuf>, Option<Service>);

/// The taba service whose process group / job holds `pid`, if any.
fn managed_by(pid: i32, running: &[Service]) -> Option<Service> {
    running
        .iter()
        .find(|s| proc::in_group(pid, s.pid, s.pgid, s.start_time))
        .cloned()
}

pub fn cmd_ps_all() -> Result<Output> {
    let store = Store::open()?;
    let running: Vec<Service> = store
        .list(None)?
        .into_iter()
        .filter(|s| s.is_running())
        .collect();
    let mut sockets = proc::all_listeners();
    sockets.sort_by(|a, b| (a.port, &a.address, a.pid).cmp(&(b.port, &b.address, b.pid)));
    sockets.dedup();

    // Facts per pid are looked up once.
    let mut per_pid: HashMap<i32, PidFacts> = HashMap::new();
    let mut rows = Vec::new();
    for s in sockets {
        let (command, directory, service) = per_pid
            .entry(s.pid)
            .or_insert_with(|| {
                (
                    crate::ports::command_line_full(s.pid),
                    proc::cwd(s.pid),
                    managed_by(s.pid, &running),
                )
            })
            .clone();
        let worktree = directory.as_deref().and_then(scope::find_worktree);
        let branch = worktree.as_deref().and_then(scope::read_branch);
        rows.push(Row {
            port: s.port,
            address: s.address,
            pid: s.pid,
            pgid: proc::pgid(s.pid),
            command,
            directory,
            worktree,
            branch,
            service,
        });
    }

    let listeners: Vec<Value> = rows
        .iter()
        .map(|r| {
            let holder = r
                .service
                .as_ref()
                .map(|s| store.get_scope(&s.scope))
                .transpose()?
                .flatten();
            let git = r
                .worktree
                .as_ref()
                .map(|w| json!({ "worktree": w, "branch": r.branch }));
            Ok(json!({
                "port": r.port,
                "address": r.address,
                "pid": r.pid,
                "pgid": r.pgid,
                "command": r.command,
                "directory": r.directory,
                "git": git,
                "managed": r.service.is_some(),
                "service": r.service.as_ref().map(Service::id),
                "scope": r.service.as_ref().map(|s| s.scope.clone()),
                "holder": holder.as_ref().map(|h| &h.holder.agent),
                "lease": holder.as_ref().map(|h| h.lease_json(crate::util::now_secs())),
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut table = vec![[
        "PORT".to_string(),
        "MANAGED".to_string(),
        "SERVICE/PID".to_string(),
        "BRANCH".to_string(),
        "DIRECTORY".to_string(),
        "COMMAND".to_string(),
    ]];
    for r in &rows {
        table.push([
            if r.address == "*" {
                r.port.to_string()
            } else {
                format!("{}:{}", r.address, r.port)
            },
            if r.service.is_some() { "yes" } else { "no" }.to_string(),
            r.service
                .as_ref()
                .map_or_else(|| r.pid.to_string(), Service::id),
            r.branch.clone().unwrap_or_else(|| "-".into()),
            r.directory.as_deref().map_or_else(|| "-".into(), tilde),
            r.command.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    let text = if rows.is_empty() {
        "no listening TCP ports".to_string()
    } else {
        render_table(&table)
    };
    let mut out = Output::new(json!({ "listeners": listeners }), text);
    if !cfg!(windows) && !cfg!(target_os = "linux") && !lsof_available() {
        out.warnings.push(Warning {
            code: "W_NO_LSOF",
            message: "lsof is not available, so no listeners could be listed".into(),
        });
    }
    Ok(out)
}

fn lsof_available() -> bool {
    std::process::Command::new("lsof")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
