//! Agent leases, collection and claim hand-over.
mod common;
use common::*;
use serde_json::Value;
use std::{
    path::Path,
    process::{Child, Stdio},
};

struct Agent(Child);
impl Agent {
    fn start() -> Self {
        Self(
            std::process::Command::new(PY)
                .args(["-c", "import time; time.sleep(90)"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
    fn call(&self, f: &Fixture, cwd: &Path, name: &str, args: &[&str]) -> Value {
        let out = f
            .cmd(cwd, args)
            .env("TABA_AGENT", name)
            .env("TABA_AGENT_PID", self.0.id().to_string())
            .env("TABA_OUTPUT", "json")
            .output()
            .unwrap();
        json(&out)
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

fn configure(f: &Fixture, text: &str) {
    std::fs::create_dir_all(f.home()).unwrap();
    std::fs::write(f.home().join("config.toml"), text).unwrap();
}

#[test]
fn live_lease_refuses_scope_impersonation_and_dead_holder_is_collected() {
    let f = Fixture::new("held");
    configure(&f, "gc_grace = \"0s\"\nclaim_grace = \"0s\"\n");
    let mut first = Agent::start();
    let second = Agent::start();
    let web = first.call(&f, &f.wt_a, "first", &run_auto("web"));
    let port = port_of(&web);
    let target = web["service"].as_str().unwrap();
    let scope = web["scope"].as_str().unwrap();
    for (dir, args) in [
        (&f.wt_a, vec!["stop", target]),
        (&f.wt_b, vec!["stop", target, "--scope", scope]),
    ] {
        let denied = second.call(&f, dir, "second", &args);
        assert_eq!(denied["error"]["code"], "E_SCOPE_HELD", "{denied}");
        let denied = second.call(&f, dir, "first", &args);
        assert_eq!(denied["error"]["code"], "E_SCOPE_HELD", "{denied}");
    }
    first.stop();
    assert_eq!(f.ps(&[])[0]["lease"]["state"], "expired");
    let dry = json(&f.taba(&f.outside, &["gc", "--dry-run", "--json"]));
    assert_eq!(dry["dropped"][0]["services"][0], target);
    assert!(http_ok(port));
    let new = second.call(&f, &f.wt_b, "second", &run_auto("api"));
    assert_eq!(new["ok"], true, "{new}");
    assert!(!f.ps(&[]).iter().any(|r| r["service"] == target));
}

#[test]
fn claim_preserves_live_instance_and_skips_dead_claimant() {
    let f = Fixture::new("claim");
    configure(&f, "gc_grace = \"1h\"\nclaim_grace = \"0s\"\n");
    let mut first = Agent::start();
    let mut next = Agent::start();
    let third = Agent::start();
    let web = first.call(&f, &f.wt_a, "first", &run_auto("web"));
    let target = web["service"].as_str().unwrap();
    assert_eq!(
        next.call(&f, &f.wt_b, "next", &["claim", target])["position"],
        1
    );
    assert_eq!(
        third.call(
            &f,
            &f.outside,
            "third",
            &["claim", target, "--scope", "third"]
        )["position"],
        2
    );
    next.stop();
    first.stop();
    let gc = json(&f.taba(&f.outside, &["gc", "--json"]));
    assert_eq!(gc["handed_over"][0]["to"], "third/web", "{gc}");
    let p = f.ps(&[]);
    assert_eq!(p[0]["instance"], web["instance"]);
    assert_eq!(p[0]["scope"], "third");
    assert!(http_ok(port_of(&web)));
    let who = third.call(&f, &f.outside, "third", &["whoami", "--scope", "third"]);
    assert_eq!(who["warnings"][0]["code"], "W_HANDED_OVER", "{who}");
    assert!(f.events().iter().any(|e| e["event"] == "claim_skipped"));
}

#[test]
fn ttl_expiry_hands_over_even_while_holder_process_survives() {
    let f = Fixture::new("ttl");
    configure(&f, "lease_ttl = \"1h\"\n");
    let first = Agent::start();
    let second = Agent::start();
    let initial = first.call(&f, &f.wt_a, "first", &["whoami"]);
    assert_eq!(initial["lease"]["ttl_source"], "config");
    let mut args = run_auto("web");
    let i = args.iter().position(|a| *a == "--").unwrap();
    args.splice(i..i, ["--ttl", "2s"]);
    let web = first.call(&f, &f.wt_a, "first", &args);
    assert_eq!(web["ok"], true, "{web}");
    assert_eq!(f.ps(&[])[0]["lease"]["ttl_source"], "flag");
    wait_until("expired lease", || {
        f.ps(&[])[0]["lease"]["state"] == "expired"
    });
    let who = second.call(&f, &f.wt_a, "second", &["whoami"]);
    assert_eq!(who["takeover"]["why"], "ttl_expired", "{who}");
    assert_eq!(who["holder"], "second");
    assert_eq!(f.ps(&[])[0]["instance"], web["instance"]);
}
