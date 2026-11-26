//! `taba up` / `taba down` with a project `taba.toml`.
mod common;
use common::*;
use serde_json::Value;
use std::path::Path;

fn call(f: &Fixture, cwd: &Path, args: &[&str]) -> (i32, Value) {
    let out = f
        .cmd(cwd, args)
        .env("TABA_AGENT", "agent-a")
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    (exit(&out), json(&out))
}

fn toml_list(argv: &[&str]) -> String {
    let items: Vec<String> = argv.iter().map(|a| format!("{a:?}")).collect();
    format!("[{}]", items.join(", "))
}

fn server_cmd() -> String {
    toml_list(&SERVER)
}

#[test]
fn up_is_idempotent_and_down_stops_declared_services() {
    let f = Fixture::new("up_down");
    std::fs::create_dir_all(f.wt_a.join("api")).unwrap();
    let worker = toml_list(&[
        PY,
        "-u",
        "-c",
        "import os, time; print('cwd=' + os.getcwd()); print('FOO=' + os.environ['FOO']); \
         print('ready'); time.sleep(30)",
    ]);
    std::fs::write(
        f.wt_a.join("taba.toml"),
        format!(
            "[services.web]\ncmd = {}\nport = \"auto\"\nready = \"http:/\"\n\n\
             [services.worker]\ncmd = {worker}\nready = \"log:^ready\"\ncwd = \"api\"\n\
             env = {{ FOO = \"bar\" }}\n",
            server_cmd()
        ),
    )
    .unwrap();
    // Run from a subdirectory: the file is found upwards and the scope is
    // still the worktree's.
    let sub = f.wt_a.join("api");
    let (code, up) = call(&f, &sub, &["up"]);
    assert_eq!(code, 0, "{up}");
    assert_eq!(up["ok"], true, "{up}");
    assert_eq!(up["scope"], f.scope_of(&f.wt_a));
    let results = up["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"], "web");
    assert_eq!(results[0]["status"], "started");
    assert_eq!(results[0]["ready"]["check"], "http");
    assert_eq!(results[1]["name"], "worker");
    assert!(http_ok(port_of(&results[0])));
    let log = std::fs::read_to_string(results[1]["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("FOO=bar"), "{log}");
    let api = canon(&f.wt_a.join("api"));
    assert!(log.contains(&format!("cwd={}", api.display())), "{log}");
    assert_eq!(results[1]["directory"], api.to_str().unwrap());

    let (code, again) = call(&f, &f.wt_a, &["up", "worker", "web"]);
    assert_eq!(code, 0, "{again}");
    let results = again["results"].as_array().unwrap();
    assert_eq!(results[0]["name"], "worker");
    assert_eq!(results[0]["status"], "already_running");
    assert_eq!(results[1]["status"], "already_running");
    assert_eq!(results[1]["instance"], up["results"][0]["instance"]);

    let (code, bad) = call(&f, &f.wt_a, &["up", "nope"]);
    assert_eq!(code, 2, "{bad}");
    assert_eq!(bad["error"]["code"], "E_USAGE");

    let (code, down) = call(&f, &f.wt_a, &["down", "web"]);
    assert_eq!(code, 0, "{down}");
    assert_eq!(down["results"][0]["result"], "stopped", "{down}");
    let (code, down) = call(&f, &f.wt_a, &["down"]);
    assert_eq!(code, 0, "{down}");
    let rows: Vec<(String, String)> = down["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["service"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string(),
                r["result"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        rows.contains(&("web".into(), "not_running".into())),
        "{down}"
    );
    assert!(
        rows.contains(&("worker".into(), "stopped".into())),
        "{down}"
    );

    // The other worktree has no services of this file.
    std::fs::write(f.wt_b.join("taba.toml"), "[services.web]\ncmd = \"true\"\n").unwrap();
    let (code, down) = call(&f, &f.wt_b, &["down"]);
    assert_eq!(code, 0, "{down}");
    assert_eq!(down["results"][0]["result"], "absent", "{down}");
}

#[test]
fn up_stops_at_first_failure_and_keeps_earlier_services() {
    let f = Fixture::new("up_fail");
    let crash = toml_list(&[PY, "-c", "import sys; sys.exit(3)"]);
    std::fs::write(
        f.wt_a.join("taba.toml"),
        format!(
            "[services.web]\ncmd = {}\nport = \"auto\"\n\n\
             [services.broken]\ncmd = {crash}\nready = \"log:never\"\nwait = 5\n\n\
             [services.later]\ncmd = {}\nport = \"auto\"\n",
            server_cmd(),
            server_cmd()
        ),
    )
    .unwrap();
    let (code, up) = call(&f, &f.wt_a, &["up"]);
    assert_eq!(code, 4, "{up}");
    assert_eq!(up["ok"], false);
    assert_eq!(up["error"]["code"], "E_START_FAILED", "{up}");
    assert_eq!(up["error"]["details"]["not_started"][0], "later");
    let results = up["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["status"], "started");
    assert_eq!(results[1]["status"], "failed");
    assert!(http_ok(port_of(&results[0])));
    let names: Vec<String> = f
        .ps(&["--running"])
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["web"]);
}

#[test]
fn missing_or_invalid_project_file() {
    let f = Fixture::new("up_nofile");
    let (code, v) = call(&f, &f.outside, &["up", "--scope", "s"]);
    assert_eq!(code, 5, "{v}");
    assert_eq!(v["error"]["code"], "E_NO_PROJECT");
    std::fs::write(f.wt_a.join("taba.toml"), "[services.web]\nrestart = true\n").unwrap();
    let (code, v) = call(&f, &f.wt_a, &["down"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "E_CONFIG");
    assert_eq!(v["error"]["details"]["source"], "project");
}
