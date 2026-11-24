//! Acceptance tests for milestone M2 of docs/design-v0.2.md (section 8):
//! JSON everywhere and `ps --all`.

mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;

/// One JSON document on stdout, nothing on stderr.
fn one_document(out: &std::process::Output) -> Value {
    assert!(out.stderr.is_empty(), "stderr is not empty: {}", text(out));
    json(out)
}

/// M2.1: every command with `--json` prints exactly one JSON document on
/// stdout, for success and failure, and stderr is empty.
#[test]
fn m2_01_json_everywhere() {
    let f = Fixture::new("json");
    let a = &f.wt_a;
    let ok = |args: &[&str]| {
        let out = f.taba(a, args);
        let v = one_document(&out);
        assert_eq!(v["ok"], true, "{args:?}: {v}");
        assert_eq!(exit(&out), 0, "{args:?}: {v}");
        v
    };
    let fails = |args: &[&str], code: &str| {
        let out = f.taba(a, args);
        let v = one_document(&out);
        assert_eq!(v["ok"], false, "{args:?}: {v}");
        assert_eq!(v["error"]["code"], code, "{args:?}: {v}");
        assert_ne!(exit(&out), 0);
        v
    };

    let v = ok(&run_auto("web"));
    let svc = v["service"].as_str().unwrap().to_string();
    ok(&["whoami", "--json"]);
    let ps = ok(&["ps", "--json"]);
    assert_eq!(ps["services"][0]["service"], svc.as_str());
    assert!(ps["warnings"].is_array());
    ok(&["ps", "--all", "--json"]);
    let logs = ok(&["logs", "web", "--json"]);
    assert!(
        logs["log"].as_str().unwrap().contains("--- taba:"),
        "{logs}"
    );
    assert_eq!(logs["service"], svc.as_str());
    let stop = ok(&["stop", "web", "--json"]);
    assert_eq!(stop["results"][0]["signal"], "TERM");
    let action = if cfg!(windows) {
        "ctrl_break"
    } else {
        "sigterm"
    };
    assert_eq!(stop["results"][0]["platform_action"], action);
    let rm = ok(&["rm", "web", "--json"]);
    assert_eq!(rm["results"][0]["result"], "removed");

    fails(&["logs", "nope", "--json"], "E_NOT_FOUND");
    fails(&["rm", "nope", "--json"], "E_NOT_FOUND");
    fails(&["stop", "nope", "--json"], "E_NOT_FOUND");
    fails(&["ps", "--scope", "bad scope", "--json"], "E_INVALID_IDENT");
    fails(&["run", "--json", "--name", "x"], "E_USAGE");
    fails(&["ps", "--all", "--running", "--json"], "E_USAGE");
    fails(&["logs", "-f", "web", "--json"], "E_USAGE");

    // TABA_OUTPUT=json does the same without the flag.
    let out = f
        .cmd(a, &["logs", "nope"])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(one_document(&out)["error"]["code"], "E_NOT_FOUND");
}

/// Starts an unmanaged `python3 -m http.server PORT` in `dir`, detached the
/// way a human would (`nohup ... &` on Unix).
fn start_unmanaged(dir: &Path, port: u16) -> Child {
    let p = port.to_string();
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new(PY);
        c.args(["-m", "http.server", &p, "--bind", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("nohup");
        c.args([PY, "-m", "http.server", &p, "--bind", "127.0.0.1"]);
        c
    };
    let child = cmd
        .current_dir(dir)
        .env("PYTHONPATH", python_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("unmanaged server", || http_ok(port));
    child
}

/// M2.2: `ps --all --json` lists a taba service (managed, with its scope)
/// and an unmanaged server started in `wt-b` (not managed, with its
/// directory and git branch).
#[test]
fn m2_02_ps_all_lists_managed_and_unmanaged() {
    let f = Fixture::new("psall");
    let v = json(&f.taba(&f.wt_a, &run_auto("web")));
    let managed_port = port_of(&v);
    let scope_a = f.scope_of(&f.wt_a);
    let port = (20000..30000).find(|p| port_free(*p)).unwrap();
    let mut holder = start_unmanaged(&f.wt_b, port);

    let out = f.taba(&f.outside, &["ps", "--all", "--json"]);
    let v = one_document(&out);
    let rows = v["listeners"].as_array().unwrap();
    let row = |p: u16| {
        rows.iter()
            .find(|r| r["port"] == p)
            .unwrap_or_else(|| panic!("port {p} not listed: {v}"))
    };
    let m = row(managed_port);
    assert_eq!(m["managed"], true, "{m}");
    assert_eq!(m["scope"], scope_a.as_str());
    assert_eq!(m["service"], format!("{scope_a}/web"));
    assert_eq!(m["directory"], f.wt_a.to_str().unwrap());
    assert_eq!(m["git"]["branch"], "a");

    let u = row(port);
    assert_eq!(u["managed"], false, "{u}");
    assert!(u["service"].is_null() && u["scope"].is_null());
    assert_eq!(u["directory"], f.wt_b.to_str().unwrap(), "{u}");
    assert_eq!(u["git"]["worktree"], f.wt_b.to_str().unwrap());
    assert_eq!(u["git"]["branch"], "b");
    assert!(
        u["command"]
            .as_str()
            .unwrap()
            .contains(&format!("http.server {port}")),
        "{u}"
    );

    // The text form has the documented columns.
    let out = f.taba(&f.outside, &["ps", "--all"]);
    let table = String::from_utf8_lossy(&out.stdout).to_string();
    for col in [
        "PORT",
        "MANAGED",
        "SERVICE/PID",
        "BRANCH",
        "DIRECTORY",
        "COMMAND",
    ] {
        assert!(table.lines().next().unwrap().contains(col), "{table}");
    }
    let _ = holder.kill();
    let _ = holder.wait();
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

/// M2.3: `ps --all` signals nothing and writes nothing.
#[test]
fn m2_03_ps_all_is_read_only() {
    let f = Fixture::new("readonly");
    let v = json(&f.taba(&f.wt_a, &run_auto("web")));
    let port = port_of(&v);
    let home = f.home();
    let watched = [
        home.clone(),
        home.join("services"),
        home.join("logs"),
        home.join("events.jsonl"),
    ];
    // events.jsonl may not exist yet; create the list of what does.
    let watched: Vec<_> = watched.into_iter().filter(|p| p.exists()).collect();
    let before: Vec<_> = watched.iter().map(|p| mtime(p)).collect();
    std::thread::sleep(std::time::Duration::from_millis(50));
    for dir in [&f.wt_a, &f.wt_b, &f.outside] {
        let out = f.taba(dir, &["ps", "--all", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
    }
    let after: Vec<_> = watched.iter().map(|p| mtime(p)).collect();
    assert_eq!(before, after, "ps --all wrote to TABA_HOME");
    assert!(http_ok(port), "ps --all affected the server");
    assert_eq!(f.ps(&[])[0]["status"], "running");
}
