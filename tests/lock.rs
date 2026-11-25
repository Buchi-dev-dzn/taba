//! The global state lock: a bounded wait that ends in `E_BUSY`, and `ps`
//! never waiting for it (issue #46).
mod common;
use common::*;
use std::fs::{File, OpenOptions};
use std::time::{Duration, Instant};

/// Holds `$TABA_HOME/lock` from the test process, as a stuck or suspended
/// taba would, and records `pid` as its holder.
fn hold(f: &Fixture, pid: u32) -> File {
    std::fs::create_dir_all(f.home()).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(f.home().join("lock"))
        .unwrap();
    file.lock().unwrap();
    std::fs::write(f.home().join("lock.holder"), format!("{pid}\n")).unwrap();
    file
}

/// A mutating command gives up after `$TABA_LOCK_TIMEOUT` with `E_BUSY`
/// (exit 3) naming the recorded holder, instead of hanging.
#[test]
fn lock_busy_times_out_with_e_busy() {
    let f = Fixture::new("lock-busy");
    f.scope_of(&f.wt_a); // creates the state directory
    let lock = hold(&f, 4242);
    let start = Instant::now();
    let out = f
        .cmd(&f.wt_a, &["whoami", "--json"])
        .env("TABA_LOCK_TIMEOUT", "0.5")
        .output()
        .unwrap();
    let took = start.elapsed();
    assert_eq!(exit(&out), 3, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_BUSY", "{v}");
    assert_eq!(v["error"]["details"]["holder_pid"], 4242, "{v}");
    assert!(
        took >= Duration::from_millis(500),
        "gave up early: {took:?}"
    );
    assert!(took < Duration::from_secs(10), "waited too long: {took:?}");
    // Not on a terminal: no waiting notice on stderr.
    assert!(!String::from_utf8_lossy(&out.stderr).contains("waiting"));

    // Once the holder lets go, the same command succeeds.
    drop(lock);
    let out = f
        .cmd(&f.wt_a, &["whoami", "--json"])
        .env("TABA_LOCK_TIMEOUT", "0.5")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
}

/// `ps` (and its lease renewal) does not wait for a held lock.
#[test]
fn lock_busy_ps_does_not_wait() {
    let f = Fixture::new("lock-ps");
    f.scope_of(&f.wt_a);
    let _lock = hold(&f, 4242);
    for dir in [&f.wt_a, &f.outside] {
        let start = Instant::now();
        let out = f
            .cmd(dir, &["ps", "--json"])
            .env("TABA_LOCK_TIMEOUT", "30")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        assert!(json(&out)["services"].is_array());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "ps waited for the lock: {:?}",
            start.elapsed()
        );
    }
}

/// An invalid `$TABA_LOCK_TIMEOUT` is `E_CONFIG`, not a silent default.
#[test]
fn lock_timeout_invalid_is_e_config() {
    let f = Fixture::new("lock-config");
    let out = f
        .cmd(&f.wt_a, &["whoami", "--json"])
        .env("TABA_LOCK_TIMEOUT", "soon")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 1, "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "E_CONFIG", "{v}");
    assert_eq!(v["error"]["details"]["key"], "TABA_LOCK_TIMEOUT", "{v}");
}
