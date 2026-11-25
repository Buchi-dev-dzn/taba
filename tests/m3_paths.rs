//! Documented M3 paths without an acceptance test before (issue #40): TTL
//! precedence and invalid configuration (design tests 5 and 7),
//! `--service-ttl` with `keep` (3), `log_retention`, `claim --cancel` (11),
//! successor first (10), `disown` and `E_NO_CLAIMANT` (12), and
//! `W_BRANCH_CHANGED`. Every test uses a throwaway `TABA_HOME`, throwaway git
//! worktrees and dummy processes it starts itself.
mod common;
use common::*;
use serde_json::Value;
use std::{
    path::Path,
    process::{Child, Stdio},
    time::Duration,
};

/// A stand-in agent process for `TABA_AGENT_PID`; killed on drop.
struct Agent(Child);
impl Agent {
    fn start() -> Self {
        Self(
            std::process::Command::new(PY)
                .args(["-c", "import time; time.sleep(120)"])
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
    fn cmd(&self, f: &Fixture, cwd: &Path, name: &str, args: &[&str]) -> std::process::Command {
        let mut c = f.cmd(cwd, args);
        c.env("TABA_AGENT", name)
            .env("TABA_AGENT_PID", self.0.id().to_string())
            .env("TABA_OUTPUT", "json")
            .env_remove("TABA_TTL");
        c
    }
    fn call(&self, f: &Fixture, cwd: &Path, name: &str, args: &[&str]) -> Value {
        json(&self.cmd(f, cwd, name, args).output().unwrap())
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

/// A long-running dummy service: `run --name NAME --json [extra] -- python -c sleep`.
fn run_sleeper<'a>(name: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["run", "--name", name, "--json"];
    v.extend_from_slice(extra);
    v.extend_from_slice(&["--", PY, "-c", "import time; time.sleep(120)"]);
    v
}

fn service<'a>(ps: &'a [Value], id: &str) -> Option<&'a Value> {
    ps.iter().find(|r| r["service"] == id)
}

fn has_event(f: &Fixture, event: &str, pred: impl Fn(&Value) -> bool) -> bool {
    f.events().iter().any(|e| e["event"] == event && pred(e))
}

fn codes(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .map(|x| x["code"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------ test 5

#[test]
fn ttl_precedence_flag_over_env_over_config_over_default() {
    let f = Fixture::new("ttlprec");
    let agent = Agent::start();
    // No config file and no TABA_TTL: 4h for an agent, none for a human.
    let who = agent.call(&f, &f.wt_a, "a1", &["whoami"]);
    assert_eq!(who["lease"]["ttl"], "4h", "{who}");
    assert_eq!(who["lease"]["ttl_source"], "default", "{who}");
    let human = json(
        &f.cmd(&f.wt_a, &["whoami", "--json"])
            .env_remove("TABA_TTL")
            .output()
            .unwrap(),
    );
    assert_eq!(human["agent"], "human", "{human}");
    assert_eq!(human["lease"]["ttl"], "none", "{human}");
    assert_eq!(human["lease"]["ttl_source"], "default", "{human}");

    configure(&f, "lease_ttl = \"1h\"\n");
    let who = agent.call(&f, &f.wt_a, "a1", &["whoami"]);
    assert_eq!(who["lease"]["ttl"], "1h", "{who}");
    assert_eq!(who["lease"]["ttl_source"], "config", "{who}");

    let env = |args: &[&str]| {
        json(
            &agent
                .cmd(&f, &f.wt_a, "a1", args)
                .env("TABA_TTL", "2h")
                .output()
                .unwrap(),
        )
    };
    let who = env(&["whoami"]);
    assert_eq!(who["lease"]["ttl"], "2h", "{who}");
    assert_eq!(who["lease"]["ttl_source"], "env", "{who}");

    let run = env(&run_sleeper("web", &["--ttl", "30m"]));
    assert_eq!(run["ok"], true, "{run}");
    let row = f.ps(&[]);
    let row = service(&row, run["service"].as_str().unwrap()).expect("listed");
    assert_eq!(row["lease"]["ttl"], "30m", "{row}");
    assert_eq!(row["lease"]["ttl_source"], "flag", "{row}");
    assert_eq!(row["lease"]["state"], "live", "{row}");
    let who = env(&["whoami"]);
    assert_eq!(who["lease"]["ttl"], "30m", "{who}");
    assert_eq!(who["lease"]["ttl_source"], "flag", "{who}");
}

// ------------------------------------------------------------ test 7

#[test]
fn zero_config_works_and_invalid_ttl_sources_are_named() {
    let f = Fixture::new("badcfg");
    // Zero config: run, ps, whoami, gc and stop work with no config.toml.
    assert!(!f.home().join("config.toml").exists());
    let run = json(&f.taba(&f.wt_a, &run_sleeper("web", &[])));
    assert_eq!(run["ok"], true, "{run}");
    assert_eq!(f.ps(&[]).len(), 1);
    for args in [
        &["whoami", "--json"][..],
        &["gc", "--dry-run", "--json"],
        &["gc", "--json"],
    ] {
        let out = f.taba(&f.wt_a, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        assert_eq!(json(&out)["ok"], true);
    }
    let stop = f.taba(&f.wt_a, &["stop", "web", "--json"]);
    assert!(stop.status.success(), "{}", text(&stop));

    // An invalid lease_ttl in config.toml: E_CONFIG naming the file and key.
    configure(&f, "lease_ttl = \"soon\"\n");
    let agent = Agent::start();
    let out = agent.cmd(&f, &f.wt_a, "a1", &["whoami"]).output().unwrap();
    assert_eq!(exit(&out), 1, "{}", text(&out));
    let err = &json(&out)["error"];
    assert_eq!(err["code"], "E_CONFIG", "{err}");
    assert_eq!(err["details"]["source"], "config", "{err}");
    assert_eq!(err["details"]["key"], "lease_ttl", "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("config.toml"),
        "{err}"
    );
    let out = agent
        .cmd(&f, &f.wt_a, "a1", &run_sleeper("api", &[]))
        .output()
        .unwrap();
    assert_eq!(json(&out)["error"]["code"], "E_CONFIG", "{}", text(&out));
    let no_api = || !f.ps(&[]).iter().any(|r| r["name"] == "api");
    assert!(no_api(), "nothing started with a bad config");

    // Other keys and malformed lines are E_CONFIG too.
    for (body, key) in [
        ("gc_grace = \"later\"\n", "gc_grace"),
        ("claim_grace = 5\n", "claim_grace"),
        ("lease_ttl\n", "line 1"),
    ] {
        configure(&f, body);
        let v = agent.call(&f, &f.wt_a, "a1", &["gc"]);
        assert_eq!(v["error"]["code"], "E_CONFIG", "{body}: {v}");
        assert_eq!(v["error"]["details"]["key"], key, "{body}: {v}");
    }

    // TABA_TTL=soon: E_CONFIG naming the environment variable.
    std::fs::remove_file(f.home().join("config.toml")).unwrap();
    let out = agent
        .cmd(&f, &f.wt_a, "a1", &["whoami"])
        .env("TABA_TTL", "soon")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 1, "{}", text(&out));
    let err = &json(&out)["error"];
    assert_eq!(err["code"], "E_CONFIG", "{err}");
    assert_eq!(err["details"]["source"], "env", "{err}");
    assert_eq!(err["details"]["key"], "TABA_TTL", "{err}");

    // --ttl soon is a usage error, not a configuration error.
    let out = agent
        .cmd(&f, &f.wt_a, "a1", &run_sleeper("api", &["--ttl", "soon"]))
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert_eq!(json(&out)["error"]["code"], "E_USAGE", "{}", text(&out));
    assert!(no_api());
}

// ------------------------------------------------------------ test 3 and log_retention

#[test]
fn service_ttl_is_collected_by_gc_unless_the_scope_is_kept() {
    let f = Fixture::new("svcttl");
    configure(&f, "log_retention = \"1h\"\n");
    // A human's lease never expires by time, so only --service-ttl applies.
    let kept = json(&f.taba(&f.wt_b, &run_sleeper("db", &["--service-ttl", "2s"])));
    assert_eq!(kept["ok"], true, "{kept}");
    let keep = f.taba(&f.wt_b, &["keep", "--json"]);
    assert!(keep.status.success(), "{}", text(&keep));
    let run = json(&f.taba(&f.wt_a, &run_sleeper("web", &["--service-ttl", "2s"])));
    assert_eq!(run["ok"], true, "{run}");
    let web = run["service"].as_str().unwrap().to_string();
    let db = kept["service"].as_str().unwrap().to_string();

    // Before the service TTL passes, gc leaves it alone.
    let gc = json(&f.taba(&f.outside, &["gc", "--dry-run", "--json"]));
    assert!(
        !gc["dropped"].to_string().contains(&web),
        "not due yet: {gc}"
    );
    std::thread::sleep(Duration::from_millis(3000));

    let dry = json(&f.taba(&f.outside, &["gc", "--dry-run", "--json"]));
    assert_eq!(dry["dropped"][0]["services"][0], web.as_str(), "{dry}");
    assert_eq!(dry["dropped"][0]["reason"], "service_ttl_expired", "{dry}");
    assert!(service(&f.ps(&[]), &web).is_some(), "dry run stops nothing");

    let gc = json(&f.taba(&f.outside, &["gc", "--json"]));
    assert_eq!(gc["dropped"][0]["services"][0], web.as_str(), "{gc}");
    assert!(
        gc["kept"].to_string().contains("keep"),
        "the kept scope is reported: {gc}"
    );
    let ps = f.ps(&[]);
    assert!(service(&ps, &web).is_none(), "{ps:?}");
    let db_row = service(&ps, &db).expect("kept service survives gc");
    assert_eq!(db_row["status"], "running", "{db_row}");
    assert!(has_event(&f, "gc_drop", |e| e["service"] == web.as_str()
        && e["reason"] == "service_ttl_expired"));

    // log_retention: the dropped service's log stays until it is older than
    // the configured retention, and a later gc removes it.
    let (scope, name) = web.split_once('/').unwrap();
    let log = f
        .home()
        .join("logs")
        .join(scope)
        .join(format!("{name}.log"));
    assert!(log.exists(), "gc keeps the log of a dropped service");
    json(&f.taba(&f.outside, &["gc", "--json"]));
    assert!(log.exists(), "younger than log_retention");
    configure(&f, "log_retention = \"0s\"\n");
    json(&f.taba(&f.outside, &["gc", "--json"]));
    assert!(!log.exists(), "gc removes logs past log_retention");
    // The kept service's own log is not an orphan and stays.
    let (scope, name) = db.split_once('/').unwrap();
    assert!(
        f.home()
            .join("logs")
            .join(scope)
            .join(format!("{name}.log"))
            .exists()
    );
}

// ------------------------------------------------------------ test 11

#[test]
fn claim_cancel_removes_the_claim_and_own_scope_claim_is_usage() {
    let f = Fixture::new("cancel");
    configure(&f, "gc_grace = \"0s\"\nclaim_grace = \"0s\"\n");
    let mut holder = Agent::start();
    let next = Agent::start();
    let web = holder.call(&f, &f.wt_a, "a1", &run_sleeper("web", &[]));
    assert_eq!(web["ok"], true, "{web}");
    let target = web["service"].as_str().unwrap().to_string();

    let own = holder
        .cmd(&f, &f.wt_a, "a1", &["claim", &target])
        .output()
        .unwrap();
    assert_eq!(exit(&own), 2, "{}", text(&own));
    assert_eq!(json(&own)["error"]["code"], "E_USAGE", "{}", text(&own));

    let claim = next.call(&f, &f.wt_b, "b1", &["claim", &target]);
    assert_eq!(claim["position"], 1, "{claim}");
    assert_eq!(f.ps(&[])[0]["claims"].as_array().unwrap().len(), 1);

    let cancel = next.call(&f, &f.wt_b, "b1", &["claim", &target, "--cancel"]);
    assert_eq!(cancel["ok"], true, "{cancel}");
    assert!(cancel["position"].is_null(), "{cancel}");
    assert_eq!(cancel["claims"], serde_json::json!([]), "{cancel}");
    assert_eq!(f.ps(&[])[0]["claims"], serde_json::json!([]));
    assert!(has_event(&f, "claim_cancelled", |e| e["service"] == target.as_str()));
    // Cancelling again changes nothing.
    let again = next.call(&f, &f.wt_b, "b1", &["claim", &target, "--cancel"]);
    assert_eq!(again["ok"], true, "{again}");

    // With an empty queue, the expired scope is dropped, not handed over.
    holder.stop();
    let gc = json(&f.taba(&f.outside, &["gc", "--json"]));
    assert_eq!(gc["handed_over"], serde_json::json!([]), "{gc}");
    assert_eq!(gc["dropped"][0]["services"][0], target.as_str(), "{gc}");
    assert!(f.ps(&[]).is_empty());
}

// ------------------------------------------------------------ test 12

#[test]
fn disown_hands_over_to_the_claimant_or_refuses_without_one() {
    let f = Fixture::new("disown");
    let holder = Agent::start();
    let claimant = Agent::start();
    let web = holder.call(&f, &f.wt_a, "a1", &run_sleeper("web", &[]));
    let api = holder.call(&f, &f.wt_a, "a1", &run_sleeper("api", &[]));
    let (web_id, api_id) = (
        web["service"].as_str().unwrap().to_string(),
        api["service"].as_str().unwrap().to_string(),
    );
    let scope_b = f.scope_of(&f.wt_b);

    // Nobody queued: E_NO_CLAIMANT, and nothing changes.
    let out = holder
        .cmd(&f, &f.wt_a, "a1", &["disown", &web_id])
        .output()
        .unwrap();
    assert_eq!(exit(&out), 3, "{}", text(&out));
    assert_eq!(
        json(&out)["error"]["code"],
        "E_NO_CLAIMANT",
        "{}",
        text(&out)
    );
    let row = f.ps(&[]);
    let row = service(&row, &web_id).expect("still owned by wt-a");
    assert_eq!(row["instance"], web["instance"]);
    assert_eq!(row["status"], "running");

    // With a claimant queued, disown moves the live instance to it.
    claimant.call(&f, &f.wt_b, "b1", &["claim", &web_id]);
    let moved = holder.call(&f, &f.wt_a, "a1", &["disown", &web_id]);
    assert_eq!(moved["ok"], true, "{moved}");
    assert_eq!(moved["how"], "disown", "{moved}");
    let new_id = format!("{scope_b}/web");
    assert_eq!(moved["to"], new_id.as_str(), "{moved}");
    let ps = f.ps(&[]);
    assert!(service(&ps, &web_id).is_none());
    let row = service(&ps, &new_id).expect("moved");
    assert_eq!(row["instance"], web["instance"], "same instance");
    assert_eq!(row["pid"], web["pid"], "same process");
    assert_eq!(row["status"], "running");
    assert!(has_event(&f, "service_moved", |e| e["how"] == "disown"
        && e["to"] == new_id.as_str()));
    let who = claimant.call(&f, &f.wt_b, "b1", &["whoami"]);
    assert!(codes(&who).contains(&"W_HANDED_OVER".into()), "{who}");

    // A plain stop of a claimed, unborrowed service succeeds and discards
    // the claims.
    claimant.call(&f, &f.wt_b, "b1", &["claim", &api_id]);
    let stop = holder.call(&f, &f.wt_a, "a1", &["stop", &api_id]);
    assert_eq!(stop["ok"], true, "{stop}");
    assert!(has_event(&f, "claims_discarded", |e| e["service"] == api_id.as_str()));
}

// ------------------------------------------------------------ test 10

#[test]
fn successor_in_the_same_worktree_beats_the_claim_queue() {
    let f = Fixture::new("successor");
    // Default claim_grace (2 min); gc_grace long enough not to drop anything.
    configure(&f, "gc_grace = \"1h\"\n");
    let mut a1 = Agent::start();
    let a2 = Agent::start();
    let b1 = Agent::start();
    let web = a1.call(&f, &f.wt_a, "a1", &run_sleeper("web", &[]));
    let web_id = web["service"].as_str().unwrap().to_string();
    let claim = b1.call(&f, &f.wt_b, "b1", &["claim", &web_id]);
    assert_eq!(claim["position"], 1, "{claim}");
    a1.stop();

    // Another scope's call within claim_grace hands nothing over.
    let who = b1.call(&f, &f.wt_b, "b1", &["whoami"]);
    assert_eq!(who["ok"], true, "{who}");
    let gc = json(&f.taba(&f.outside, &["gc", "--json"]));
    assert_eq!(gc["handed_over"], serde_json::json!([]), "{gc}");
    let row = f.ps(&[]);
    let row = service(&row, &web_id).expect("still in wt-a");
    assert_eq!(row["lease"]["state"], "expired", "{row}");

    // The successor in wt-a takes the scope over with its service.
    let took = a2.call(&f, &f.wt_a, "a2", &["whoami"]);
    assert_eq!(took["you_hold_scope"], true, "{took}");
    assert!(codes(&took).contains(&"W_TOOK_OVER".into()), "{took}");
    let ps = f.ps(&[]);
    let row = service(&ps, &web_id).expect("kept its id");
    assert_eq!(row["instance"], web["instance"]);
    assert_eq!(row["lease"]["state"], "live", "{row}");
    let claims = row["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1, "the claim stays queued: {row}");
    assert_eq!(claims[0]["scope"], f.scope_of(&f.wt_b).as_str());
}

// ------------------------------------------------------------ W_BRANCH_CHANGED

#[test]
fn branch_switch_in_a_held_worktree_warns_once() {
    let f = Fixture::new("branch");
    let agent = Agent::start();
    let web = agent.call(&f, &f.wt_a, "a1", &run_sleeper("web", &[]));
    assert_eq!(web["ok"], true, "{web}");
    let first = agent.call(&f, &f.wt_a, "a1", &["whoami"]);
    assert_eq!(first["branch"], "a", "{first}");
    assert!(
        !codes(&first).contains(&"W_BRANCH_CHANGED".into()),
        "{first}"
    );

    git(&f.wt_a, &["checkout", "-q", "-b", "a-next"]);
    let changed = agent.call(&f, &f.wt_a, "a1", &["whoami"]);
    assert_eq!(changed["branch"], "a-next", "{changed}");
    assert_eq!(
        changed["scope"], web["scope"],
        "the scope follows the folder"
    );
    let w = changed["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "W_BRANCH_CHANGED")
        .unwrap_or_else(|| panic!("W_BRANCH_CHANGED: {changed}"));
    let msg = w["message"].as_str().unwrap();
    assert!(msg.contains("a-next"), "{msg}");

    let after = agent.call(&f, &f.wt_a, "a1", &["whoami"]);
    assert!(
        !codes(&after).contains(&"W_BRANCH_CHANGED".into()),
        "{after}"
    );
    assert_eq!(f.ps(&[])[0]["status"], "running");
}
