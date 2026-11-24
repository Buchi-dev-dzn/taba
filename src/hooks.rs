//! Optional shell guardrail. It never runs a supplied command or signals a process.
use crate::{
    error::{Result, TabaError},
    proc, scope,
    state::Store,
    util,
};
use clap::ValueEnum;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Harness {
    ClaudeCode,
    Codex,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Event {
    PreToolUse,
    SessionStart,
    SessionEnd,
}

fn words(s: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    let mut args = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\\' && quote != Some('\'') {
            let next = iter.next()?;
            if next != '\n' {
                word.push(next);
                started = true;
            }
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && matches!(c, '$' | '`') {
                return None;
            } else {
                word.push(c);
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
            started = true;
            continue;
        }
        if matches!(c, '$' | '`' | '>' | '<' | '(' | ')' | '*' | '?' | '[' | ']') {
            return None;
        }
        if c == '#' && !started {
            while iter.peek().is_some_and(|&ch| ch != '\n') {
                iter.next();
            }
            continue;
        }
        if c.is_whitespace() || matches!(c, ';' | '|' | '&') {
            if started {
                args.push(std::mem::take(&mut word));
                started = false;
            }
            if matches!(c, '\n' | ';' | '|' | '&') && !args.is_empty() {
                commands.push(std::mem::take(&mut args));
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(word);
    }
    if !args.is_empty() {
        commands.push(args);
    }
    Some(commands)
}

fn program(s: &str) -> &str {
    s.rsplit(['/', '\\'])
        .next()
        .unwrap_or(s)
        .trim_end_matches(".exe")
}
fn resolve_numeric_tail(s: &str) -> Option<u16> {
    s.trim_end_matches("/tcp").rsplit(':').next()?.parse().ok()
}
fn audit(store: &Store, event: &str, caller: &scope::Caller) {
    if let Ok(_lock) = store.lock() {
        let _=store.append_event(&json!({"at":util::rfc3339_now(),"event":event,"agent":caller.agent,"scope":caller.scope}));
    }
}

fn inspect(store: &Store, caller: &scope::Caller, command: &str, is_claude: bool) -> Result<Value> {
    // Recognize the common lsof command substitution without evaluating it.
    let substitution = regex::Regex::new(
        r"^\s*kill\s+(?:-[A-Za-z0-9]+\s+)?\$\(lsof\s+-t\s+-i:([0-9]+)(?:\s+-nP)?\s*\)\s*$",
    )
    .expect("constant regex");
    let synthetic;
    let commands = if let Some(c) = substitution.captures(command) {
        synthetic = vec![
            vec!["lsof".into(), format!("-ti:{}", &c[1])],
            vec!["xargs".into(), "kill".into()],
        ];
        Some(synthetic)
    } else {
        words(command)
    };
    let Some(commands) = commands else {
        audit(store, "hook_unchecked", caller);
        return Ok(json!({}));
    };
    let mut targets = Vec::new();
    let mut ports = Vec::new();
    let mut ask = None;
    let mut unchecked = false;
    for (i, args) in commands.iter().enumerate() {
        let Some((name, args)) = args.split_first() else {
            continue;
        };
        match program(name) {
            "taba"
                if args
                    .iter()
                    .take_while(|a| a.as_str() != "--")
                    .any(|a| a == "--force") =>
            {
                let target = args
                    .iter()
                    .take_while(|a| a.as_str() != "--force")
                    .filter(|a| !a.starts_with('-'))
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
                let why = args
                    .windows(2)
                    .find(|p| p[0] == "--reason")
                    .map_or("not supplied", |p| p[1].as_str());
                ask = Some(format!(
                    "Confirm forced taba operation ({target}); reason: {}",
                    why.chars().take(200).collect::<String>()
                ));
            }
            "kill" => {
                let mut after_dash = false;
                for (i, a) in args.iter().enumerate() {
                    if a == "--" {
                        after_dash = true;
                        continue;
                    }
                    if !after_dash && i == 0 && a.starts_with('-') {
                        continue;
                    }
                    match a.parse::<i32>() {
                        Ok(p) if p > 0 => targets.push(p),
                        Ok(p) if p < -1 => targets.extend(
                            proc::all_pids()
                                .into_iter()
                                .filter(|id| proc::pgid(*id) == p.checked_neg()),
                        ),
                        _ => unchecked = true,
                    }
                }
            }
            "xargs" if args.first().is_some_and(|s| program(s) == "kill") && i > 0 => {
                let prev = &commands[i - 1];
                if prev.first().is_some_and(|s| program(s) == "lsof")
                    && prev.iter().any(|s| s.starts_with("-t"))
                {
                    ports.extend(
                        prev.iter()
                            .filter(|s| s.starts_with("-i:") || s.starts_with("-ti:"))
                            .filter_map(|s| resolve_numeric_tail(s)),
                    );
                } else {
                    unchecked = true;
                }
            }
            "fuser" if args.iter().any(|s| s == "-k") => {
                ports.extend(args.iter().filter_map(|s| resolve_numeric_tail(s)))
            }
            "npx" if args.first().is_some_and(|s| s == "kill-port") => {
                ports.extend(args.iter().skip(1).filter_map(|s| resolve_numeric_tail(s)))
            }
            "pkill" | "killall" => {
                let opts = args
                    .iter()
                    .take_while(|a| a.starts_with('-'))
                    .collect::<Vec<_>>();
                if opts.iter().any(|a| a.as_str() != "-f") {
                    unchecked = true;
                    continue;
                }
                let Some(pat) = args
                    .get(opts.len())
                    .filter(|_| args.len() == opts.len() + 1)
                else {
                    unchecked = true;
                    continue;
                };
                let full = opts.iter().any(|s| s.as_str() == "-f");
                let re = if program(name) == "pkill" {
                    match regex::Regex::new(pat) {
                        Ok(re) => Some(re),
                        Err(_) => {
                            unchecked = true;
                            continue;
                        }
                    }
                } else {
                    None
                };
                for id in proc::all_pids() {
                    if id == std::process::id() as i32 {
                        continue;
                    }
                    let Some(a) = proc::argv(id).filter(|a| !a.is_empty()) else {
                        continue;
                    };
                    let value = if full {
                        a.join(" ")
                    } else {
                        program(&a[0]).into()
                    };
                    if re
                        .as_ref()
                        .map_or_else(|| value == *pat, |r| r.is_match(&value))
                    {
                        targets.push(id);
                    }
                }
            }
            "eval" | "sh" | "bash" | "zsh" | "pwsh" | "powershell" | "cmd" => unchecked = true,
            _ => {}
        }
    }
    ports.sort_unstable();
    ports.dedup();
    for port in ports {
        targets.extend(proc::listeners(port).into_iter().map(|(p, _)| p));
    }
    targets.sort_unstable();
    targets.dedup();
    if !targets.is_empty() {
        let running: Vec<_> = store
            .list(None)?
            .into_iter()
            .filter(|s| s.is_running())
            .collect();
        let listeners = proc::all_listeners();
        for pid in targets {
            let deny = if let Some(svc) = running
                .iter()
                .find(|s| proc::in_group(pid, s.pid, s.pgid, s.start_time))
            {
                let same = caller.scope.as_deref() == Some(svc.scope.as_str());
                let owner = store.get_scope(&svc.scope)?;
                let holds = owner
                    .as_ref()
                    .is_none_or(|r| r.is_held_by(caller) && r.state(util::now_secs()).is_live());
                if !same || !holds || !crate::borrows::live(store, svc)?.is_empty() {
                    Some(format!(
                        "{} is owned by scope {} and is not yours to signal. Use taba stop {} to check ownership and borrows, start with --port auto, or ask the human.",
                        svc.id(),
                        svc.scope,
                        svc.id()
                    ))
                } else {
                    None
                }
            } else {
                let worktree = proc::cwd(pid).as_deref().and_then(scope::find_worktree);
                if listeners.iter().any(|s| s.pid == pid)
                    || worktree.is_some_and(|w| Some(w) != caller.worktree)
                {
                    Some(format!(
                        "Unmanaged process {pid} is a server or belongs to another worktree. Start your own service with --port auto, or ask the human."
                    ))
                } else {
                    None
                }
            };
            if let Some(reason) = deny {
                audit(store, "hook_denied", caller);
                return Ok(permission("deny", reason));
            }
        }
    }
    if unchecked {
        audit(store, "hook_unchecked", caller);
    }
    if let Some(reason) = ask {
        if is_claude {
            return Ok(permission("ask", reason));
        }
        audit(store, "hook_denied", caller);
        return Ok(permission(
            "deny",
            format!(
                "{reason}. Codex does not support hook confirmation; ask the human to run it separately."
            ),
        ));
    }
    // No decision: defer to the normal harness permissions, never elevate.
    Ok(json!({}))
}

fn permission(decision: &str, reason: String) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":decision,"permissionDecisionReason":reason}})
}

pub fn run(harness: Harness, event: Event) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    std::io::stdin().take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err(TabaError::usage("hook input exceeds 1 MiB"));
    }
    let input: Value =
        serde_json::from_slice(&bytes).map_err(|_| TabaError::usage("invalid hook JSON"))?;
    let claude = matches!(harness, Harness::ClaudeCode);
    let id = input["session_id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 100
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
        .ok_or_else(|| TabaError::usage("invalid hook session ID"))?;
    let agent = format!("{}:{id}", if claude { "claude-code" } else { "codex" });
    if matches!(event, Event::SessionStart) && claude {
        if let Some(path) = std::env::var_os("CLAUDE_ENV_FILE") {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(
                f,
                "export TABA_AGENT={}\nexport TABA_OUTPUT=json",
                util::posix_quote(&agent)
            )?;
        }
        return Ok(None);
    }
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let cwd = util::canonicalize(Path::new(&cwd))?;
    let caller = scope::resolve_at(cwd, None, Some(agent))?;
    match event {
        Event::PreToolUse => {
            let Some(command) = input["tool_input"]["command"].as_str() else {
                return Ok(None);
            };
            Ok(Some(inspect(&Store::open()?, &caller, command, claude)?))
        }
        Event::SessionEnd if claude => {
            crate::lease::cmd_lease_end(&caller, false)?;
            Ok(None)
        }
        _ => Err(TabaError::usage(
            "this lifecycle hook is only available for Claude Code",
        )),
    }
}
