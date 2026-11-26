//! `taba run --env KEY=VALUE` and `taba run --save` (issue #44).
mod common;
use common::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Output;

/// A value no part of taba may print or store (only the service sees it).
const SECRET: &str = "s3cr3t-Tb4-q7Zx9-never-print-me";

fn run(f: &Fixture, cwd: &Path, args: &[&str]) -> Output {
    f.cmd(cwd, args)
        .env("TABA_AGENT", "agent-a")
        .output()
        .unwrap()
}

fn call(f: &Fixture, cwd: &Path, args: &[&str]) -> (i32, Value) {
    let out = f
        .cmd(cwd, args)
        .env("TABA_AGENT", "agent-a")
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_no_secret("JSON output", &out);
    (exit(&out), json(&out))
}

fn assert_no_secret(what: &str, out: &Output) {
    let all = text(out);
    assert!(!all.contains(SECRET), "{what} shows the secret:\n{all}");
}

/// Every file under `dir`, read as bytes: none may hold the secret.
fn assert_secret_not_under(dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    let mut files = 0;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p) {
                files += 1;
                let hit = bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes());
                assert!(!hit, "{} holds the secret", p.display());
            }
        }
    }
    assert!(files > 0, "nothing under {}", dir.display());
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A service that logs a hash of `API_TOKEN` (so the test sees it arrived
/// without the value reaching the log), then "ready".
fn token_service() -> Vec<String> {
    vec![
        PY.into(),
        "-u".into(),
        "-c".into(),
        "import hashlib, os, time; v = os.environ['API_TOKEN']; \
         print('token-sha=' + hashlib.sha256(v.encode()).hexdigest()); \
         print('mode=' + os.environ.get('MODE', '-')); print('ready'); time.sleep(30)"
            .into(),
    ]
}

fn run_args<'a>(
    name: &'a str,
    env: &[&'a str],
    extra: &[&'a str],
    cmd: &'a [String],
) -> Vec<&'a str> {
    let mut v = vec!["run", "--name", name, "--ready", "log:^ready", "--wait=20"];
    for e in env {
        v.extend_from_slice(&["--env", e]);
    }
    v.extend_from_slice(extra);
    v.push("--");
    v.extend(cmd.iter().map(String::as_str));
    v
}

#[test]
fn correction_diagnostics_do_not_echo_env_values() {
    let f = Fixture::new("env_corrections");
    let token = format!("API_TOKEN={SECRET}");
    let inline = format!("--env={token}");
    for env in [vec!["--env", token.as_str()], vec![inline.as_str()]] {
        for quoted in [false, true] {
            let mut args = vec!["run", "--name", "web", "--interactive", "--save"];
            args.extend_from_slice(&env);
            if quoted {
                args.extend_from_slice(&["npm run dev", "--open"]);
            } else {
                args.extend_from_slice(&["npm", "run", "dev"]);
            }
            let (code, v) = call(&f, &f.wt_a, &args);
            assert_eq!(code, 2, "{v}");
            assert_eq!(v["error"]["code"], "E_USAGE");
            let out = run(&f, &f.wt_a, &args);
            assert_eq!(exit(&out), 2);
            assert_no_secret("correction", &out);
            assert!(text(&out).contains("API_TOKEN=<hidden>"));
        }
    }
}

#[cfg(unix)]
#[test]
fn interactive_correction_hides_value_and_child_receives_original() {
    let f = Fixture::new("env_prompt");
    let token = format!("API_TOKEN={SECRET}");
    let cmd = token_service();
    let mut args = run_args("api", &[&token], &[], &cmd);
    args.retain(|a| *a != "--");
    let out = std::process::Command::new(PY)
        .args([
            "-c",
            r#"
import os, pty, select, subprocess, sys
master, slave = pty.openpty()
p = subprocess.Popen(sys.argv[1:], stdin=slave, stderr=slave, stdout=subprocess.PIPE)
os.close(slave)
chunks = []
answered = False
while True:
    if not select.select([master], [], [], 30)[0]:
        p.kill()
        raise RuntimeError('timed out waiting for confirmation')
    try:
        chunk = os.read(master, 4096)
        if not chunk: break
        chunks.append(chunk)
        if not answered and b'Run it? [Y/n]' in b''.join(chunks):
            os.write(master, b'\n')
            answered = True
    except OSError: break
stdout, _ = p.communicate(timeout=30)
os.close(master)
sys.stdout.buffer.write(stdout + b''.join(chunks))
sys.exit(p.returncode)
"#,
            BIN,
        ])
        .args(&args)
        .current_dir(&f.wt_a)
        .env("TABA_HOME", f.home())
        .env_remove("TABA_AGENT")
        .env_remove("TABA_AGENT_PID")
        .env_remove("TABA_SCOPE")
        .env_remove("TABA_OUTPUT")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_no_secret("interactive confirmation", &out);
    assert!(text(&out).contains("Run it? [Y/n]"), "{}", text(&out));
    assert!(text(&out).contains("API_TOKEN=<hidden>"));
    let log = f.taba(&f.wt_a, &["logs", "api"]);
    assert!(text(&log).contains(&sha256_hex(SECRET)), "{}", text(&log));
}

#[test]
fn child_drops_caller_identity_but_honors_deliberate_env_overrides() {
    let f = Fixture::new("child_identity");
    let command = vec![PY.into(), "-u".into(), "-c".into(),
        "import os, time; print('vars=' + repr([os.environ.get(k) for k in ['TABA_AGENT', 'TABA_AGENT_PID', 'TABA_OUTPUT']])); print('ready'); time.sleep(30)".into()];
    for (name, env, expected) in [
        ("clean", vec![], "vars=[None, None, None]"),
        (
            "explicit",
            vec![
                "TABA_AGENT=deliberate",
                "TABA_AGENT_PID=123",
                "TABA_OUTPUT=text",
            ],
            "vars=['deliberate', '123', 'text']",
        ),
    ] {
        let switches = if cfg!(unix) {
            vec!["--interactive", "--save"]
        } else {
            vec!["--no-interactive", "--save"]
        };
        let args = run_args(name, &env, &switches, &command);
        let out = f
            .cmd(&f.wt_a, &args)
            .env("TABA_AGENT", "agent-a")
            .env("TABA_AGENT_PID", std::process::id().to_string())
            .env("TABA_OUTPUT", "json")
            .output()
            .unwrap();
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let v = json(&out);
        assert_eq!(v["interactive"], cfg!(unix));
        assert!(
            v["saved"]["definition"]
                .as_str()
                .unwrap()
                .contains(&format!("interactive = {}", cfg!(unix)))
        );
        let log = std::fs::read_to_string(v["log_path"].as_str().unwrap()).unwrap();
        assert!(log.contains(expected), "{log}");
        let stopped = f
            .cmd(&f.wt_a, &["stop", name, "--json"])
            .env("TABA_AGENT", "agent-a")
            .env("TABA_AGENT_PID", std::process::id().to_string())
            .output()
            .unwrap();
        assert_eq!(exit(&stopped), 0, "{}", text(&stopped));
        let up = f
            .cmd(&f.wt_a, &["up", name, "--json"])
            .env("TABA_AGENT", "agent-a")
            .env("TABA_AGENT_PID", std::process::id().to_string())
            .output()
            .unwrap();
        assert_eq!(exit(&up), 0, "{}", text(&up));
        assert_eq!(json(&up)["results"][0]["interactive"], cfg!(unix));
        let logs = f.taba(&f.wt_a, &["logs", name]);
        assert!(text(&logs).contains(expected), "{}", text(&logs));
        let stopped = f
            .cmd(&f.wt_a, &["stop", name, "--json"])
            .env("TABA_AGENT", "agent-a")
            .env("TABA_AGENT_PID", std::process::id().to_string())
            .output()
            .unwrap();
        assert_eq!(exit(&stopped), 0, "{}", text(&stopped));
    }
}

#[test]
fn env_values_never_reach_state_events_logs_or_output() {
    let f = Fixture::new("env_secret");
    let token = format!("API_TOKEN={SECRET}");
    let cmd = token_service();
    let env = [token.as_str(), "MODE=dev"];

    let (code, v) = call(&f, &f.wt_a, &run_args("api", &env, &[], &cmd));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], "started");
    assert_eq!(v["env"], serde_json::json!(["API_TOKEN", "MODE"]));
    let log_path = v["log_path"].as_str().unwrap().to_string();
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains(&format!("token-sha={}", sha256_hex(SECRET))),
        "the service did not get the value: {log}"
    );
    assert!(log.contains("mode=dev"), "{log}");
    let instance = v["instance"].clone();

    // The record keeps the names, and a keyed hash, not the value.
    let rec_path = f
        .home()
        .join("services")
        .join(f.scope_of(&f.wt_a))
        .join("api.json");
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(&rec_path).unwrap()).unwrap();
    assert_eq!(rec["env_keys"], serde_json::json!(["API_TOKEN", "MODE"]));
    let hash = rec["env_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    assert_ne!(hash, sha256_hex(SECRET));

    // Same names and values, in another order, human output: reused.
    let swapped = ["MODE=dev", token.as_str()];
    let out = run(&f, &f.wt_a, &run_args("api", &swapped, &[], &cmd));
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_no_secret("human output", &out);
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("already running"), "{shown}");
    assert!(
        shown.contains("API_TOKEN, MODE (values not shown)"),
        "{shown}"
    );
    let (code, v) = call(&f, &f.wt_a, &run_args("api", &swapped, &[], &cmd));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], "already_running");
    assert_eq!(v["instance"], instance);

    // A changed value is noticed although it is not stored.
    let other = format!("API_TOKEN={SECRET}-rotated");
    let changed = [other.as_str(), "MODE=dev"];
    let (code, v) = call(&f, &f.wt_a, &run_args("api", &changed, &[], &cmd));
    assert_eq!(code, 3, "{v}");
    assert_eq!(v["error"]["code"], "E_CONFLICT");
    assert_eq!(
        v["error"]["details"]["env"],
        serde_json::json!(["API_TOKEN", "MODE"])
    );
    assert_eq!(v["error"]["details"]["interactive"], false);
    let out = run(&f, &f.wt_a, &run_args("api", &changed, &[], &cmd));
    assert_eq!(exit(&out), 3);
    assert_no_secret("human error", &out);
    // So is a missing name.
    let (code, v) = call(&f, &f.wt_a, &run_args("api", &[token.as_str()], &[], &cmd));
    assert_eq!(code, 3, "{v}");
    // And no --env at all.
    let (code, v) = call(&f, &f.wt_a, &run_args("api", &[], &[], &cmd));
    assert_eq!(code, 3, "{v}");

    // Bad --env arguments are usage errors that do not echo the argument.
    let no_eq = SECRET.to_string();
    let no_name = format!("={SECRET}");
    let reserved = format!("PORT={SECRET}");
    let twice = format!("MODE={SECRET}");
    for bad in [
        vec![no_eq.as_str()],
        vec![no_name.as_str()],
        vec![reserved.as_str()],
        vec!["MODE=a", twice.as_str()],
    ] {
        let (code, v) = call(&f, &f.wt_b, &run_args("bad", &bad, &[], &cmd));
        assert_eq!(code, 2, "{bad:?}: {v}");
        assert_eq!(v["error"]["code"], "E_USAGE", "{v}");
        let out = run(&f, &f.wt_b, &run_args("bad", &bad, &[], &cmd));
        assert_eq!(exit(&out), 2);
        assert_no_secret("usage error", &out);
    }

    // Listings, logs and the record files.
    for args in [
        &["ps"][..],
        &["ps", "--json"],
        &["ps", "--all", "--json"],
        &["logs", "api"],
        &["whoami", "--json"],
    ] {
        let out = run(&f, &f.wt_a, args);
        assert_no_secret(&format!("taba {args:?}"), &out);
    }
    let (code, v) = call(&f, &f.wt_a, &["stop", "api"]);
    assert_eq!(code, 0, "{v}");
    assert!(!f.events().is_empty());
    assert_secret_not_under(&f.home());
}

#[test]
fn concurrent_cli_saves_merge_new_file_plans() {
    let f = Fixture::new("concurrent_save");
    // Neither child reports ready until both runs have passed preflight,
    // so both save plans observed a nonexistent config file.
    let script = "import pathlib, sys, time\na = pathlib.Path(sys.argv[1]); b = pathlib.Path(sys.argv[2]); a.touch()\ndeadline = time.monotonic() + 15\nwhile not b.exists():\n if time.monotonic() > deadline: sys.exit(1)\n time.sleep(.01)\nprint('ready', flush=True)\ntime.sleep(30)";
    let mut children = Vec::new();
    for (name, other) in [("first", "second"), ("second", "first")] {
        let cmd = vec![
            PY.into(),
            "-u".into(),
            "-c".into(),
            script.into(),
            f.root.join(name).to_string_lossy().into_owned(),
            f.root.join(other).to_string_lossy().into_owned(),
        ];
        let args = run_args(name, &[], &["--save"], &cmd);
        children.push(
            f.cmd(&f.wt_a, &args)
                .env("TABA_AGENT", "agent-a")
                .env("TABA_OUTPUT", "json")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert_eq!(exit(&out), 0, "{}", text(&out));
    }
    let saved: toml::Table = std::fs::read_to_string(f.wt_a.join("taba.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let services = saved["services"].as_table().unwrap();
    assert!(
        services.contains_key("first") && services.contains_key("second"),
        "{services:?}"
    );
}

#[test]
fn save_writes_taba_toml_and_up_starts_it_the_same_way() {
    let f = Fixture::new("run_save");
    let file = f.wt_a.join("taba.toml");
    std::fs::write(
        &file,
        "# project services\n\n# the worker\n[services.worker]\ncmd = [\"true\"]\n",
    )
    .unwrap();
    let mut server: Vec<String> = SERVER.iter().map(|s| s.to_string()).collect();
    let cmd = token_service();
    let token = format!("API_TOKEN={SECRET}");

    // --save adds the service after it started.
    let mut args = vec![
        "run", "--name", "web", "--port", "auto", "--ready", "http:/", "--wait",
    ];
    args.extend_from_slice(&["--env", &token, "--save", "--"]);
    args.extend(server.iter().map(String::as_str));
    let (code, v) = call(&f, &f.wt_a, &args);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], "started");
    assert_eq!(v["saved"]["result"], "added", "{v}");
    assert_eq!(v["saved"]["name"], "web");
    assert_eq!(
        canon(Path::new(v["saved"]["path"].as_str().unwrap())),
        canon(&file)
    );
    let def = v["saved"]["definition"].as_str().unwrap();
    assert!(
        def.contains("[services.web]") && def.contains("<hidden>"),
        "{def}"
    );
    let warn: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(warn.contains(&"W_ENV_SAVED"), "{v}");
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.starts_with("# project services\n"), "{saved}");
    assert!(saved.contains("# the worker\n[services.worker]"), "{saved}");
    assert!(saved.contains("[services.web]"), "{saved}");
    assert!(saved.contains("port = \"auto\""), "{saved}");
    assert!(saved.contains("ready = \"http:/\""), "{saved}");
    // The file is where the value lives, so `up` can set it again.
    assert!(
        saved.contains(&format!("API_TOKEN = \"{SECRET}\"")),
        "{saved}"
    );
    let instance = v["instance"].clone();

    // `up web` reads it as the same service.
    let (code, up) = call(&f, &f.wt_a, &["up", "web"]);
    assert_eq!(code, 0, "{up}");
    assert_eq!(up["results"][0]["status"], "already_running", "{up}");
    assert_eq!(up["results"][0]["instance"], instance);

    // Saving the same run again changes nothing.
    let (code, v) = call(&f, &f.wt_a, &args);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], "already_running");
    assert_eq!(v["saved"]["result"], "unchanged");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);

    // A different definition is refused before anything starts.
    let (code, v) = call(
        &f,
        &f.outside,
        &["stop", "--scope", &f.scope_of(&f.wt_a), "web"],
    );
    assert_eq!(code, 0, "{v}");
    server.push("--directory".into());
    server.push(".".into());
    let mut changed = vec!["run", "--name", "web", "--port", "auto", "--save", "--"];
    changed.extend(server.iter().map(String::as_str));
    let (code, v) = call(&f, &f.wt_a, &changed);
    assert_eq!(code, 3, "{v}");
    assert_eq!(v["error"]["code"], "E_CONFLICT");
    assert!(
        v["error"]["details"]["existing"]
            .as_str()
            .unwrap()
            .contains("<hidden>")
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);
    assert!(f.ps(&["--running"]).is_empty(), "nothing may start");

    // --force replaces it, keeping the rest of the file.
    let mut forced = changed.clone();
    forced.insert(1, "--force");
    let (code, v) = call(&f, &f.wt_a, &forced);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["saved"]["result"], "updated");
    let now = std::fs::read_to_string(&file).unwrap();
    assert!(now.contains("\"--directory\""), "{now}");
    assert!(!now.contains("API_TOKEN"), "{now}");
    assert!(now.contains("# the worker\n[services.worker]"), "{now}");
    // ready was not given: the saved one is kept.
    assert!(now.contains("ready = \"http:/\""), "{now}");
    let (code, v) = call(&f, &f.wt_a, &["down", "web"]);
    assert_eq!(code, 0, "{v}");

    // A service with env, saved from a subdirectory of a worktree without
    // a taba.toml: the file goes to the worktree root, with cwd relative.
    let sub = f.wt_b.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let (code, v) = call(
        &f,
        &sub,
        &run_args("api", &[token.as_str(), "MODE=x"], &["--save"], &cmd),
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["saved"]["result"], "created", "{v}");
    let file_b = f.wt_b.join("taba.toml");
    let saved_b = std::fs::read_to_string(&file_b).unwrap();
    assert!(saved_b.contains("cwd = \"sub\""), "{saved_b}");
    assert!(saved_b.contains("wait = 20"), "{saved_b}");
    let instance = v["instance"].clone();
    let (code, up) = call(&f, &f.wt_b, &["up"]);
    assert_eq!(code, 0, "{up}");
    assert_eq!(up["results"][0]["status"], "already_running", "{up}");
    assert_eq!(up["results"][0]["instance"], instance);
    // After down, up starts it with the saved env.
    let (code, v) = call(&f, &f.wt_b, &["down"]);
    assert_eq!(code, 0, "{v}");
    let (code, up) = call(&f, &f.wt_b, &["up"]);
    assert_eq!(code, 0, "{up}");
    assert_eq!(up["results"][0]["status"], "started", "{up}");
    assert_eq!(
        up["results"][0]["env"],
        serde_json::json!(["API_TOKEN", "MODE"])
    );
    assert_eq!(up["results"][0]["directory"], canon(&sub).to_str().unwrap());
    let log = std::fs::read_to_string(up["results"][0]["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(&format!("token-sha={}", sha256_hex(SECRET))),
        "{log}"
    );
    assert!(log.contains("mode=x"), "{log}");
    let (code, v) = call(&f, &f.wt_b, &["down"]);
    assert_eq!(code, 0, "{v}");
    assert_secret_not_under(&f.home());
}

#[test]
fn save_refuses_bad_files_and_force_needs_save() {
    let f = Fixture::new("save_bad");
    let cmd = token_service();
    std::fs::write(f.wt_a.join("taba.toml"), "[services.web]\nrestart = true\n").unwrap();
    let token = format!("API_TOKEN={SECRET}");
    let (code, v) = call(
        &f,
        &f.wt_a,
        &run_args("api", &[token.as_str()], &["--save"], &cmd),
    );
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "E_CONFIG");
    assert!(f.ps(&[]).is_empty(), "nothing may start");

    let (code, v) = call(&f, &f.wt_b, &run_args("api", &[], &["--force"], &cmd));
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["error"]["code"], "E_USAGE");
    assert!(!f.wt_b.join("taba.toml").exists());
}
