//! Interactive services (#68): `run --interactive`, `send`, `attach`,
//! `reload`, `signal`, and input mode in `logs -f`.
mod common;
use common::*;

/// Reports whether stdin is a terminal with echo, then echoes each stdin
/// line as `got:LINE`; reports SIGUSR1 and EOF.
#[cfg(unix)]
const ECHO: &str = r#"
import signal, sys, termios
signal.signal(signal.SIGUSR1, lambda *a: print("usr1", flush=True))
echo = bool(termios.tcgetattr(0)[3] & termios.ECHO) if sys.stdin.isatty() else None
print("tty=%s echo=%s" % (sys.stdin.isatty(), echo), flush=True)
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

    fn record_path(f: &Fixture, svc: &Value) -> std::path::PathBuf {
        f.home()
            .join("services")
            .join(svc["scope"].as_str().unwrap())
            .join(format!("{}.json", svc["name"].as_str().unwrap()))
    }

    /// The holder pid from the service record.
    fn holder_pid(f: &Fixture, svc: &Value) -> i32 {
        let rec: Value =
            serde_json::from_slice(&std::fs::read(record_path(f, svc)).unwrap()).unwrap();
        rec["input"]["holder_pid"].as_i64().expect("holder_pid") as i32
    }

    fn alive(pid: i32) -> bool {
        // kill(pid, 0) succeeds for zombies too; the holder is reparented
        // and reaped once it exits.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn fake_tool(f: &Fixture, name: &str, body: &str) -> String {
        script_tool(f, name, "/usr/bin/env python3", body)
    }

    /// An executable script in the fixture's private bin directory.
    fn script_tool(f: &Fixture, name: &str, interpreter: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = f.root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!{interpreter}\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
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
        assert_eq!(row["interactive"], true);
        assert_eq!(svc["interactive"], true);
        // stdin is a terminal with echo off; stdout is still the log file.
        assert!(log(&f, &f.wt_a, &id).contains("tty=True echo=False"));

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

        // After stop: the holder exits too, send gives E_NOT_RUNNING, and
        // rm removes the FIFO.
        let holder = holder_pid(&f, &svc);
        assert!(alive(holder), "holder {holder} runs while the service does");
        assert_eq!(exit(&f.taba(&f.wt_a, &["stop", &id])), 0);
        wait_until("the holder to exit", || !alive(holder));
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

    fn send_with_queued_borrow_warning(interactive: bool) {
        let f = Fixture::new("input-borrow-warning");
        let input = start(&f, &f.wt_b, "input", interactive);
        let id = input["service"].as_str().unwrap();
        let other = start(&f, &f.wt_a, "other", false);
        let target = other["service"].as_str().unwrap();
        let out = f.taba(&f.wt_b, &["borrow", target, "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let borrow = json(&out);
        let borrow_id = borrow["borrow"].as_str().unwrap();
        let out = f.taba(&f.wt_a, &["stop", target, "--force", "--reason", "test"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));

        let out = f.taba(&f.wt_b, &["send", id, "hello", "--json"]);
        if interactive {
            assert_eq!(exit(&out), 0, "{}", text(&out));
            assert_eq!(json(&out)["bytes"], 6);
        } else {
            assert_eq!(exit(&out), 3, "{}", text(&out));
            assert_eq!(code(&out), "E_NO_INPUT");
        }

        // Observe before any other command can drain the borrow notice.
        let out = f.taba(&f.wt_b, &["ps", "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let ps = json(&out);
        let warnings = ps["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1, "{ps}");
        let warning = &warnings[0];
        assert_eq!(warning["code"], "W_BORROW_BROKEN");
        assert_eq!(warning["details"]["borrow"], borrow_id);
        assert_eq!(warning["details"]["service"], target);
        let release = format!("taba release {borrow_id}");
        assert_eq!(warning["details"]["release_command"], release);
        assert!(warning["message"].as_str().unwrap().contains(&release));
    }

    #[test]
    fn successful_send_preserves_queued_borrow_warning() {
        send_with_queued_borrow_warning(true);
    }

    #[test]
    fn refused_send_preserves_queued_borrow_warning() {
        send_with_queued_borrow_warning(false);
    }

    #[test]
    fn a_piped_attachment_rechecks_permission_for_each_input() {
        struct ReleaseOnDrop<'a> {
            fixture: &'a Fixture,
            id: &'a str,
            armed: bool,
        }

        impl Drop for ReleaseOnDrop<'_> {
            fn drop(&mut self) {
                if self.armed {
                    // Release only this fixture's borrow before its owner stops the service.
                    let _ = self
                        .fixture
                        .cmd(&self.fixture.wt_b, &["release", self.id])
                        .output();
                }
            }
        }

        let f = Fixture::new("input-attach-permission");
        let svc = start(&f, &f.wt_a, "echo", true);
        let id = svc["service"].as_str().unwrap();
        let mut child = f
            .cmd(&f.wt_a, &["attach", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"first line\n").unwrap();
        wait_log(&f, &f.wt_a, id, "got:first line");
        let mut cleanup = ReleaseOnDrop {
            fixture: &f,
            id,
            armed: true,
        };
        let borrow = f.taba(&f.wt_b, &["borrow", id, "--mut"]);
        assert_eq!(exit(&borrow), 0, "{}", text(&borrow));
        stdin.write_all(b"second line\n").unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert!(!log(&f, &f.wt_a, id).contains("got:second line"));
        let release = f.taba(&f.wt_b, &["release", id]);
        assert_eq!(exit(&release), 0, "{}", text(&release));
        cleanup.armed = false;
    }

    fn assert_interactive_restart(out: &std::process::Output, id: &str) {
        let v = json(out);
        let next = v["error"]["next"].as_array().unwrap();
        assert!(
            next.iter().any(|action| {
                action["command"] == format!("taba stop {id}")
                    && action["do"]
                        .as_str()
                        .unwrap()
                        .contains("taba run --interactive")
            }),
            "{}",
            text(out)
        );
    }

    #[test]
    fn legacy_input_is_refused_with_restart_instructions() {
        let f = Fixture::new("input-legacy");
        let service = start(&f, &f.wt_a, "echo", true);
        let id = service["service"].as_str().unwrap();
        let path = record_path(&f, &service);
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["input"]
            .as_object_mut()
            .unwrap()
            .remove("fifo_identity")
            .unwrap();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let out = f.taba(&f.wt_a, &["send", id, "legacy input", "--json"]);
        assert_eq!(code(&out), "E_NO_INPUT");
        assert_interactive_restart(&out, id);
        assert!(!log(&f, &f.wt_a, id).contains("got:legacy input"));
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
        assert_eq!(row["interactive"], false);
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

    /// Like Flutter: single-key input and an observable receipt of each signal.
    const FAKE_FLUTTER: &str = r#"
trap 'printf "signal:USR1\n"' USR1
trap 'printf "signal:USR2\n"' USR2
printf 'Flutter run key commands.\n'
if [[ -t 0 ]]; then
    while :; do
        if IFS= read -r -n 1 key; then
            printf 'key:%s\n' "$key"
        fi
    done
else
    printf 'EOF\n'
    while :; do /bin/sleep 1; done
fi
"#;

    // Use system Bash on both Unix platforms. Preserve Dart's argv layout
    // without renaming/symlinking a Python executable or depending on PATH.
    fn flutter_launcher(f: &Fixture) -> String {
        let flutter = script_tool(
            f,
            "flutter",
            "/bin/bash",
            r#"
bin_dir=${0%/*}
exec -a "$bin_dir/dart" /bin/bash "$bin_dir/flutter_tools.snapshot" "$@"
"#,
        );
        std::fs::write(f.root.join("bin/flutter_tools.snapshot"), FAKE_FLUTTER).unwrap();
        flutter
    }

    /// Like `vite`: line-based shortcuts.
    const FAKE_VITE: &str = r#"
import sys
print("tty=%s" % sys.stdin.isatty(), flush=True)
print("VITE ready", flush=True)
for line in sys.stdin:
    print("shortcut:" + line.strip(), flush=True)
"#;

    #[test]
    fn flutter_reload_respects_the_platform_policy() {
        let f = Fixture::new("input-flutter");
        let flutter = flutter_launcher(&f);
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "app",
                "--wait",
                "--ready",
                "log:Flutter run key commands",
                "--",
                &flutter,
                "run",
                "-d",
                "chrome",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("interactive: yes"), "{stdout}");
        let id = format!("{}/app", f.scope_of(&f.wt_a));
        let row = f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap();
        assert_eq!(row["interactive"], true);

        // A single key arrives without Enter.
        assert_eq!(exit(&f.taba(&f.wt_a, &["send", &id, "--key", "r"])), 0);
        wait_log(&f, &f.wt_a, &id, "key:r");

        let reload = f.taba(&f.wt_a, &["reload", &id, "--json"]);
        let restart = f.taba(&f.wt_a, &["reload", &id, "--restart", "--json"]);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(exit(&reload), 0, "{}", text(&reload));
            let v = json(&reload);
            assert_eq!(v["kind"], "flutter");
            assert_eq!(v["method"], "SIGUSR1");
            wait_log(&f, &f.wt_a, &id, "signal:USR1");
            assert_eq!(exit(&restart), 0, "{}", text(&restart));
            assert_eq!(json(&restart)["method"], "SIGUSR2");
            wait_log(&f, &f.wt_a, &id, "signal:USR2");
        }
        #[cfg(target_os = "macos")]
        {
            for (out, key) in [(&reload, "r"), (&restart, "R")] {
                assert_eq!(exit(out), 3, "{}", text(out));
                let v = json(out);
                assert_eq!(v["error"]["code"], "E_NO_RELOAD");
                assert!(
                    v["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("taba attach")
                );
                assert_eq!(
                    v["error"]["next"][0]["command"],
                    format!("taba send {id} --key {key}")
                );
            }
            // Both keys still reach the same live instance. The fixture
            // logs either signal before consuming the next terminal input.
            for key in ["r", "R"] {
                let out = f.taba(&f.wt_a, &["send", &id, "--key", key, "--json"]);
                assert_eq!(exit(&out), 0, "{}", text(&out));
                assert_eq!(json(&out)["instance"], row["instance"]);
            }
            wait_log(&f, &f.wt_a, &id, "key:R");
            let log = log(&f, &f.wt_a, &id);
            assert!(!log.contains("signal:USR"), "{log}");
            let live = f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap();
            assert_eq!(live["status"], "running");
            assert_eq!(live["instance"], row["instance"]);
        }

        // Another scope may not reload it.
        let out = f.taba(&f.wt_b, &["reload", &id, "--json"]);
        assert_eq!(code(&out), "E_NOT_OWNER");

        // --no-interactive: stdin is /dev/null (EOF), so manual input is refused.
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "plain",
                "--no-interactive",
                "--wait",
                "--ready",
                "log:Flutter run key commands",
                "--json",
                "--",
                &flutter,
                "run",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["interactive"], false);
        let plain = format!("{}/plain", f.scope_of(&f.wt_a));
        wait_log(&f, &f.wt_a, &plain, "EOF");
        #[cfg(target_os = "macos")]
        {
            let out = f.taba(&f.wt_a, &["reload", &plain, "--json"]);
            assert_eq!(code(&out), "E_NO_RELOAD");
            assert_interactive_restart(&out, &plain);
        }
        let out = f.taba(&f.wt_a, &["send", &plain, "x", "--json"]);
        assert_eq!(code(&out), "E_NO_INPUT");
    }

    #[test]
    fn vite_is_interactive_by_default_and_reloads_with_r() {
        let f = Fixture::new("input-vite");
        let vite = fake_tool(&f, "vite", FAKE_VITE);
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "web",
                "--wait",
                "--ready",
                "log:VITE ready",
                "--json",
                "--",
                &vite,
                "--host",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["interactive"], true);
        let id = json(&out)["service"].as_str().unwrap().to_string();
        assert!(log(&f, &f.wt_a, &id).contains("tty=True"));
        let out = f.taba(&f.wt_a, &["reload", &id, "--json"]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        assert_eq!(json(&out)["kind"], "vite");
        assert_eq!(json(&out)["method"], "stdin");
        wait_log(&f, &f.wt_a, &id, "shortcut:r");

        // Something else: no reload method.
        let plain = start(&f, &f.wt_a, "plain", false);
        let out = f.taba(
            &f.wt_a,
            &["reload", plain["service"].as_str().unwrap(), "--json"],
        );
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NO_RELOAD");
    }

    #[test]
    fn preview_is_interactive_but_does_not_offer_reload() {
        let f = Fixture::new("input-preview");
        let vite = fake_tool(&f, "vite", FAKE_VITE);
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "preview",
                "--wait",
                "--ready",
                "log:VITE ready",
                "--json",
                "--",
                &vite,
                "preview",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let value = json(&out);
        assert_eq!(value["interactive"], true);
        let id = value["service"].as_str().unwrap();
        let out = f.taba(&f.wt_a, &["reload", id, "--json"]);
        assert_eq!(code(&out), "E_NO_RELOAD");
    }

    #[test]
    fn flutter_reload_refuses_a_non_flutter_wrapper_leader() {
        let f = Fixture::new("input-flutter-wrapper");
        let fvm = fake_tool(&f, "fvm", SLEEPER);
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "app",
                "--no-interactive",
                "--wait",
                "--ready",
                "log:ready",
                "--json",
                "--",
                &fvm,
                "flutter",
                "run",
            ],
        );
        assert_eq!(exit(&out), 0, "{}", text(&out));
        let value = json(&out);
        let id = value["service"].as_str().unwrap();
        let out = f.taba(&f.wt_a, &["reload", id, "--json"]);
        assert_eq!(exit(&out), 3, "{}", text(&out));
        assert_eq!(code(&out), "E_NO_RELOAD");
        assert_interactive_restart(&out, id);
        assert!(!log(&f, &f.wt_a, id).contains("usr1"));
    }

    #[test]
    fn a_failed_interactive_spawn_does_not_leave_a_fifo() {
        let f = Fixture::new("input-failed-start");
        let out = f.taba(
            &f.wt_a,
            &[
                "run",
                "--name",
                "broken",
                "--interactive",
                "--json",
                "--",
                "./missing-program",
            ],
        );
        assert_eq!(code(&out), "E_START_FAILED");
        let fifo = f
            .home()
            .join("inputs")
            .join(f.scope_of(&f.wt_a))
            .join("broken.fifo");
        assert!(!fifo.exists());
        assert!(f.ps(&[]).is_empty());
    }

    /// `logs -f` on a terminal: read-only until `i`, Ctrl+] stops typing,
    /// q quits; the service keeps running.
    #[test]
    fn the_log_view_types_only_in_input_mode() {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let f = Fixture::new("input-view");
        let svc = start(&f, &f.wt_a, "echo", true);
        let id = svc["service"].as_str().unwrap().to_string();

        let (mut master, slave) = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(m >= 0);
            assert_eq!(libc::grantpt(m), 0);
            assert_eq!(libc::unlockpt(m), 0);
            let name = std::ffi::CStr::from_ptr(libc::ptsname(m))
                .to_string_lossy()
                .into_owned();
            let master = std::fs::File::from_raw_fd(m);
            let slave = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
                .unwrap();
            (master, slave)
        };
        let mut child = f
            .cmd(&f.wt_a, &["logs", "-f", &id])
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap())
            .spawn()
            .unwrap();
        drop(slave);
        let screen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut reader = master.try_clone().unwrap();
        let sink = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let shown = |needle: &str| screen.lock().unwrap().contains(needle);
        wait_until("the read-only banner", || shown("read-only log of"));
        let mut key = |bytes: &[u8]| {
            master.write_all(bytes).unwrap();
            std::thread::sleep(Duration::from_millis(400));
        };
        key(b"nope\r"); // read-only: dropped
        key(b"i");
        wait_until("the input banner", || shown("INPUT MODE"));
        key(b"typed\r");
        wait_log(&f, &f.wt_a, &id, "got:typed");
        key(&[0x1d]); // Ctrl+]: back to read-only
        key(b"late\r");
        key(b"q");
        let status = child.wait().unwrap();
        assert!(status.success());
        let log = log(&f, &f.wt_a, &id);
        assert!(
            !log.contains("got:nope") && !log.contains("got:late"),
            "{log}"
        );
        assert!(shown("got:typed"), "the view shows the log");
        let row = f.ps(&[]).into_iter().find(|r| r["service"] == id).unwrap();
        assert_eq!(row["status"], "running");
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
