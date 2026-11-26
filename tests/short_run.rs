//! The short human form of `taba run` (#67) and the rules for writing the
//! command: no options (everything is the command), a quoted command (run
//! by the shell, taba options before or after it), `--` after options, and
//! the correction when `--` is missing. Port discovery, re-runs, name
//! clashes, `taba` alone, and the explicit form left unchanged.
mod common;
use common::*;
use serde_json::{Value, json};
use std::path::Path;

/// Prints its arguments as JSON, then listens on a free port until killed.
const ARGV_PY: &str = "import json, socket, sys, time
print('ARGV ' + json.dumps(sys.argv[1:]), flush=True)
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen()
time.sleep(60)
";

fn write_argv_script(dir: &Path) {
    std::fs::write(dir.join("argv.py"), ARGV_PY).unwrap();
}

/// The arguments the service's `argv.py` received, read from its log.
fn argv_of(f: &Fixture, dir: &Path, scope: &str, name: &str) -> Value {
    let mut found = Value::Null;
    wait_until("the service's argv in its log", || {
        let out = f
            .cmd(dir, &["logs", &format!("{scope}/{name}"), "--json"])
            .output()
            .unwrap();
        let log = json(&out)["log"].as_str().unwrap_or("").to_string();
        match log.lines().find_map(|l| l.strip_prefix("ARGV ")) {
            Some(a) => {
                found = serde_json::from_str(a.trim()).unwrap();
                true
            }
            None => false,
        }
    });
    found
}

/// The program name the short form derives for the Python interpreter.
fn py_name() -> &'static str {
    PY
}

#[test]
fn short_form_reports_the_port_the_app_chose_and_reuses_it() {
    let f = Fixture::new("short-reuse");
    let scope = f.scope_of(&f.wt_a);
    write_argv_script(&f.wt_a);
    let args = ["run", PY, "argv.py", "--port", "4000"];

    let out = f
        .cmd(&f.wt_a, &args)
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["status"], "started", "{v}");
    assert_eq!(v["name"], py_name(), "{v}");
    // No port was assigned; the one the app listens on was discovered.
    assert!(v["port"].is_null(), "{v}");
    let ports = v["discovered_ports"].as_array().unwrap().clone();
    assert_eq!(ports.len(), 1, "{v}");
    let port = ports[0].as_u64().unwrap();
    assert_eq!(v["url"], format!("http://localhost:{port}"), "{v}");
    assert_eq!(v["ready"]["check"], "port", "{v}");
    let instance = v["instance"].clone();
    // The app got its own options, and no PORT from taba.
    assert_eq!(
        argv_of(&f, &f.wt_a, &scope, py_name()),
        json!(["--port", "4000"])
    );

    // The same command again reuses it, and says what it understood.
    let out = f.taba(&f.wt_a, &args);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("already running: {} → http://localhost:{port}", py_name()),
        "{stdout}"
    );
    assert_eq!(
        lines[1],
        format!("  command: {PY} argv.py --port 4000"),
        "{stdout}"
    );
    assert_eq!(
        lines[2],
        format!(
            "  name \"{0}\" (from \"{0}\"; set another with --name)",
            py_name()
        ),
        "{stdout}"
    );
    assert_eq!(
        lines[3],
        format!("  logs: taba logs {0} · stop: taba stop {0}", py_name()),
        "{stdout}"
    );
    let rows = f.ps(&[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["instance"], instance);
    assert_eq!(rows[0]["url"], format!("http://localhost:{port}"));
    assert_eq!(rows[0]["discovered_ports"], json!([port]));
}

#[test]
fn short_form_without_a_port_says_so_and_succeeds() {
    let f = Fixture::new("short-noport");
    let out = f.taba(&f.wt_a, &["run", "--wait=1", "--", PY, "-c", SLEEP_30]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(&format!(
            "started {0}, running, no port yet (see: taba logs {0})\n",
            py_name()
        )),
        "{stdout}"
    );
    let row = &f.ps(&[])[0];
    assert_eq!(row["status"], "running", "{row}");
    assert!(row["url"].is_null(), "{row}");
}

#[test]
fn short_form_refuses_a_derived_name_used_by_another_command_or_directory() {
    let f = Fixture::new("short-clash");
    let scope = f.scope_of(&f.wt_a);
    write_argv_script(&f.wt_a);
    let sub = f.wt_a.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    write_argv_script(&sub);
    let v = json(&f.taba(&f.wt_a, &["run", "--json", "--", PY, "argv.py", "one"]));
    assert_eq!(v["ok"], true, "{v}");

    for (dir, extra) in [(&sub, "one"), (&f.wt_a, "two")] {
        let out = f.taba(dir, &["run", "--json", "--", PY, "argv.py", extra]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        let v = json(&out);
        let e = &v["error"];
        assert_eq!(e["code"], "E_CONFLICT", "{v}");
        let message = e["message"].as_str().unwrap();
        // Both commands are shown, and --name is suggested.
        assert!(message.contains(&format!("\"{PY} argv.py one\"")), "{v}");
        assert!(
            message.contains(&format!("\"{PY} argv.py {extra}\"")),
            "{v}"
        );
        assert!(message.contains("--name"), "{v}");
        assert_eq!(e["details"]["service"], format!("{scope}/{}", py_name()));
        assert_eq!(
            e["next"][0]["command"],
            format!("taba run --name {}-2 -- {PY} argv.py {extra}", py_name()),
            "{v}"
        );
    }
    // Also when the service has exited.
    let v = json(&f.taba(&f.wt_a, &["stop", py_name(), "--json"]));
    assert_eq!(v["ok"], true, "{v}");
    let v = json(&f.taba(&f.wt_a, &["run", "--json", "--", PY, "argv.py", "two"]));
    assert_eq!(v["error"]["code"], "E_CONFLICT", "{v}");
    // With --name it starts.
    let v = json(&f.taba(
        &f.wt_a,
        &[
            "run", "--name", "second", "--json", "--", PY, "argv.py", "two",
        ],
    ));
    assert_eq!(v["ok"], true, "{v}");
}

#[test]
fn the_command_reaches_the_service_exactly_as_written() {
    let f = Fixture::new("argv");
    write_argv_script(&f.wt_a);
    // (scope, taba arguments, the arguments argv.py must receive)
    let cases: [(&str, Vec<&str>, Value); 5] = [
        // 1. No taba options: everything is the command, its `--` included.
        (
            "s1",
            vec!["run", PY, "argv.py", "run", "dev", "--", "--host"],
            json!(["run", "dev", "--", "--host"]),
        ),
        // 3. Only the first, leading `--` is taba's.
        (
            "s2",
            vec![
                "run", "--", PY, "argv.py", "run", "dev", "--", "--port", "3000",
            ],
            json!(["run", "dev", "--", "--port", "3000"]),
        ),
        (
            "s3",
            vec![
                "run",
                PY,
                "argv.py",
                "run",
                "-d",
                "chrome",
                "--web-port",
                "8080",
            ],
            json!(["run", "-d", "chrome", "--web-port", "8080"]),
        ),
        (
            "s4",
            vec![
                "run", "--name", "web", "--", PY, "argv.py", "run", "dev", "--", "--open",
            ],
            json!(["run", "dev", "--", "--open"]),
        ),
        // Options named like taba's, after the command, are the command's.
        (
            "s5",
            vec![
                "run",
                PY,
                "argv.py",
                "server.js",
                "--port",
                "4000",
                "--json",
            ],
            json!(["server.js", "--port", "4000", "--json"]),
        ),
    ];
    for (scope, args, expected) in cases {
        let out = f
            .cmd(&f.wt_a, &args)
            .env("TABA_SCOPE", scope)
            .output()
            .unwrap();
        assert_eq!(exit(&out), 0, "{args:?}: {}", text(&out));
        // s5's --json went to the command, so taba printed text.
        assert!(
            String::from_utf8_lossy(&out.stdout).starts_with("started "),
            "{args:?}: {}",
            text(&out)
        );
        let name = if args.contains(&"--name") {
            "web"
        } else {
            py_name()
        };
        assert_eq!(argv_of(&f, &f.wt_a, scope, name), expected, "{args:?}");
    }
}

#[test]
fn a_quoted_command_runs_in_the_shell_with_options_before_or_after_it() {
    let f = Fixture::new("quoted");
    let scope = f.scope_of(&f.wt_a);
    write_argv_script(&f.wt_a);
    let line = format!("echo first && {PY} argv.py a b");
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    for (name, args) in [
        (
            "after",
            vec!["run", line.as_str(), "--name", "after", "--json"],
        ),
        (
            "before",
            vec!["run", "--name", "before", "--json", line.as_str()],
        ),
    ] {
        let out = f.taba(&f.wt_a, &args);
        assert_eq!(exit(&out), 0, "{args:?}: {}", text(&out));
        let v = json(&out);
        assert_eq!(v["name"], name, "{v}");
        assert_eq!(argv_of(&f, &f.wt_a, &scope, name), json!(["a", "b"]));
        let log = json(&f.taba(&f.wt_a, &["logs", name, "--json"]));
        assert!(log["log"].as_str().unwrap().contains("first"), "{log}");
    }
    // The text output shows the command line and that the shell runs it.
    let out = f.taba(&f.wt_a, &["run", line.as_str(), "--name", "after"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("\n  command: {line} (via {shell})\n")),
        "{stdout}"
    );
    // Without --name, the name comes from the line's first words.
    let out = f.taba(&f.wt_a, &["run", "--wait=1", &format!("{PY} argv.py x")]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with(&format!("started {} → ", py_name())),
        "{}",
        text(&out)
    );

    // Anything else after the quotes is refused, with the fix.
    let out = f
        .cmd(&f.wt_a, &["run", "npm run dev", "--name", "web", "--open"])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_USAGE", "{v}");
    assert_eq!(
        v["error"]["message"],
        "--open is not a taba option; if it is for your command, put it inside the quotes: \
         taba run \"npm run dev -- --open\" --name web",
        "{v}"
    );
}

#[test]
fn options_before_an_unquoted_command_need_the_separator() {
    let f = Fixture::new("separator");
    let out = f
        .cmd(&f.wt_a, &["run", "--name", "web", "npm", "run", "dev"])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_USAGE", "{v}");
    assert_eq!(
        v["error"]["details"]["corrected"], "taba run --name web -- npm run dev",
        "{v}"
    );
    assert_eq!(
        v["error"]["next"][0]["command"],
        "taba run --name web -- npm run dev"
    );
    // Without a terminal, nothing is asked and nothing starts.
    let out = f.taba(&f.wt_a, &["run", "--wait=1", "npm", "run", "dev"]);
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("taba run --wait=1 -- npm run dev"),
        "{}",
        text(&out)
    );
    assert!(f.ps(&[]).is_empty());
}

#[test]
fn explicit_form_is_unchanged() {
    let f = Fixture::new("explicit");
    let scope = f.scope_of(&f.wt_a);
    // No wait and no port are added when --name is given.
    let mut args = vec!["run", "--name", "web", "--json", "--", PY, "-c", SLEEP_30];
    let v = json(&f.taba(&f.wt_a, &args));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["status"], "started", "{v}");
    assert!(v["port"].is_null() && v["url"].is_null(), "{v}");
    assert!(v["ready"].is_null(), "{v}");
    assert_eq!(v["warnings"], json!([]), "{v}");
    assert_eq!(v["discovered_ports"], json!([]), "{v}");
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "directory",
            "discovered_ports",
            "instance",
            "log_path",
            "name",
            "ok",
            "pid",
            "port",
            "ready",
            "scope",
            "service",
            "status",
            "url",
            "warnings"
        ],
        "{v}"
    );
    args.retain(|a| *a != "--json");
    let out = f.taba(&f.wt_a, &args);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(&format!("already running {scope}/web\n  command: {PY} -c ")),
        "{stdout}"
    );
    assert!(!stdout.contains("name \""), "{stdout}");

    // The agent form with a port: the port is assigned and checked as before.
    let v = json(&f.taba(&f.wt_a, &run_auto("api")));
    assert_eq!(v["ok"], true, "{v}");
    let port = port_of(&v);
    assert_eq!(v["url"], format!("http://localhost:{port}"), "{v}");
    assert_eq!(v["ready"]["check"], "tcp", "{v}");
    assert_eq!(v["discovered_ports"], json!([port]), "{v}");

    // An explicit name may be replaced by another command once it exited.
    let v = json(&f.taba(&f.wt_a, &["stop", "web", "--json"]));
    assert_eq!(v["ok"], true, "{v}");
    let v = json(&f.taba(
        &f.wt_a,
        &[
            "run",
            "--name",
            "web",
            "--json",
            "--",
            PY,
            "-c",
            "import time; time.sleep(29)",
        ],
    ));
    assert_eq!(v["status"], "started", "{v}");
}

#[test]
fn taba_alone_lists_services() {
    let f = Fixture::new("bare");
    let scope = f.scope_of(&f.wt_a);
    let out = f.taba(&f.wt_a, &[]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("SERVICE "),
        "{}",
        text(&out)
    );
    let v = json(&f.taba(
        &f.wt_a,
        &["run", "--name", "w", "--json", "--", PY, "-c", SLEEP_30],
    ));
    assert_eq!(v["ok"], true, "{v}");
    let out = f.taba(&f.wt_a, &[]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(listing.contains(&format!("{scope}/w")), "{listing}");
    assert!(listing.contains("running"), "{listing}");
    let out = f
        .cmd(&f.wt_a, &[])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(json(&out)["services"][0]["service"], format!("{scope}/w"));
}
