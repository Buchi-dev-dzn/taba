//! Explicit scope and service hand-offs.
mod common;
use common::*;
use serde_json::Value;
use std::path::Path;

fn call(f: &Fixture, cwd: &Path, agent: &str, args: &[&str]) -> Value {
    json(
        &f.cmd(cwd, args)
            .env("TABA_AGENT", agent)
            .env("TABA_OUTPUT", "json")
            .output()
            .unwrap(),
    )
}

#[test]
fn give_and_forced_take_keep_instance() {
    let f = Fixture::new("move_scope");
    let web = call(&f, &f.wt_a, "first", &run_auto("web"));
    assert_eq!(web["ok"], true, "{web}");
    let hold = call(&f, &f.wt_a, "second", &["move", "--take"]);
    assert_eq!(hold["error"]["code"], "E_LEASE_LIVE");
    let missing = call(&f, &f.wt_a, "second", &["move", "--take", "--force"]);
    assert_eq!(missing["error"]["code"], "E_FORCE_NEEDS_REASON");
    let give = call(&f, &f.wt_a, "first", &["move", "--to", "second"]);
    assert_eq!(give["how"], "give", "{give}");
    assert_eq!(give["services"][0]["instance"], web["instance"]);
    let denied = call(&f, &f.wt_a, "first", &["stop", "web"]);
    assert_eq!(denied["error"]["code"], "E_SCOPE_HELD");
    let take = call(
        &f,
        &f.wt_a,
        "first",
        &["move", "--take", "--force", "--reason", "test"],
    );
    assert_eq!(take["how"], "take_forced", "{take}");
    assert!(http_ok(port_of(&web)));
    assert_eq!(call(&f, &f.wt_a, "first", &["stop", "web"])["ok"], true);
}

#[test]
fn service_move_preserves_instance_and_other_borrows() {
    let f = Fixture::new("move_service");
    let web = call(&f, &f.wt_a, "first", &run_auto("db"));
    let target = web["service"].as_str().unwrap();
    let b = call(&f, &f.wt_b, "second", &["borrow", target]);
    let c = call(
        &f,
        &f.outside,
        "third",
        &["borrow", target, "--scope", "third"],
    );
    let dest = f.scope_of(&f.wt_b);
    let denied = call(&f, &f.outside, "third", &["move", target, "--to", &dest]);
    assert_eq!(denied["error"]["code"], "E_NOT_OWNER");
    let mv = call(&f, &f.wt_a, "first", &["move", target, "--to", &dest]);
    assert_eq!(mv["instance"], web["instance"], "{mv}");
    assert_eq!(mv["borrows"]["ended"][0], b["borrow"]);
    let cid = c["borrow"].as_str().unwrap();
    let check = call(
        &f,
        &f.outside,
        "third",
        &["borrow", "--scope", "third", "--check", cid],
    );
    assert_eq!(check["service"], mv["service"], "{check}");
    assert!(http_ok(port_of(&web)));
    assert_eq!(
        call(
            &f,
            &f.outside,
            "third",
            &["release", "--scope", "third", cid]
        )["ok"],
        true
    );
    assert_eq!(call(&f, &f.wt_b, "second", &["stop", "db"])["ok"], true);
}
