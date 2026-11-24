//! Acceptance tests for milestone M1 of docs/design-v0.2.md (section 8):
//! `run --wait`, `--port auto`, cross-scope stop refusal, and what they need
//! (scope derived from the git worktree, `whoami`, JSON envelopes, the
//! operation lock for `stop`).
//!
//! Each test gets a throwaway TABA_HOME and a throwaway git repository with
//! two worktrees, `wt-a` and `wt-b`, plus a directory outside any repository.
//! Tests that use `--port auto` share the 4100-4999 range across separate
//! TABA_HOMEs, which do not see each other's reservations, so every test in
//! this file holds `SERIAL` while it runs.

mod common;
use common::*;
use serde_json::Value;
use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;

/// M1.1: `run --port auto --wait --json` in a worktree returns a URL in
/// 4100-4999 that answers right away, under the scope derived from the
/// worktree folder.
#[test]
fn m1_01_port_auto_wait_returns_a_ready_url() {
    let f = Fixture::new("auto");
    let out = f.taba(&f.wt_a, &run_auto("web"));
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "started");
    let port = port_of(&v);
    assert!((4100..=4999).contains(&port), "{v}");
    assert_eq!(v["url"], format!("http://localhost:{port}"));
    assert!(http_ok(port), "server did not answer right after --wait");
    assert_eq!(v["ready"]["check"], "tcp");
    assert!(v["ready"]["ms"].is_u64());
    assert_eq!(v["warnings"], serde_json::json!([]));

    let scope = f.scope_of(&f.wt_a);
    assert!(
        scope.starts_with("wt-a-") && scope.len() == "wt-a-".len() + 4,
        "{scope}"
    );
    assert_eq!(v["scope"], scope);
    assert_eq!(v["service"], format!("{scope}/web"));
    assert_eq!(v["directory"], f.wt_a.to_str().unwrap());
    let start_time = &f.ps(&[])[0]["start_time"];
    assert_eq!(v["instance"], format!("{}-{start_time}", v["pid"]));
}

/// M1.2: ten parallel `--port auto` runs get ten distinct ports, and all ten
/// answer.
#[test]
fn m1_02_parallel_auto_runs_get_distinct_ports() {
    let f = Fixture::new("parallel");
    let names: Vec<String> = (0..10).map(|i| format!("web{i}")).collect();
    let children: Vec<Child> = names
        .iter()
        .map(|n| {
            f.cmd(&f.wt_a, &run_auto(n))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ports = Vec::new();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert_eq!(exit(&out), 0, "{}", text(&out));
        ports.push(port_of(&json(&out)));
    }
    let mut unique = ports.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 10, "ports: {ports:?}");
    for p in &ports {
        assert!((4100..=4999).contains(p));
        assert!(http_ok(*p), "port {p} does not answer");
    }
    assert_eq!(f.ps(&["--running"]).len(), 10);
}

/// M1.3: re-running the same `--port auto` command returns
/// `already_running` with the same port and instance.
#[test]
fn m1_03_rerun_with_port_auto_is_idempotent() {
    let f = Fixture::new("rerun");
    let first = json(&f.taba(&f.wt_a, &run_auto("web")));
    assert_eq!(first["status"], "started");
    let out = f.taba(&f.wt_a, &run_auto("web"));
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let again = json(&out);
    assert_eq!(again["status"], "already_running");
    assert_eq!(again["port"], first["port"]);
    assert_eq!(again["instance"], first["instance"]);
    assert_eq!(again["url"], first["url"]);
    assert_eq!(f.ps(&[]).len(), 1);
}

/// M1.4: `--port auto:P` skips a port held by an unmanaged process;
/// `--port P` reports the unmanaged holder and never suggests `kill`.
#[test]
fn m1_04_unmanaged_holder_is_skipped_and_reported() {
    let f = Fixture::new("unmanaged");
    // The design uses 8001; take the first P >= 8001 with P and P+1 free.
    let p = (8001..9000)
        .find(|p| port_free(*p) && port_free(p + 1))
        .expect("two free ports");
    let holder = Unmanaged::start(p, &f.outside);

    let spec = format!("auto:{p}");
    let mut args = vec![
        "run", "--name", "web", "--port", &spec, "--wait", "--json", "--",
    ];
    args.extend_from_slice(&SERVER);
    let out = f.taba(&f.wt_a, &args);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    assert_eq!(port_of(&v), p + 1, "{v}");
    assert!(http_ok(p + 1));

    let ps = p.to_string();
    let mut args = vec!["run", "--name", "api", "--port", &ps, "--json", "--"];
    args.extend_from_slice(&SERVER);
    let out = f.taba(&f.wt_a, &args);
    assert_eq!(exit(&out), 4, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "E_PORT_IN_USE");
    assert_eq!(v["error"]["details"]["holder"]["kind"], "unmanaged");
    assert_eq!(v["error"]["details"]["holder"]["pid"], holder.0.id());
    let next = v["error"]["next"].to_string();
    assert!(next.contains("--port auto"), "{next}");
    assert!(next.contains("ask the human"), "{next}");
    assert!(!next.contains("kill"), "{next}");
    assert!(http_ok(p), "the unmanaged server was affected");
    // Nothing was started for `api`.
    assert_eq!(f.ps(&[]).len(), 1);
}

/// M1.5: a server that is not ready in time gives `E_NOT_READY` (exit 4) and
/// keeps running; a command that exits gives `E_START_FAILED` with the end of
/// its log.
#[test]
fn m1_05_not_ready_and_start_failed() {
    let f = Fixture::new("notready");
    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "slow",
            "--port",
            "auto",
            "--wait=2",
            "--json",
            "--",
            PY,
            "-c",
            "import runpy, sys, time; time.sleep(5); \
             sys.argv = ['http.server', '{port}', '--bind', '127.0.0.1']; \
             runpy.run_module('http.server', run_name='__main__')",
        ],
    );
    assert_eq!(exit(&out), 4, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_NOT_READY");
    assert_eq!(v["error"]["details"]["status"], "running");
    let scope = f.scope_of(&f.wt_a);
    let rows = f.ps(&[]);
    let row = rows
        .iter()
        .find(|r| r["service"] == format!("{scope}/slow"))
        .unwrap();
    assert_eq!(row["status"], "running", "{row}");

    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "bad",
            "--port",
            "auto",
            "--wait",
            "--json",
            "--",
            PY,
            "-c",
            "raise SystemExit(1)",
        ],
    );
    assert_eq!(exit(&out), 4, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_START_FAILED");
    assert!(v["error"]["details"]["log_tail"].is_array(), "{v}");

    // The log tail carries the program's own output.
    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "boom",
            "--wait",
            "--json",
            "--",
            PY,
            "-c",
            "import sys; print('boom-on-stderr', file=sys.stderr); sys.exit(3)",
        ],
    );
    assert_eq!(exit(&out), 4, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_START_FAILED");
    assert!(
        v["error"]["details"]["log_tail"]
            .to_string()
            .contains("boom-on-stderr"),
        "{v}"
    );
}

/// M1.6: `stop` of another scope's service is refused with `E_NOT_OWNER`
/// (exit 3) for agents and for humans without a scope; `--force --reason`
/// stops it and is recorded in events.jsonl.
#[test]
fn m1_06_cross_scope_stop_is_refused_unless_forced() {
    let f = Fixture::new("notowner");
    let v = json(&f.taba(&f.wt_a, &run_auto("web")));
    let port = port_of(&v);
    let target = v["service"].as_str().unwrap().to_string();
    let scope_b = f.scope_of(&f.wt_b);

    // An agent in wt-b.
    let out = f
        .cmd(&f.wt_b, &["stop", &target, "--json"])
        .env("TABA_AGENT", "codex:test-b")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 3, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_NOT_OWNER");
    assert_eq!(v["error"]["details"]["caller_scope"], scope_b);
    assert!(!v["error"]["next"].to_string().contains("--force"), "{v}");
    thread::sleep(Duration::from_millis(300));
    assert!(http_ok(port), "the server was stopped");

    let out = f
        .cmd(
            &f.wt_b,
            &["stop", &target, "--force", "--reason", "test", "--json"],
        )
        .env("TABA_AGENT", "codex:test-b")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["results"][0]["result"], "stopped");
    assert_eq!(v["results"][0]["forced"], true);
    wait_until("server to stop", || !http_ok(port));
    let ev = f.events();
    let e = ev.last().expect("an event");
    assert_eq!(e["event"], "force_stop");
    assert_eq!(e["target"], target);
    assert_eq!(e["reason"], "test");
    assert_eq!(e["agent"], "codex:test-b");
    assert_eq!(e["caller_scope"], scope_b);
    assert_eq!(e["cwd"], f.wt_b.to_str().unwrap());

    // A human (no TABA_AGENT) outside any repository: same rule.
    let v = json(&f.taba(&f.wt_a, &run_auto("web")));
    let port = port_of(&v);
    let out = f.taba(&f.outside, &["stop", &target, "--json"]);
    assert_eq!(exit(&out), 3, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_NOT_OWNER");
    assert!(v["error"]["details"]["caller_scope"].is_null());
    // Text mode says what the human can do.
    let out = f.taba(&f.outside, &["stop", &target]);
    assert_eq!(exit(&out), 3, "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force --reason"));
    let out = f.taba(&f.outside, &["stop", &target, "--force", "--json"]);
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert_eq!(json(&out)["error"]["code"], "E_FORCE_NEEDS_REASON");
    assert!(http_ok(port), "the server was stopped");

    let out = f.taba(
        &f.outside,
        &["stop", &target, "--force", "--reason", "test"],
    );
    assert_eq!(exit(&out), 0, "{}", text(&out));
    wait_until("server to stop", || !http_ok(port));
    let ev = f.events();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[1]["event"], "force_stop");
    assert_eq!(ev[1]["agent"], "human");
    assert!(ev[1]["caller_scope"].is_null());

    // rm follows the same rule.
    let out = f.taba(&f.wt_b, &["rm", &target]);
    assert_eq!(exit(&out), 3, "{}", text(&out));
    let out = f.taba(&f.wt_a, &["rm", "web"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
}

/// M1.7: two simultaneous stops of one service: one stops it, the other
/// gets `E_BUSY` or `not_running`, and SIGTERM (Windows: Ctrl+Break, which
/// Python sees as SIGBREAK) is delivered once.
#[test]
fn m1_07_concurrent_stops_signal_once() {
    let f = Fixture::new("busy");
    // Counts SIGTERMs in its log and takes a second to exit after the first.
    let script = "import signal, sys, time\n\
                  def term(*_):\n    print('got-TERM', flush=True)\n    time.sleep(1)\n    sys.exit(0)\n\
                  signal.signal(getattr(signal, 'SIGBREAK', signal.SIGTERM), term)\n\
                  print('ready', flush=True)\n\
                  while True:\n    time.sleep(0.05)\n";
    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "sig",
            "--wait",
            "--ready",
            "log:^ready$",
            "--json",
            "--",
            PY,
            "-c",
            script,
        ],
    );
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let target = json(&out)["service"].as_str().unwrap().to_string();

    let children: Vec<Child> = (0..2)
        .map(|_| {
            f.cmd(&f.wt_a, &["stop", &target, "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outs: Vec<Value> = children
        .into_iter()
        .map(|c| json(&c.wait_with_output().unwrap()))
        .collect();
    let outcome = |v: &Value| {
        if v["ok"] == true {
            v["results"][0]["result"].as_str().unwrap().to_string()
        } else {
            v["error"]["code"].as_str().unwrap().to_string()
        }
    };
    let mut got: Vec<String> = outs.iter().map(outcome).collect();
    got.sort();
    assert!(
        got == ["E_BUSY", "stopped"] || got == ["not_running", "stopped"],
        "{outs:?}"
    );
    let log = String::from_utf8_lossy(&f.taba(&f.wt_a, &["logs", "sig"]).stdout).to_string();
    assert_eq!(log.lines().filter(|l| *l == "got-TERM").count(), 1, "{log}");
    // The operation lock is gone afterwards.
    assert!(!f.home().join("ops").join(format!("{target}.json")).exists());
}

/// M1.8: no scope outside a repository without `--scope` / `TABA_SCOPE`
/// gives `E_NO_SCOPE`; with `TABA_SCOPE` v0.1 behaviour is unchanged.
#[test]
fn m1_08_no_scope_outside_a_repository() {
    let f = Fixture::new("noscope");
    let out = f.taba(
        &f.outside,
        &["run", "--name", "x", "--json", "--", PY, "-c", SLEEP_30],
    );
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert_eq!(json(&out)["error"]["code"], "E_NO_SCOPE");
    assert!(f.ps(&[]).is_empty());

    let out = f
        .cmd(
            &f.outside,
            &["run", "--name", "x", "--", PY, "-c", SLEEP_30],
        )
        .env("TABA_SCOPE", "x")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("started x/x\n"));
    let rows = f.ps(&[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["service"], "x/x");
    assert_eq!(rows[0]["status"], "running");
    // TABA_SCOPE also wins inside a worktree.
    let v = json(
        &f.cmd(&f.wt_a, &["whoami", "--json"])
            .env("TABA_SCOPE", "x")
            .output()
            .unwrap(),
    );
    assert_eq!(v["scope"], "x");
    assert_eq!(v["scope_source"], "env");
    let out = f
        .cmd(&f.outside, &["stop", "x"])
        .env("TABA_SCOPE", "x")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 0, "{}", text(&out));
}

/// M1.9: a v0.1 state directory (records without the v0.2 fields, no
/// `version` file) still lists, re-runs, stops and removes correctly.
#[test]
fn m1_09_v01_state_still_works() {
    let f = Fixture::new("v01");
    let port = (20000..30000).find(|p| port_free(*p)).unwrap();
    let ps = port.to_string();
    let cmd = [PY, "-m", "http.server", &ps, "--bind", "127.0.0.1"];
    let mut run = vec![
        "run", "--scope", "old", "--name", "web", "--port", &ps, "--",
    ];
    run.extend_from_slice(&cmd);
    let out = f.taba(&f.outside, &run);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    wait_until("server", || http_ok(port));

    // Rewrite the state as v0.1 wrote it.
    let record = f.home().join("services/old/web.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    let obj = v.as_object_mut().unwrap();
    let v01 = [
        "scope",
        "name",
        "command",
        "cwd",
        "port",
        "pid",
        "pgid",
        "start_time",
        "log_path",
    ];
    obj.retain(|k, _| v01.contains(&k.as_str()));
    assert_eq!(obj.len(), v01.len());
    let (pid, start) = (obj["pid"].clone(), obj["start_time"].clone());
    std::fs::write(&record, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    std::fs::remove_file(f.home().join("version")).unwrap();
    let _ = std::fs::remove_dir_all(f.home().join("ops"));

    let rows = f.ps(&[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "running");
    assert_eq!(rows[0]["instance"], format!("{pid}-{start}"));

    // Same command again: already running (compared like v0.1 did).
    let mut rerun = run.clone();
    rerun.insert(rerun.len() - cmd.len() - 1, "--json");
    let out = f.taba(&f.outside, &rerun);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_eq!(json(&out)["status"], "already_running");

    // Ownership applies to v0.1 records too.
    let out = f.taba(&f.outside, &["stop", "old/web"]);
    assert_eq!(exit(&out), 3, "{}", text(&out));

    let out = f.taba(&f.outside, &["stop", "--scope", "old", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_eq!(json(&out)["results"][0]["result"], "stopped");
    wait_until("server to stop", || !http_ok(port));
    assert_eq!(f.ps(&[])[0]["status"], "exited");

    let out = f.taba(&f.outside, &["rm", "--scope", "old"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert!(f.ps(&[]).is_empty());
    assert!(!f.home().join("logs/old/web.log").exists());
}

/// M1.10: one scope, many services: `web`, `api` and `worker` (no port) run
/// under the derived scope; `ps --scope` lists all three and `stop --scope`
/// stops all three.
#[test]
fn m1_10_one_scope_many_services() {
    let f = Fixture::new("many");
    let web = json(&f.taba(&f.wt_a, &run_auto("web")));
    let api = json(&f.taba(&f.wt_a, &run_auto("api")));
    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "worker",
            "--json",
            "--",
            PY,
            "-c",
            "import time; time.sleep(300)",
        ],
    );
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let worker = json(&out);
    assert!(worker["port"].is_null() && worker["url"].is_null());
    let scope = f.scope_of(&f.wt_a);
    for v in [&web, &api, &worker] {
        assert_eq!(v["scope"], scope);
    }
    // Something in wt-b does not show up under wt-a's scope.
    let out = f.taba(&f.wt_b, &run_auto("web"));
    assert_eq!(exit(&out), 0, "{}", text(&out));

    let rows = f.ps(&["--scope", &scope]);
    let mut names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["api", "web", "worker"]);
    assert!(rows.iter().all(|r| r["status"] == "running"));

    let who = json(&f.taba(&f.wt_a, &["whoami", "--json"]));
    assert_eq!(who["services"].as_array().unwrap().len(), 3);

    let out = f.taba(&f.wt_a, &["stop", "--scope", &scope, "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["result"] == "stopped"), "{v}");
    assert!(
        f.ps(&["--scope", &scope])
            .iter()
            .all(|r| r["status"] == "exited")
    );
    assert!(!http_ok(port_of(&web)) && !http_ok(port_of(&api)));
    let scope_b = f.scope_of(&f.wt_b);
    assert_eq!(f.ps(&["--scope", &scope_b])[0]["status"], "running");
}

/// `whoami` reports the scope and where it came from: git worktree (with
/// branch), TABA_SCOPE, --scope, or none.
#[test]
fn m1_whoami_reports_scope_and_source() {
    let f = Fixture::new("whoami");
    let a = json(&f.taba(&f.wt_a, &["whoami", "--json"]));
    assert_eq!(a["ok"], true);
    assert_eq!(a["scope_source"], "git");
    assert_eq!(a["worktree"], f.wt_a.to_str().unwrap());
    assert_eq!(a["branch"], "a");
    assert_eq!(a["agent"], "human");
    // A subdirectory belongs to the same scope; another worktree does not.
    let sub = f.wt_a.join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(
        json(&f.taba(&sub, &["whoami", "--json"]))["scope"],
        a["scope"]
    );
    let b = json(&f.taba(&f.wt_b, &["whoami", "--json"]));
    assert!(b["scope"].as_str().unwrap().starts_with("wt-b-"));
    assert_ne!(a["scope"], b["scope"]);
    assert_eq!(b["branch"], "b");
    // The branch is not part of the scope.
    git(&f.wt_a, &["switch", "-q", "-c", "other"]);
    let a2 = json(&f.taba(&f.wt_a, &["whoami", "--json"]));
    assert_eq!(a2["scope"], a["scope"]);
    assert_eq!(a2["branch"], "other");
    // The main checkout is a worktree too.
    let m = json(&f.taba(&f.root.join("repo"), &["whoami", "--json"]));
    assert!(m["scope"].as_str().unwrap().starts_with("repo-"));

    let v = json(
        &f.cmd(
            &f.wt_a,
            &["whoami", "--scope", "named", "--agent", "codex:1", "--json"],
        )
        .output()
        .unwrap(),
    );
    assert_eq!(v["scope"], "named");
    assert_eq!(v["scope_source"], "flag");
    assert_eq!(v["agent"], "codex:1");

    let out = f.taba(&f.outside, &["whoami", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    assert!(v["scope"].is_null() && v["scope_source"].is_null());

    let out = f
        .cmd(&f.outside, &["whoami"])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert!(json(&out)["scope"].is_null());
}
