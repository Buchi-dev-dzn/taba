//! Recorded hook payloads; the proposed shell commands are never executed.
mod common;
use common::*;
use serde_json::{Value, json};
use std::{io::Write, path::Path, process::Stdio};

fn hook(
    f: &Fixture,
    cwd: &Path,
    harness: &str,
    event: &str,
    command: &str,
    env_file: Option<&Path>,
) -> Value {
    let mut c = f.cmd(cwd, &["hook", harness, event]);
    if let Some(path) = env_file {
        c.env("CLAUDE_ENV_FILE", path);
    }
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&json!({"session_id":"worker-a","cwd":cwd,"tool_name":"Bash","tool_input":{"command":command}})).unwrap()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    if out.stdout.is_empty() {
        Value::Null
    } else {
        json(&out)
    }
}

fn decision(out: &Value) -> &Value {
    &out["hookSpecificOutput"]["permissionDecision"]
}

#[test]
fn two_worktrees_keep_their_managed_servers_and_unmanaged_is_denied() {
    let f = Fixture::new("guardrail");
    let a = json(&f.taba(&f.wt_a, &run_auto("web")));
    let b = json(&f.taba(&f.wt_b, &run_auto("api")));
    let run = |cwd: &Path, harness: &str, command: &str| {
        hook(&f, cwd, harness, "pre-tool-use", command, None)
    };
    let cmd = format!("lsof -ti:{} | xargs kill", port_of(&b));
    for harness in ["claude-code", "codex"] {
        let denial = run(&f.wt_a, harness, &cmd);
        assert_eq!(decision(&denial), "deny", "{denial}");
        let reason = denial["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(reason.contains(b["scope"].as_str().unwrap()) && reason.contains("--port auto"));
    }
    assert_eq!(
        decision(&run(&f.wt_a, "claude-code", "pkill -f http.server")),
        "deny"
    );
    let pid = format!("kill {}", a["pid"]);
    // The default fixture holder is human; a hook identity must not borrow it.
    assert_eq!(decision(&run(&f.wt_a, "claude-code", &pid)), "deny");
    let forced = "taba stop web --force --reason 'review needed'";
    assert_eq!(decision(&run(&f.wt_a, "claude-code", forced)), "ask");
    assert_eq!(decision(&run(&f.wt_a, "codex", forced)), "deny");
    assert!(decision(&run(&f.wt_a, "claude-code", "kill $SERVER_PID")).is_null());
    assert!(f.events().iter().any(|e| e["event"] == "hook_unchecked"));
    let port = (21_000..30_000).find(|p| port_free(*p)).unwrap();
    let unmanaged = Unmanaged::start(port, &f.outside);
    let denial = run(
        &f.wt_a,
        "claude-code",
        &format!("kill {}", unmanaged.0.id()),
    );
    assert_eq!(decision(&denial), "deny");
    assert!(denial.to_string().contains("ask the human"));
    assert!(http_ok(port_of(&a)) && http_ok(port_of(&b)) && http_ok(port));
}

#[test]
fn session_hooks_set_identity_and_end_its_lease_without_signalling() {
    let f = Fixture::new("session");
    let file = f.root.join("claude.env");
    assert!(hook(&f, &f.wt_a, "claude-code", "session-start", "", Some(&file)).is_null());
    let exports = std::fs::read_to_string(file).unwrap();
    assert!(
        exports.contains("TABA_AGENT=claude-code:worker-a") && exports.contains("TABA_OUTPUT=json")
    );
    let web = json(
        &f.cmd(&f.wt_a, &run_auto("web"))
            .env("TABA_AGENT", "claude-code:worker-a")
            .output()
            .unwrap(),
    );
    assert_eq!(web["ok"], true, "{web}");
    let own = hook(
        &f,
        &f.wt_a,
        "claude-code",
        "pre-tool-use",
        &format!("kill {}", web["pid"]),
        None,
    );
    assert!(decision(&own).is_null(), "{own}");
    assert!(hook(&f, &f.wt_a, "claude-code", "session-end", "", None).is_null());
    assert_eq!(f.ps(&[])[0]["lease"]["state"], "expired");
    assert!(http_ok(port_of(&web)));
}
