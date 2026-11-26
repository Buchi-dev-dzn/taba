//! Interactive input (#68): `run --interactive`, `send`, `attach`, `signal`.
mod common;
use common::*;

/// Echoes each stdin line as `got:LINE`, reports SIGUSR1 and EOF.
#[cfg(unix)]
const ECHO: &str = r#"
import signal, sys
signal.signal(signal.SIGUSR1, lambda *a: print("usr1", flush=True))
print("ready", flush=True)
for line in sys.stdin:
    print("got:" + line.rstrip("\n"), flush=True)
print("EOF", flush=True)
"#;

/// Never reads stdin (which is /dev/null without `--interactive`); reports SIGUSR1.
#[cfg(unix)]
const SLEEPER: &str = r#"
import signal, time
signal.signal(signal.SIGUSR1, lambda *a: print("usr1", flush=True))
print("ready", flush=True)
time.sleep(60)
"#;

#[cfg(unix)]
mod unix {
    use super::*;
    use serde_json::Value;
    use std::io::Write;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;

    fn start(f: &Fixture, cwd: &Path, name: &str, input: bool) -> Value {
        // Without --interactive stdin is /dev/null, so that service must not read it.
        let script = f.root.join(if input { "echo.py" } else { "sleeper.py" });
        std::fs::write(&script, if input { ECHO } else { SLEEPER }).unwrap();
        let script = script.to_str().unwrap().to_string();
        let mut args = vec!["run", "--name", name, "--json"];
        if input {
            args.push("--interactive");
        }
        args.extend(["--wait", "--ready", "log:ready", "--", PY, "-u", &script]);
        let out = f.taba(cwd, &args);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        json(&out)
    }

    fn log(f: &Fixture, cwd: &Path, id: &str) -> String {
        let out = f.taba(cwd, &["logs", id]);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn wait_log(f: &Fixture, cwd: &Path, id: &str, needle: &str) {
        wait_until(&format!("{needle:?} in the log of {id}"), || {
            log(f, cwd, id).contains(needle)
        });
    }

    fn code(out: &std::process::Output) -> String {
        json(out)["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn send_reaches_stdin_and_stdin_stays_open_without_a_writer() {
        let f = Fixture::new("input-send");
        let svc = start(&f, &f.wt_a, "echo", true);
        let id = svc["service"].as_str().unwrap().to_string();

        // Nobody writes: the service must not read EOF.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!log(&f, &f.wt_a, &id).contains("EOF"));
        let row = f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap();
        assert_eq!(row["status"], "running");
        assert_eq!(row["input"], true);

        let out = f.taba(&f.wt_a, &["send", &id, "hello", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["bytes"], 6);
        wait_log(&f, &f.wt_a, &id, "got:hello");

        // A key and text without newline join the next line.
        assert_eq!(exit(&f.taba(&f.wt_a, &["send", &id, "--key", "r"])), 0);
        let out = f.taba(&f.wt_a, &["send", &id, "x", "--no-newline"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(exit(&f.taba(&f.wt_a, &["send", &id, "--key", "enter"])), 0);
        wait_log(&f, &f.wt_a, &id, "got:rx");

        // Between sends no writer is connected; still no EOF.
        std::thread::sleep(Duration::from_millis(500));
        assert!(!log(&f, &f.wt_a, &id).contains("EOF"));

        // attach from a pipe: forwards stdin, shows the log, detaches at EOF.
        let mut child = f
            .cmd(&f.wt_a, &["attach", &id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"via attach\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert!(String::from_utf8_lossy(&out.stderr).contains("keeps running"));
        wait_log(&f, &f.wt_a, &id, "got:via attach");
        assert_eq!(
            f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap()["status"],
            "running"
        );

        // signal goes to the main process.
        let out = f.taba(&f.wt_a, &["signal", &id, "USR1", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["signal"], "USR1");
        wait_log(&f, &f.wt_a, &id, "usr1");

        // Same flags again: already running; different --interactive: conflict.
        let again = start(&f, &f.wt_a, "echo", true);
        assert_eq!(again["status"], "already_running");
        let script = f.root.join("echo.py");
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "echo",
                "--json",
                "--",
                PY,
                "-u",
                script.to_str().unwrap(),
            ],
        );
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_CONFLICT");

        // After stop: E_NOT_RUNNING; rm removes the FIFO.
        assert_eq!(exit(&f.taba(&f.wt_a, &["stop", &id])), 0);
        let out = f.taba(&f.wt_a, &["send", &id, "late", "--json"]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NOT_RUNNING");
        let fifo = f
            .home()
            .join("inputs")
            .join(svc["scope"].as_str().unwrap())
            .join("echo.fifo");
        assert!(fifo.exists());
        assert_eq!(exit(&f.taba(&f.wt_a, &["rm", &id])), 0);
        assert!(!fifo.exists());
    }

    #[test]
    fn a_service_without_input_is_refused() {
        let f = Fixture::new("input-none");
        let svc = start(&f, &f.wt_a, "plain", false);
        let id = svc["service"].as_str().unwrap();
        let out = f.taba(&f.wt_a, &["send", id, "hello", "--json"]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NO_INPUT");
        let out = f
            .cmd(&f.wt_a, &["attach", id])
            .env("TABA_OUTPUT", "json")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NO_INPUT");
        let row = f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap();
        assert_eq!(row["input"], false);
        // signal does not need --interactive.
        let out = f.taba(&f.wt_a, &["signal", id, "usr1"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        wait_log(&f, &f.wt_a, id, "usr1");
    }

    #[test]
    fn input_follows_the_ownership_rules() {
        let f = Fixture::new("input-owner");
        let svc = start(&f, &f.wt_a, "echo", true);
        let id = svc["service"].as_str().unwrap();

        // Another scope is refused, for send, attach and signal alike.
        let out = f.taba(&f.wt_b, &["send", id, "intruder", "--json"]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NOT_OWNER");
        let out = f
            .cmd(&f.wt_b, &["attach", id])
            .env("TABA_OUTPUT", "json")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(code(&out), "E_NOT_OWNER");
        let out = f.taba(&f.wt_b, &["signal", id, "USR1", "--json"]);
        assert_eq!(code(&out), "E_NOT_OWNER");
        let out = f.taba(&f.wt_b, &["send", id, "x", "--force", "--json"]);
        assert_eq!(code(&out), "E_FORCE_NEEDS_REASON");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!log(&f, &f.wt_a, id).contains("intruder"));

        // --force --reason is allowed and recorded.
        let out = f.taba(
            &f.wt_b,
            &[
                "send", id, "forced", "--force", "--reason", "test", "--json",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["forced"], true);
        wait_log(&f, &f.wt_a, id, "got:forced");
        let ev = f
            .events()
            .into_iter()
            .find(|e| e["event"] == "force_send")
            .expect("force_send recorded");
        assert_eq!(ev["reason"], "test");
        assert_eq!(ev["overrode"], serde_json::json!(["E_NOT_OWNER"]));

        // An exclusive borrow lets the borrower send and locks out the owner.
        let out = f.taba(&f.wt_b, &["borrow", id, "--mut", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let out = f.taba(&f.wt_b, &["send", id, "borrower", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        wait_log(&f, &f.wt_a, id, "got:borrower");
        let out = f.taba(&f.wt_a, &["send", id, "owner", "--json"]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_LOCKED");
        assert_eq!(exit(&f.taba(&f.wt_b, &["release", id])), 0);
        let out = f.taba(&f.wt_a, &["send", id, "owner", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        wait_log(&f, &f.wt_a, id, "got:owner");
    }

    #[test]
    fn a_bad_key_is_a_usage_error() {
        let f = Fixture::new("input-key");
        let out = f.taba(&f.wt_a, &["send", "x", "--key", "rr", "--json"]);
        assert_eq!(exit(&out), 2, "{}", text(&out));
        assert_eq!(code(&out), "E_USAGE");
    }
}

#[cfg(windows)]
#[test]
fn run_input_is_unsupported_on_windows() {
    let f = Fixture::new("input-windows");
    let out = f.taba(
        &f.wt_a,
        &[
            "run",
            "--interactive",
            "--name",
            "x",
            "--json",
            "--",
            PY,
            "-c",
            SLEEP_30,
        ],
    );
    assert_eq!(exit(&out), 1, "{}", text(&out));
    assert_eq!(json(&out)["error"]["code"], "E_UNSUPPORTED");
    assert!(f.ps(&[]).is_empty());
}
