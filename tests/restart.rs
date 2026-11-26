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
