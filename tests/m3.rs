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

/// A dummy service that exits `delay` seconds after a graceful stop request
/// (SIGTERM, or Ctrl+Break on Windows), writing `stopped`; a negative delay
/// ignores the request. It writes `ready` once its handler is installed.
const SLOW_STOP: &str = r#"
import pathlib, signal, sys, time
delay, dir = float(sys.argv[1]), pathlib.Path(sys.argv[2])
def on_stop(*_):
    if delay < 0:
        return
    time.sleep(delay)
    (dir / "stopped").write_text("graceful")
    sys.exit(0)
for name in ("SIGTERM", "SIGBREAK"):
    if hasattr(signal, name):
        signal.signal(getattr(signal, name), on_stop)
(dir / "ready").write_text("ready")
while True:
    time.sleep(0.05)
"#;

/// Starts the dummy as `first`'s service in `wt_a`, then ends `first`'s lease.
fn slow_service(f: &Fixture, delay: &str) -> (std::path::PathBuf, String) {
    let dir = f
        .root
        .join(format!("slow-{}", delay.replace(['-', '.'], "_")));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("slow.py");
    std::fs::write(&script, SLOW_STOP).unwrap();
    let mut first = Agent::start();
    let (script, dir_arg) = (script.to_str().unwrap(), dir.to_str().unwrap());
    let svc = first.call(
        f,
        &f.wt_a,
        "first",
        &[
            "run", "--name", "db", "--json", "--", PY, script, delay, dir_arg,
        ],
    );
    assert_eq!(svc["ok"], true, "{svc}");
    wait_until("handler installed", || dir.join("ready").exists());
    first.stop();
    assert_eq!(f.ps(&[])[0]["lease"]["state"], "expired");
    (dir, svc["service"].as_str().unwrap().to_string())
}

fn gc_drop(f: &Fixture, service: &str) -> Option<Value> {
    f.events()
        .into_iter()
        .find(|e| e["event"] == "gc_drop" && e["service"] == service)
}

#[test]
fn gc_waits_the_configured_stop_timeout_before_forcing() {
    let f = Fixture::new("gcterm");
    // stop_timeout defaults to 5 s, like `taba stop`.
    configure(&f, "gc_grace = \"0s\"\nclaim_grace = \"0s\"\n");
    let (dir, service) = slow_service(&f, "2");
    let gc = json(&f.taba(&f.outside, &["gc", "--json"]));
    assert_eq!(gc["dropped"][0]["services"][0], service.as_str(), "{gc}");
    assert!(
        dir.join("stopped").exists(),
        "the service exited on its own"
    );
    assert_eq!(gc_drop(&f, &service).unwrap()["signal"], "TERM");
}

#[test]
fn lazy_cleanup_does_not_wait_out_or_force_a_slow_service() {
    let f = Fixture::new("gclazy");
    configure(
        &f,
        "gc_grace = \"0s\"\nclaim_grace = \"0s\"\nstop_timeout = \"1m\"\n",
    );
    let other = Agent::start();
    let (dir, service) = slow_service(&f, "4");
    // Another scope's command runs the lazy pass: it asks, waits briefly and
    // leaves the service running instead of forcing it.
    let who = other.call(&f, &f.wt_b, "other", &["whoami"]);
    assert!(who["scope"].is_string(), "{who}");
    assert!(!dir.join("stopped").exists());
    assert!(f.ps(&[]).iter().any(|r| r["service"] == service.as_str()));
    assert!(gc_drop(&f, &service).is_none());
    wait_until("graceful exit", || dir.join("stopped").exists());
    other.call(&f, &f.wt_b, "other", &["whoami"]);
    assert!(!f.ps(&[]).iter().any(|r| r["service"] == service.as_str()));
    assert!(gc_drop(&f, &service).unwrap()["signal"].is_null());
}

#[test]
fn lazy_cleanup_forces_a_service_only_after_its_stop_timeout() {
    let f = Fixture::new("gcforce");
    configure(
        &f,
        "gc_grace = \"0s\"\nclaim_grace = \"0s\"\nstop_timeout = \"2s\"\n",
    );
    let other = Agent::start();
    // This dummy ignores the graceful request.
    let (_dir, service) = slow_service(&f, "-1");
    let started = std::time::Instant::now();
    other.call(&f, &f.wt_b, "other", &["whoami"]);
    if started.elapsed() < std::time::Duration::from_secs(2) {
        other.call(&f, &f.wt_b, "other", &["whoami"]);
        assert!(f.ps(&[]).iter().any(|r| r["service"] == service.as_str()));
        assert!(gc_drop(&f, &service).is_none());
    }
    std::thread::sleep(std::time::Duration::from_millis(2500));
    other.call(&f, &f.wt_b, "other", &["whoami"]);
    assert!(!f.ps(&[]).iter().any(|r| r["service"] == service.as_str()));
    assert_eq!(gc_drop(&f, &service).unwrap()["signal"], "KILL");
}
