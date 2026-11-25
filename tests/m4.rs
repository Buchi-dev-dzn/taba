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
