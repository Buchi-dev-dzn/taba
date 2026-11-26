//! Integration tests for instance-bound borrows.
mod common;
use common::*;
use serde_json::Value;
use std::{path::Path, process::Stdio};

fn call(f: &Fixture, dir: &Path, args: &[&str]) -> Value {
    json(
        &f.cmd(dir, args)
            .env("TABA_OUTPUT", "json")
            .output()
            .unwrap(),
    )
}

#[test]
fn shared_borrow_protects_instance_and_forcing_breaks_it() {
    let f = Fixture::new("borrow_shared");
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let b = call(&f, &f.wt_b, &["borrow", target]);
    let id = b["borrow"].as_str().unwrap();
    assert_eq!(b["instance"], web["instance"]);
    assert_eq!(b["warnings"], serde_json::json!([]), "{b}");
    let denied = call(&f, &f.wt_a, &["stop", target]);
    assert_eq!(denied["error"]["code"], "E_BORROWED", "{denied}");
    assert!(http_ok(port_of(&web)));
    let stop = call(
        &f,
        &f.wt_a,
        &["stop", target, "--force", "--reason", "test"],
    );
    assert_eq!(stop["ok"], true, "{stop}");
    let check = call(&f, &f.wt_b, &["borrow", "--check", id]);
    assert_eq!(check["error"]["code"], "E_DANGLING", "{check}");
}

#[test]
fn same_scope_shared_borrow_warns_without_blocking_owner_stop() {
    let f = Fixture::new("borrow_same_scope");
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let b = call(&f, &f.wt_a, &["borrow", target]);
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["kind"], "shared");
    assert_eq!(b["instance"], web["instance"]);
    let id = b["borrow"].as_str().unwrap();
    let live_check = call(&f, &f.wt_a, &["borrow", "--check", id]);
    assert_eq!(live_check["ok"], true, "{live_check}");
    assert_eq!(live_check["warnings"][0]["code"], "W_SAME_SCOPE_BORROW");
    let stop = call(&f, &f.wt_a, &["stop", "db"]);
    assert_eq!(stop["ok"], true, "{stop}");
    let check = call(&f, &f.wt_a, &["borrow", "--check", id]);
    assert_eq!(check["error"]["code"], "E_DANGLING", "{check}");
    let warning = &b["warnings"][0];
    assert_eq!(warning["code"], "W_SAME_SCOPE_BORROW", "{b}");
    assert!(
        warning["message"]
            .as_str()
            .unwrap()
            .contains("does not prevent")
    );
    assert_eq!(warning["details"]["borrow"], id);
    assert_eq!(warning["details"]["service"], target);

    let web = call(&f, &f.wt_a, &run_auto("db"));
    let human = f.taba(&f.wt_a, &["borrow", web["service"].as_str().unwrap()]);
    assert!(human.status.success(), "{}", text(&human));
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(stderr.contains("same scope"), "{stderr}");
    assert!(stderr.contains("does not prevent"), "{stderr}");
    assert!(stderr.contains("owner"), "{stderr}");
    assert!(stderr.contains("stop"), "{stderr}");
    let exclusive = call(&f, &f.wt_a, &["borrow", "--mut", "db"]);
    assert_eq!(exclusive["ok"], true, "{exclusive}");
    assert_eq!(exclusive["kind"], "exclusive");
    assert_eq!(exclusive["warnings"], serde_json::json!([]));
    assert_eq!(
        call(
            &f,
            &f.wt_a,
            &["release", exclusive["borrow"].as_str().unwrap()]
        )["ok"],
        true
    );
}

#[test]
fn dangling_warnings_identify_each_borrow_and_release_cleans_up() {
    let f = Fixture::new("borrow_dangling_diagnostics");
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let first = call(&f, &f.wt_b, &["borrow", target]);
    let second = call(&f, &f.wt_b, &["borrow", target]);
    let ids = [
        first["borrow"].as_str().unwrap(),
        second["borrow"].as_str().unwrap(),
    ];
    assert_ne!(ids[0], ids[1]);
    let stop = call(
        &f,
        &f.wt_a,
        &["stop", target, "--force", "--reason", "test"],
    );
    assert_eq!(stop["ok"], true, "{stop}");
    let ps = call(&f, &f.wt_b, &["ps"]);
    let warnings = ps["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{ps}");
    for id in ids {
        let warning = warnings
            .iter()
            .find(|w| w["details"]["borrow"] == id)
            .unwrap_or_else(|| panic!("missing warning for {id}: {ps}"));
        assert_eq!(warning["code"], "W_BORROW_BROKEN");
        assert_eq!(warning["details"]["service"], target);
        let command = format!("taba release {id}");
        assert_eq!(warning["details"]["release_command"], command);
        let message = warning["message"].as_str().unwrap();
        assert!(
            message.contains(id) && message.contains(target) && message.contains(&command),
            "{message}"
        );
        assert_eq!(
            call(&f, &f.wt_b, &["borrow", "--check", id])["error"]["code"],
            "E_DANGLING"
        );
        let release = call(&f, &f.wt_b, &["release", id]);
        assert_eq!(release["released"][0], id, "{release}");
        assert_eq!(
            call(&f, &f.wt_b, &["borrow", "--check", id])["error"]["code"],
            "E_NOT_FOUND"
        );
    }
    assert_eq!(
        call(&f, &f.wt_b, &["ps"])["warnings"],
        serde_json::json!([])
    );
    assert!(!f.home().join("borrows").exists());
}

#[test]
fn release_removes_only_its_queued_dangling_warning() {
    let f = Fixture::new("borrow_release_notice");
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let first = call(&f, &f.wt_b, &["borrow", target]);
    let second = call(&f, &f.wt_b, &["borrow", target]);
    let first_id = first["borrow"].as_str().unwrap();
    let second_id = second["borrow"].as_str().unwrap();
    assert_eq!(
        call(
            &f,
            &f.wt_a,
            &["stop", target, "--force", "--reason", "test"]
        )["ok"],
        true
    );
    assert_eq!(call(&f, &f.wt_b, &["release", first_id])["ok"], true);
    let human = f.taba(&f.wt_b, &["ps"]);
    assert!(human.status.success(), "{}", text(&human));
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(!stderr.contains(first_id), "{stderr}");
    assert_eq!(stderr.matches("taba: warning:").count(), 1, "{stderr}");
    assert!(stderr.contains(target), "{stderr}");
    assert!(
        stderr.contains(&format!("taba release {second_id}")),
        "{stderr}"
    );
    assert_eq!(call(&f, &f.wt_b, &["release", second_id])["ok"], true);
    assert_eq!(
        call(&f, &f.wt_b, &["ps"])["warnings"],
        serde_json::json!([])
    );
}

#[test]
fn exclusive_command_releases_lock_and_waiter_can_acquire() {
    let f = Fixture::new("borrow_exclusive");
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let mut child = f
        .cmd(
            &f.wt_b,
            &[
                "borrow",
                "--mut",
                target,
                "--json",
                "--",
                PY,
                "-c",
                "import time; time.sleep(4)",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the exclusive lock", || {
        f.ps(&[])[0]["locked_by"].is_string()
    });
    let denied = call(
        &f,
        &f.outside,
        &["borrow", "--scope", "other", "--mut", target],
    );
    assert_eq!(denied["error"]["code"], "E_LOCKED", "{denied}");
    let next = call(
        &f,
        &f.outside,
        &[
            "borrow",
            "--scope",
            "other",
            "--mut",
            "--wait-lock",
            "10",
            target,
        ],
    );
    assert!(child.wait().unwrap().success());
    assert_eq!(next["ok"], true, "{next}");
    let res = call(
        &f,
        &f.outside,
        &[
            "release",
            "--scope",
            "other",
            next["borrow"].as_str().unwrap(),
        ],
    );
    assert_eq!(res["ok"], true, "{res}");
}

#[test]
fn collection_offers_borrowed_service_without_restarting() {
    let f = Fixture::new("borrow_gc");
    std::fs::create_dir_all(f.home()).unwrap();
    std::fs::write(
        f.home().join("config.toml"),
        "gc_grace=\"0s\"\nclaim_grace=\"0s\"\n",
    )
    .unwrap();
    let web = call(&f, &f.wt_a, &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let b = call(&f, &f.wt_b, &["borrow", target]);
    let c = call(&f, &f.outside, &["borrow", "--scope", "third", target]);
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(c["ok"], true, "{c}");
    assert_eq!(
        call(
            &f,
            &f.wt_a,
            &["lease", "end", "--scope", web["scope"].as_str().unwrap()]
        )["ok"],
        true
    );
    let res = call(&f, &f.outside, &["gc"]);
    assert_eq!(res["handed_over"][0]["how"], "borrower", "{res}");
    let next = call(
        &f,
        &f.outside,
        &[
            "borrow",
            "--scope",
            "third",
            "--check",
            c["borrow"].as_str().unwrap(),
        ],
    );
    assert_eq!(next["service"], res["handed_over"][0]["to"]);
    assert_eq!(next["instance"], web["instance"]);
    assert!(http_ok(port_of(&web)));
    assert_eq!(
        call(
            &f,
            &f.outside,
            &["release", "--scope", "third", c["borrow"].as_str().unwrap()]
        )["ok"],
        true
    );
}

/// A past service TTL is still reported, but a live borrower means collection
/// is not a certain next step. This is a pure `ps` observation: it must not
/// end the borrow or stop the instance.
#[test]
fn borrowed_service_ttl_is_expired_without_promising_gc() {
    let f = Fixture::new("borrow_ttl_visibility");
    let service = call(
        &f,
        &f.wt_a,
        &[
            "run",
            "--name",
            "db",
            "--service-ttl",
            "1s",
            "--json",
            "--",
            PY,
            "-c",
            SLEEP_30,
        ],
    );
    let target = service["service"].as_str().unwrap();
    let borrow = call(&f, &f.wt_b, &["borrow", target]);
    assert_eq!(borrow["ok"], true, "{borrow}");

    std::thread::sleep(std::time::Duration::from_millis(1200));
    let row = &f.ps(&[])[0];
    assert_eq!(row["service_ttl"], "1s");
    assert!(row["service_expires_at"].is_string());
    assert!(row["pending_gc"].is_null(), "{row}");
    assert_eq!(row["borrows"]["shared"], 1, "{row}");
}
