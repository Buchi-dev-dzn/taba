//! Empty or damaged state files in a disposable `$TABA_HOME`.
mod common;
use common::*;
use serde_json::Value;
use std::path::Path;

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn skipped(v: &Value, what: &str) -> bool {
    v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["code"] == "W_STATE_SKIPPED" && w["message"].as_str().unwrap().contains(what))
}

fn size(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().len()
}

#[test]
fn unreadable_service_record_is_reported_and_removable() {
    let f = Fixture::new("badrecord");
    let web = json(&f.taba(&f.wt_a, &run_auto("web")));
    let scope = web["scope"].as_str().unwrap().to_string();
    let record = f.home().join("services").join(&scope).join("ghost.json");
    std::fs::write(&record, b"").unwrap();
    let ghost = format!("{scope}/ghost");

    let ps = f.taba(&f.outside, &["ps", "--json"]);
    assert!(ps.status.success(), "{}", text(&ps));
    assert_eq!(stderr(&ps), "", "nothing on stderr in JSON mode");
    let ps = json(&ps);
    assert!(skipped(&ps, &ghost), "{ps}");
    assert_eq!(ps["services"].as_array().unwrap().len(), 1, "{ps}");
    assert_eq!(ps["services"][0]["service"], web["service"]);

    for args in [
        vec!["stop", ghost.as_str(), "--json"],
        vec!["rm", ghost.as_str(), "--json"],
    ] {
        let out = f.taba(&f.wt_a, &args);
        assert_eq!(stderr(&out), "");
        let v = json(&out);
        assert_eq!(v["error"]["code"], "E_STATE", "{v}");
        assert_eq!(v["error"]["details"]["reason"], "unreadable", "{v}");
        let next = v["error"]["next"][0]["command"].as_str().unwrap();
        assert!(next.starts_with(&format!("taba rm {ghost} --force")), "{v}");
    }
    // Nothing rewrote the damaged file.
    assert_eq!(size(&record), 0);

    let rm = json(&f.taba(
        &f.wt_a,
        &[
            "rm",
            &ghost,
            "--force",
            "--reason",
            "unreadable record",
            "--json",
        ],
    ));
    assert_eq!(rm["ok"], true, "{rm}");
    assert_eq!(rm["results"][0]["result"], "removed");
    assert_eq!(rm["results"][0]["unreadable"], true);
    assert!(!record.exists());
    assert!(
        f.events()
            .iter()
            .any(|e| e["event"] == "force_rm" && e["target"] == ghost.as_str())
    );
    let ps = json(&f.taba(&f.outside, &["ps", "--json"]));
    assert_eq!(ps["warnings"], serde_json::json!([]), "{ps}");
    assert_eq!(ps["services"][0]["service"], web["service"]);
    assert!(http_ok(port_of(&web)));
}

#[test]
fn unreadable_scope_record_is_kept_until_a_forced_rm() {
    let f = Fixture::new("badscope");
    // Initialise the state directory with a normal command first.
    assert!(f.taba(&f.outside, &["ps", "--json"]).status.success());
    let scopes = f.home().join("scopes");
    std::fs::create_dir_all(&scopes).unwrap();
    let record = scopes.join("zz.json");
    std::fs::write(&record, b"").unwrap();

    let who = f.taba(&f.outside, &["whoami", "--scope", "zz", "--json"]);
    assert_eq!(stderr(&who), "");
    let who = json(&who);
    assert_eq!(who["error"]["code"], "E_STATE", "{who}");
    let next = who["error"]["next"][0]["command"].as_str().unwrap();
    assert!(next.starts_with("taba rm --scope zz --force"), "{who}");

    // gc reports the file instead of skipping it silently, and leaves it.
    let gc = f.taba(&f.outside, &["gc", "--json"]);
    assert_eq!(stderr(&gc), "");
    let gc = json(&gc);
    assert_eq!(gc["ok"], true, "{gc}");
    assert!(skipped(&gc, "scope record zz"), "{gc}");
    assert_eq!(size(&record), 0);

    let rm = json(&f.taba(
        &f.outside,
        &[
            "rm",
            "--scope",
            "zz",
            "--force",
            "--reason",
            "unreadable",
            "--json",
        ],
    ));
    assert_eq!(rm["ok"], true, "{rm}");
    assert_eq!(rm["scope_record"], "removed_unreadable");
    assert!(!record.exists());
    let who = json(&f.taba(&f.outside, &["whoami", "--scope", "zz", "--json"]));
    assert_eq!(who["ok"], true, "{who}");
}

/// Issue #35: the state directory and everything taba creates in it are
/// owner-only on Unix, and an existing world-readable home is tightened.
#[cfg(unix)]
#[test]
fn state_directory_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("private");
    // A home left 0755 by an older taba.
    std::fs::create_dir_all(f.home()).unwrap();
    std::fs::set_permissions(f.home(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let web = json(&f.taba(&f.wt_a, &run_auto("web")));
    assert_eq!(web["ok"], true, "{web}");
    let scope = web["scope"].as_str().unwrap().to_string();
    // A forced stop writes an event to events.jsonl.
    let stop = f.taba(
        &f.wt_b,
        &[
            "stop",
            web["service"].as_str().unwrap(),
            "--force",
            "--reason",
            "test",
            "--json",
        ],
    );
    assert!(stop.status.success(), "{}", text(&stop));

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let home = f.home();
    for dir in [
        home.clone(),
        home.join("services"),
        home.join("services").join(&scope),
        home.join("logs"),
        home.join("logs").join(&scope),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    for file in [
        home.join("services").join(&scope).join("web.json"),
        home.join("logs").join(&scope).join("web.log"),
        home.join("events.jsonl"),
        home.join("version"),
        home.join("lock"),
    ] {
        assert_eq!(mode(&file), 0o600, "{}", file.display());
    }
}
