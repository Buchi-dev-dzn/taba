//! Issue #45: size-capped service logs and `events.jsonl`, each with one
//! rotated `.1` file, without breaking `logs -f`, `logs --tail` or a running
//! service's open log handle.

mod common;
use common::*;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

fn configure(f: &Fixture, text: &str) {
    std::fs::create_dir_all(f.home()).unwrap();
    std::fs::write(f.home().join("config.toml"), text).unwrap();
}

fn rotated(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.1", p.display()))
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

/// Prints numbered lines until a `stop` file appears in its directory, then
/// prints `DONE` and exits.
const WRITER: &str = "import os, time
i = 0
while not os.path.exists('stop'):
    print('line %06d %s' % (i, 'x' * 50), flush=True)
    i += 1
    time.sleep(0.005)
print('DONE', flush=True)
";

/// Prints `hi`, then sleeps.
const HI: &str = "import time; print('hi', flush=True); time.sleep(30)";

/// The line numbers in `text`, in order.
fn numbers(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| l.strip_prefix("line "))
        .filter_map(|l| l.get(..6)?.parse().ok())
        .collect()
}

#[test]
fn running_service_keeps_logging_and_followers_keep_following_across_rotation() {
    let f = Fixture::new("rotate-running");
    let a = &f.wt_a;
    configure(&f, "log_max_size = \"4KB\"\n");
    let run = json(&f.taba(a, &["run", "--name", "w", "--json", "--", PY, "-c", WRITER]));
    assert_eq!(run["ok"], true, "{run}");
    let log = PathBuf::from(run["log_path"].as_str().unwrap());

    let mut follow = f
        .cmd(a, &["logs", "w", "-f"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_until("the log to pass the cap", || len(&log) > 8 * 1024);
    let out = f.taba(a, &["gc", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    let old = std::fs::read(rotated(&log)).expect("a rotated .1 file");
    assert!(old.len() > 8 * 1024, "{}", old.len());
    assert!(
        String::from_utf8_lossy(&old).starts_with("--- taba:"),
        "the .1 file holds the start of the log"
    );
    let last_old = *numbers(&String::from_utf8_lossy(&old)).last().unwrap();

    // The service writes on through its inherited handle, at the start of
    // the truncated file: no hole of zeros, no write into the `.1` file.
    wait_until("new output after the rotation", || {
        numbers(&String::from_utf8_lossy(&std::fs::read(&log).unwrap())).len() >= 5
    });
    let now = std::fs::read(&log).unwrap();
    assert!(!now.contains(&0), "the truncated log has a hole");
    assert!(len(&log) < 8 * 1024 + 4096, "the log was not truncated");
    let first_new = numbers(&String::from_utf8_lossy(&now))[0];
    assert!(first_new > last_old, "{first_new} after {last_old}");
    assert_eq!(std::fs::read(rotated(&log)).unwrap().len(), old.len());

    // A second rotation, from the lazy cleanup inside `whoami`.
    wait_until("the log to pass the cap again", || len(&log) > 4 * 1024);
    let out = f.taba(a, &["whoami", "--json"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    wait_until("the second rotation's output", || len(&log) > 0);
    let second = std::fs::read(rotated(&log)).unwrap();
    assert!(numbers(&String::from_utf8_lossy(&second))[0] >= first_new);

    std::fs::write(a.join("stop"), "").unwrap();
    wait_until("the service to exit", || {
        f.ps(&[]).iter().any(|s| s["status"] == "exited")
    });
    let mut seen = String::new();
    thread::sleep(Duration::from_millis(600));
    let status = follow.wait().unwrap();
    follow
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut seen)
        .unwrap();
    assert!(status.success());
    assert!(seen.contains("DONE"), "logs -f stopped following: {seen}");
    let seen = numbers(&seen);
    assert!(seen.contains(&first_new), "logs -f missed the restart");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "logs -f repeated lines"
    );

    // --tail reads the current file.
    let out = f.taba(a, &["logs", "w", "--tail", "1"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "DONE");
    let out = json(&f.taba(a, &["logs", "w", "--json"]));
    assert_eq!(out["bytes_total"], len(&log));

    // rm deletes the rotated file too.
    let out = f.taba(a, &["rm", "w"]);
    assert_eq!(exit(&out), 0, "{}", text(&out));
    assert!(!log.exists() && !rotated(&log).exists());
}

#[test]
fn run_rotates_a_full_log_before_starting_and_defaults_keep_rotation_off() {
    let f = Fixture::new("rotate-run");
    let a = &f.wt_a;
    let start = |name: &str| {
        let v = json(&f.taba(a, &["run", "--name", name, "--json", "--", PY, "-c", HI]));
        assert_eq!(v["ok"], true, "{v}");
        let log = PathBuf::from(v["log_path"].as_str().unwrap());
        wait_until("its output", || {
            String::from_utf8_lossy(&std::fs::read(&log).unwrap()).contains("hi")
        });
        let out = f.taba(a, &["stop", name]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
        log
    };

    // No config: rotation is opt-in, including for a log already past the
    // documented 10 MiB example cap.
    let log = start("web");
    let big = vec![b'z'; 2000];
    std::fs::write(&log, &big).unwrap();
    start("web");
    assert!(!rotated(&log).exists());
    assert!(std::fs::read(&log).unwrap().starts_with(&big));

    configure(&f, "log_max_size = \"1KB\"\n");
    std::fs::write(&log, &big).unwrap();
    start("web");
    assert_eq!(std::fs::read(rotated(&log)).unwrap(), big);
    let now = String::from_utf8_lossy(&std::fs::read(&log).unwrap()).into_owned();
    assert!(now.starts_with("--- taba:"), "{now}");
    assert!(now.contains("hi"), "{now}");

    // `none` turns rotation off.
    configure(&f, "log_max_size = \"none\"\n");
    std::fs::write(&log, &big).unwrap();
    start("web");
    assert!(std::fs::read(&log).unwrap().starts_with(&big));

    // A bad size is E_CONFIG naming the key, as for the other settings.
    configure(&f, "log_max_size = \"lots\"\n");
    let out = f.taba(a, &["whoami", "--json"]);
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_CONFIG", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("log_max_size")
    );
}

#[test]
fn events_jsonl_rotates_at_its_cap() {
    let f = Fixture::new("rotate-events");
    let a = &f.wt_a;
    configure(&f, "events_max_size = \"1KB\"\n");
    let events = f.home().join("events.jsonl");
    let filler = format!(
        "{}\n",
        serde_json::json!({ "event": "filler", "pad": "p".repeat(1100) })
    );
    std::fs::write(&events, &filler).unwrap();

    // Any event: a new scope's lease is `created` by its first `run`.
    let run = |dir: &Path| {
        let out = f.taba(dir, &["run", "--name", "web", "--json", "--", PY, "-c", HI]);
        assert_eq!(exit(&out), 0, "{}", text(&out));
    };
    run(a);
    assert_eq!(std::fs::read_to_string(rotated(&events)).unwrap(), filler);
    let now = f.events();
    assert!(
        !now.is_empty() && now.iter().all(|e| e["event"] != "filler"),
        "{now:?}"
    );

    // Under the cap nothing moves.
    let kept = std::fs::read_to_string(&events).unwrap();
    run(&f.wt_b);
    let after = std::fs::read_to_string(&events).unwrap();
    assert!(after.starts_with(&kept) && after.len() > kept.len());
    assert_eq!(std::fs::read_to_string(rotated(&events)).unwrap(), filler);
}
