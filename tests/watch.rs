//! `taba ps --watch --json` (issue #14): NDJSON snapshots on change.

mod common;

use common::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

/// A running `taba ps --watch --json`, its stdout lines on a channel.
struct Watch {
    child: Child,
    lines: Receiver<String>,
}

fn spawn_watch(fx: &Fixture, cwd: &Path, extra: &[&str]) -> Child {
    let mut args = vec!["ps", "--watch", "--json", "--interval", "250ms"];
    args.extend_from_slice(extra);
    let mut cmd = fx.cmd(cwd, &args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // Its own (hidden) console and process group, so the test can send
        // it Ctrl+Break like a terminal would.
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().expect("spawn taba ps --watch")
}

impl Watch {
    fn start(fx: &Fixture, cwd: &Path, extra: &[&str]) -> Watch {
        let mut child = spawn_watch(fx, cwd, extra);
        let out = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Watch { child, lines }
    }

    /// The next snapshot line, parsed.
    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(20))
            .expect("a snapshot line");
        assert!(!line.trim().is_empty(), "empty line");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// True if no line arrives within `d`.
    fn quiet(&self, d: Duration) -> bool {
        matches!(self.lines.recv_timeout(d), Err(RecvTimeoutError::Timeout))
    }

    /// Reads lines until one satisfies `f`; every line differs from the one
    /// before it.
    fn until(&self, prev: &mut Value, what: &str, f: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let v = self.next();
            assert_eq!(v["ok"], true, "{v}");
            assert_ne!(v["services"], prev["services"], "a line without a change");
            *prev = v;
            if f(prev) {
                return;
            }
        }
        panic!("timed out waiting for {what}");
    }

    fn interrupt_and_wait(mut self) -> i32 {
        interrupt(&self.child);
        wait_exit(&mut self.child)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// SIGINT on Unix; Ctrl+Break to its console process group on Windows.
fn interrupt(child: &Child) {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        assert_eq!(rc, 0);
    }
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let st = std::process::Command::new(BIN)
            .args(["__ctrl-break", &pid, &pid])
            .status()
            .unwrap();
        assert!(st.success(), "ctrl-break helper: {st:?}");
    }
}

fn wait_exit(child: &mut Child) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code().expect("exited normally");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("taba ps --watch did not exit");
}

fn run_sleeper(fx: &Fixture, cwd: &Path, name: &str) {
    let out = fx.taba(
        cwd,
        &["run", "--name", name, "--json", "--", PY, "-c", SLEEP_30],
    );
    assert_eq!(exit(&out), 0, "{}", text(&out));
}

#[test]
fn streams_full_snapshots_only_on_change() {
    let fx = Fixture::new("watch_stream");
    run_sleeper(&fx, &fx.wt_a, "sleeper");

    let w = Watch::start(&fx, &fx.outside, &[]);
    let mut prev = w.next();
    assert_eq!(prev["ok"], true);
    assert_eq!(prev["warnings"], serde_json::json!([]));
    // The same rows as `ps --json`.
    assert_eq!(prev["services"], Value::Array(fx.ps(&[])));
    assert_eq!(prev["services"][0]["status"], "running");
    // Nothing changes: nothing is printed.
    assert!(
        w.quiet(Duration::from_millis(1500)),
        "a line without a change"
    );

    let out = fx.taba(&fx.wt_a, &["stop", "sleeper", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    w.until(&mut prev, "status exited", |v| {
        v["services"][0]["status"] == "exited"
    });
    assert_eq!(prev["services"], Value::Array(fx.ps(&[])));

    let out = fx.taba(&fx.wt_a, &["rm", "sleeper", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    w.until(&mut prev, "the service to go", |v| {
        v["services"].as_array().is_some_and(Vec::is_empty)
    });

    run_sleeper(&fx, &fx.wt_b, "again");
    w.until(&mut prev, "a new service", |v| {
        v["services"][0]["name"] == "again"
    });
    assert_eq!(w.interrupt_and_wait(), 0);
}

#[test]
fn filters_by_scope_and_running() {
    let fx = Fixture::new("watch_filters");
    run_sleeper(&fx, &fx.wt_a, "a1");
    run_sleeper(&fx, &fx.wt_b, "b1");
    run_sleeper(&fx, &fx.wt_b, "b2");
    let out = fx.taba(&fx.wt_b, &["stop", "b2", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let scope_b = fx.scope_of(&fx.wt_b);

    let w = Watch::start(&fx, &fx.outside, &["--scope", &scope_b]);
    let v = w.next();
    assert_eq!(v["services"], Value::Array(fx.ps(&["--scope", &scope_b])));
    assert_eq!(v["services"].as_array().unwrap().len(), 2);
    assert_eq!(w.interrupt_and_wait(), 0);

    let w = Watch::start(&fx, &fx.outside, &["--running"]);
    let v = w.next();
    assert_eq!(v["services"], Value::Array(fx.ps(&["--running"])));
    let names: Vec<&str> = v["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a1", "b1"]);
    assert_eq!(w.interrupt_and_wait(), 0);
}

#[test]
fn exits_zero_when_stdout_is_closed() {
    let fx = Fixture::new("watch_closed");
    let mut child = spawn_watch(&fx, &fx.outside, &[]);
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let v: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["ok"], true);
    drop(reader); // the consumer goes away
    // A change makes the next write fail on every platform.
    run_sleeper(&fx, &fx.wt_a, "s");
    assert_eq!(wait_exit(&mut child), 0);
}

#[test]
fn rejects_bad_usage() {
    let fx = Fixture::new("watch_usage");
    // Without --json: rejected for now (exit 2), with a text message.
    let out = fx.taba(&fx.outside, &["ps", "--watch"]);
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--watch needs --json"),
        "{}",
        text(&out)
    );
    assert!(out.stdout.is_empty());
    for args in [
        &["ps", "--watch", "--json", "--interval", "100ms"][..],
        &["ps", "--watch", "--json", "--interval", "soon"],
        &["ps", "--watch", "--json", "--all"],
        &["ps", "--json", "--interval", "1"],
    ] {
        let out = fx.taba(&fx.outside, args);
        assert_eq!(exit(&out), 2, "{args:?}: {}", text(&out));
        assert_eq!(json(&out)["error"]["code"], "E_USAGE", "{args:?}");
    }
    // An invalid scope: one compact JSON line, exit 2.
    let out = fx.taba(&fx.outside, &["ps", "--watch", "--json", "--scope", "../x"]);
    assert_eq!(exit(&out), 2, "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert_eq!(json(&out)["error"]["code"], "E_INVALID_IDENT");
}

#[test]
fn taba_output_json_is_enough() {
    let fx = Fixture::new("watch_env");
    let mut cmd = fx.cmd(&fx.outside, &["ps", "--watch", "--interval", "0.25"]);
    cmd.env("TABA_OUTPUT", "json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let v: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["services"], serde_json::json!([]));
    let _ = child.kill();
    let _ = child.wait();
}

/// Unlike `ps`, watching never renews the caller's lease (or writes state).
#[test]
fn never_changes_state() {
    let fx = Fixture::new("watch_readonly");
    run_sleeper(&fx, &fx.wt_a, "s");
    let scope = fx.scope_of(&fx.wt_a);
    let rec_path = fx.home().join("scopes").join(format!("{scope}.json"));
    let mut rec: Value = serde_json::from_slice(&std::fs::read(&rec_path).unwrap()).unwrap();
    // A lease renewed long ago: `ps` in this scope would renew it now.
    rec["lease"]["renewed_at"] = "2020-01-01T00:00:00Z".into();
    std::fs::write(&rec_path, serde_json::to_vec_pretty(&rec).unwrap()).unwrap();
    let before = std::fs::read(&rec_path).unwrap();
    let events = fx.events();

    let w = Watch::start(&fx, &fx.wt_a, &[]);
    let v = w.next();
    assert_eq!(v["services"][0]["name"], "s");
    assert!(w.quiet(Duration::from_millis(1200)));
    assert_eq!(w.interrupt_and_wait(), 0);
    assert_eq!(std::fs::read(&rec_path).unwrap(), before);
    assert_eq!(fx.events(), events);

    // The check is meaningful: plain `ps` in that scope does renew it.
    let out = fx.taba(&fx.wt_a, &["ps", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_ne!(std::fs::read(&rec_path).unwrap(), before);
}
