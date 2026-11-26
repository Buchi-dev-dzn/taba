//! `taba restart S/N` (issue #43): keeps the URL, starts a new instance,
//! refuses non-owners like `stop`, and is safe to run twice at once.
mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Child, Stdio};

fn call(f: &Fixture, dir: &Path, args: &[&str]) -> (i32, Value) {
    let out = f
        .cmd(dir, args)
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    (exit(&out), json(&out))
}

#[test]
fn restart_keeps_the_url_and_starts_a_new_instance() {
    let f = Fixture::new("restart_url");
    let (code, web) = call(&f, &f.wt_a, &run_auto("web"));
    assert_eq!(code, 0, "{web}");
    let target = web["service"].as_str().unwrap().to_string();
    let port = port_of(&web);

    // No --wait: the instance was run with --wait, so restart waits too.
    let (code, r) = call(&f, &f.wt_a, &["restart", "web"]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["status"], "restarted", "{r}");
    assert_eq!(r["service"], target.as_str());
    assert_eq!(r["url"], web["url"], "{r}");
    assert_eq!(r["port"], port, "{r}");
    assert_ne!(r["instance"], web["instance"], "{r}");
    assert_eq!(r["ready"]["check"], "tcp", "{r}");
    assert_eq!(r["previous"]["instance"], web["instance"], "{r}");
    assert_eq!(r["previous"]["result"], "stopped", "{r}");
    assert_eq!(r["directory"], web["directory"]);
    assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");
    assert!(http_ok(port));

    let ps = f.ps(&[]);
    assert_eq!(ps.len(), 1, "{ps:?}");
    assert_eq!(ps[0]["instance"], r["instance"], "{ps:?}");
    // The lock is released, and the same `run` is now idempotent again.
    assert!(!f.home().join("ops").join(format!("{target}.json")).exists());
    let (code, again) = call(&f, &f.wt_a, &run_auto("web"));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["status"], "already_running", "{again}");
    assert_eq!(again["instance"], r["instance"], "{again}");

    // Text output.
    let out = f.taba(&f.wt_a, &["restart", "web", "--wait=20"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.starts_with(&format!("restarted {target} → http://localhost:{port}")),
        "{s}"
    );
}

#[test]
fn restart_waits_for_a_short_runs_delayed_discovered_port() {
    let f = Fixture::new("restart_discovered_port");
    // No --name or --port: the short run must discover a listener. On
    // restart, the new listener is deliberately absent at the 200ms alive
    // check, so returning early deterministically loses the URL.
    let script = "import http.server, time\n\
                  time.sleep(1)\n\
                  server = http.server.HTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler)\n\
                  server.serve_forever()\n";
    let (code, started) = call(&f, &f.wt_a, &["run", "--wait=10", "--", PY, "-c", script]);
    assert_eq!(code, 0, "{started}");
    assert_eq!(started["ready"]["check"], "port", "{started}");
    assert!(started["url"].is_string(), "{started}");
    let name = started["name"].as_str().unwrap();
    let record_path = f
        .home()
        .join("services")
        .join(format!("{}.json", started["service"].as_str().unwrap()));
    // Exercise migration of the older short-run representation too.
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["wait_request"]
        .as_object_mut()
        .unwrap()
        .remove("ready");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

    // Repeating this also checks that the discovery wait survives another
    // generation, rather than being inferred only from the initial run.
    for _ in 0..2 {
        let (code, restarted) = call(&f, &f.wt_a, &["restart", name]);
        assert_eq!(code, 0, "{restarted}");
        assert_eq!(restarted["ready"]["check"], "port", "{restarted}");
        assert!(
            restarted["ready"]["ms"].as_u64().unwrap() >= 500,
            "{restarted}"
        );
        assert!(
            restarted["port"].is_null(),
            "not a managed port: {restarted}"
        );
        let ports = restarted["discovered_ports"].as_array().unwrap();
        assert_eq!(ports.len(), 1, "{restarted}");
        let port = ports[0].as_u64().unwrap() as u16;
        assert_eq!(restarted["url"], format!("http://localhost:{port}"));
        assert!(http_ok(port), "{restarted}");
        let ps = f.ps(&[]);
        assert_eq!(ps[0]["discovered_ports"], restarted["discovered_ports"]);
        assert_eq!(ps[0]["url"], restarted["url"]);
        let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(record["ready"]["check"], "port");
        assert_eq!(record["wait_request"]["ready"], "port");
    }
}

#[test]
fn restart_preserves_discovery_after_an_initial_port_wait_timeout() {
    let f = Fixture::new("restart_discovery_timeout");
    let script = "import http.server, time\n\
                  time.sleep(1)\n\
                  http.server.HTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler).serve_forever()\n";
    let (code, started) = call(&f, &f.wt_a, &["run", "--wait=0.05", "--", PY, "-c", script]);
    assert_eq!(code, 0, "{started}");
    assert!(started["ready"].is_null(), "{started}");
    assert!(started["url"].is_null(), "{started}");
    let name = started["name"].as_str().unwrap();

    // A discovery timeout remains advisory on restart, as on run.
    let (code, timed_out) = call(&f, &f.wt_a, &["restart", name]);
    assert_eq!(code, 0, "{timed_out}");
    assert!(timed_out["ready"].is_null(), "{timed_out}");
    assert!(timed_out["url"].is_null(), "{timed_out}");
    assert!(timed_out["discovered_ports"].as_array().unwrap().is_empty());

    // No successful ready.check existed to infer discovery from: the
    // recorded request itself must preserve the wait across restarts.
    let (code, restarted) = call(&f, &f.wt_a, &["restart", name, "--wait=10"]);
    assert_eq!(code, 0, "{restarted}");
    assert_eq!(restarted["ready"]["check"], "port", "{restarted}");
    assert!(restarted["url"].is_string(), "{restarted}");
    let port = restarted["discovered_ports"][0].as_u64().unwrap() as u16;
    assert!(http_ok(port));
}

#[test]
fn restart_refuses_non_owners_like_stop() {
    let f = Fixture::new("restart_owner");
    let (_, web) = call(&f, &f.wt_a, &run_auto("web"));
    let target = web["service"].as_str().unwrap().to_string();

    let (code, denied) = call(&f, &f.wt_b, &["restart", &target]);
    assert_eq!(code, 3, "{denied}");
    assert_eq!(denied["error"]["code"], "E_NOT_OWNER", "{denied}");
    assert!(
        denied["error"]["message"]
            .as_str()
            .unwrap()
            .contains("taba restart"),
        "{denied}"
    );
    let (code, denied) = call(&f, &f.outside, &["restart", &target]);
    assert_eq!(code, 3, "{denied}");
    assert_eq!(denied["error"]["code"], "E_NOT_OWNER", "{denied}");
    let (code, bad) = call(&f, &f.outside, &["restart", &target, "--force"]);
    assert_eq!(code, 2, "{bad}");
    assert_eq!(f.ps(&[])[0]["instance"], web["instance"]);

    let (code, forced) = call(
        &f,
        &f.outside,
        &[
            "restart",
            &target,
            "--force",
            "--reason",
            "stuck dev server",
        ],
    );
    assert_eq!(code, 0, "{forced}");
    assert_eq!(forced["forced"], true, "{forced}");
    assert_eq!(forced["port"], web["port"], "{forced}");
    let ev: Vec<Value> = f
        .events()
        .into_iter()
        .filter(|e| e["event"] == "force_restart")
        .collect();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["target"], target.as_str());
    assert_eq!(ev[0]["reason"], "stuck dev server");
    assert_eq!(ev[0]["instance"], web["instance"]);
    assert_eq!(ev[0]["overrode"][0], "E_NOT_OWNER");

    let (code, missing) = call(&f, &f.wt_a, &["restart", "nope"]);
    assert_eq!(code, 5, "{missing}");
    assert_eq!(missing["error"]["code"], "E_NOT_FOUND", "{missing}");
}

#[test]
fn restart_under_an_exclusive_borrow_leaves_shared_borrows_dangling() {
    let f = Fixture::new("restart_borrow");
    let (_, web) = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap().to_string();
    let (_, shared) = call(&f, &f.wt_b, &["borrow", &target]);
    let shared_id = shared["borrow"].as_str().unwrap().to_string();

    // The owner is refused while another scope borrows it, as with stop.
    let (code, denied) = call(&f, &f.wt_a, &["restart", "db"]);
    assert_eq!(code, 3, "{denied}");
    assert_eq!(denied["error"]["code"], "E_BORROWED", "{denied}");

    // The exclusive borrower may restart it.
    let (code, ex) = call(
        &f,
        &f.outside,
        &["borrow", "--scope", "other", "--mut", &target],
    );
    assert_eq!(code, 0, "{ex}");
    let ex_id = ex["borrow"].as_str().unwrap().to_string();
    let (code, r) = call(&f, &f.outside, &["restart", "--scope", "other", &target]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["port"], web["port"], "{r}");
    assert_ne!(r["instance"], web["instance"], "{r}");

    let (_, check) = call(&f, &f.wt_b, &["borrow", "--check", &shared_id]);
    assert_eq!(check["error"]["code"], "E_DANGLING", "{check}");
    assert_eq!(check["error"]["details"]["current"]["url"], web["url"]);
    // The exclusive borrow follows the new instance.
    let (code, check) = call(
        &f,
        &f.outside,
        &["borrow", "--scope", "other", "--check", &ex_id],
    );
    assert_eq!(code, 0, "{check}");
    assert_eq!(check["instance"], r["instance"], "{check}");
}

#[test]
fn concurrent_restarts_restart_once_or_report_busy() {
    let f = Fixture::new("restart_busy");
    // Takes a second to exit after the graceful stop request.
    let script = "import signal, sys, time\n\
                  def term(*_):\n    print('got-TERM', flush=True)\n    time.sleep(1)\n    sys.exit(0)\n\
                  signal.signal(getattr(signal, 'SIGBREAK', signal.SIGTERM), term)\n\
                  print('ready', flush=True)\n\
                  while True:\n    time.sleep(0.05)\n";
    let (code, v) = call(
        &f,
        &f.wt_a,
        &[
            "run",
            "--name",
            "sig",
            "--wait",
            "--ready",
            "log:^ready$",
            "--",
            PY,
            "-c",
            script,
        ],
    );
    assert_eq!(code, 0, "{v}");
    let target = v["service"].as_str().unwrap().to_string();

    let children: Vec<Child> = (0..2)
        .map(|_| {
            f.cmd(&f.wt_a, &["restart", &target, "--json"])
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
    let mut got: Vec<String> = outs
        .iter()
        .map(|v| {
            if v["ok"] == true {
                v["status"].as_str().unwrap().to_string()
            } else {
                v["error"]["code"].as_str().unwrap().to_string()
            }
        })
        .collect();
    got.sort();
    // Either the second one ran into the first (E_BUSY), or it started after
    // the first had finished and restarted the new instance in turn.
    assert!(
        got == ["E_BUSY", "restarted"] || got == ["restarted", "restarted"],
        "{outs:?}"
    );
    let ps = f.ps(&[]);
    assert_eq!(ps.len(), 1, "{ps:?}");
    assert_eq!(ps[0]["status"], "running", "{ps:?}");
    let restarted: Vec<&Value> = outs.iter().filter(|v| v["ok"] == true).collect();
    assert_eq!(
        ps[0]["instance"],
        restarted.last().unwrap()["instance"],
        "{ps:?}"
    );
    assert!(!f.home().join("ops").join(format!("{target}.json")).exists());
    // The recorded ready check (log:^ready$) was used again.
    assert_eq!(restarted[0]["ready"]["check"], "log", "{outs:?}");
}

#[test]
fn restart_starts_an_exited_service_again() {
    let f = Fixture::new("restart_exited");
    // Exits at once the first time, keeps running the second time.
    let script = "import os, time\n\
                  if not os.path.exists('started-once'):\n    open('started-once', 'w').close()\n    raise SystemExit(0)\n\
                  print('up', flush=True)\n\
                  time.sleep(30)\n";
    let out = f.taba(
        &f.wt_a,
        &["run", "--name", "job", "--json", "--", PY, "-c", script],
    );
    let _ = out; // E_START_FAILED or started, depending on timing.
    wait_until("the first run to exit", || {
        f.ps(&[]).first().is_some_and(|s| s["status"] != "running")
    });
    let (code, r) = call(&f, &f.wt_a, &["restart", "job"]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["status"], "restarted", "{r}");
    assert_eq!(r["previous"]["result"], "not_running", "{r}");
    assert!(r["url"].is_null(), "{r}");
    assert_eq!(f.ps(&[])[0]["status"], "running");
}

#[test]
fn restart_reuses_the_directory_and_env_of_a_taba_toml_service() {
    let f = Fixture::new("restart_up");
    std::fs::create_dir_all(f.wt_a.join("api")).unwrap();
    let cmd: Vec<String> = [
        PY,
        "-u",
        "-c",
        "import os, time; print('cwd=' + os.getcwd()); print('FOO=' + os.environ['FOO']); \
         print('ready', flush=True); time.sleep(30)",
    ]
    .iter()
    .map(|a| format!("{a:?}"))
    .collect();
    std::fs::write(
        f.wt_a.join("taba.toml"),
        format!(
            "[services.worker]\ncmd = [{}]\nready = \"log:^ready\"\ncwd = \"api\"\n\
             env = {{ FOO = \"bar\" }}\n",
            cmd.join(", ")
        ),
    )
    .unwrap();
    let (code, up) = call(&f, &f.wt_a, &["up"]);
    assert_eq!(code, 0, "{up}");
    let first = &up["results"][0];

    // From another directory of the same scope, with FOO unset.
    let (code, r) = call(&f, &f.wt_a, &["restart", "worker"]);
    assert_eq!(code, 0, "{r}");
    assert_ne!(r["instance"], first["instance"], "{r}");
    assert_eq!(r["directory"], first["directory"], "{r}");
    assert_eq!(r["ready"]["check"], "log", "{r}");
    let log = std::fs::read_to_string(r["log_path"].as_str().unwrap()).unwrap();
    assert_eq!(log.matches("FOO=bar").count(), 2, "{log}");
    let api = canon(&f.wt_a.join("api"));
    assert_eq!(
        log.matches(&format!("cwd={}", api.display())).count(),
        2,
        "{log}"
    );
}
