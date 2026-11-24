//! Scope records, holders and leases (design v0.2, section 3, "Leases" and
//! "Hand-off").
//!
//! One agent holds a scope at a time, under a lease that the holder renews
//! by using taba. A lease ends when the holder's `TABA_AGENT_PID` process is
//! gone, when the holder runs `taba lease end`, or when its TTL passes. The
//! next agent whose call resolves to the scope then takes it over with every
//! service (`auto_take`); while the lease is live, others get
//! `E_SCOPE_HELD`.

use crate::config::{Config, Ttl, TtlSource};
use crate::error::{Next, Result, TabaError, Warning};
use crate::scope::Caller;
use crate::state::Store;
use crate::util::{now_secs, parse_rfc3339, rfc3339};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Holder {
    pub agent: String,
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub pid_start_time: Option<u64>,
}

impl Holder {
    pub fn of(caller: &Caller) -> Holder {
        Holder {
            agent: caller.agent.clone(),
            pid: caller.agent_pid.map(|p| p.0),
            pid_start_time: caller.agent_pid.map(|p| p.1),
        }
    }

    /// Whether the holder's recorded agent process is gone. `false` when no
    /// pid was recorded.
    pub fn pid_gone(&self) -> bool {
        match (self.pid, self.pid_start_time) {
            (Some(pid), Some(start)) => !crate::proc::is_alive(pid, start),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRec {
    pub ttl_secs: Option<u64>,
    pub ttl_source: String,
    pub renewed_at: String,
    pub expires_at: Option<String>,
    /// When the lease was seen to end (`lease end`, or the first time taba
    /// noticed the holder's process gone). Graces count from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notice {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeRecord {
    pub scope: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub worktree: Option<PathBuf>,
    #[serde(default)]
    pub branch: Option<String>,
    pub holder: Holder,
    pub lease: LeaseRec,
    #[serde(default)]
    pub keep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_reason: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub moves: Vec<Value>,
    /// Warnings for the scope's next call (`W_HANDED_OVER`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

/// Why a lease is not live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Live,
    /// `why` is `holder_pid_gone`, `lease_ended` or `ttl_expired`; `since`
    /// is when it ended (seconds since the epoch).
    Expired {
        why: &'static str,
        since: u64,
    },
}

impl LeaseState {
    pub fn is_live(self) -> bool {
        self == LeaseState::Live
    }
}

fn reason_str(s: &str) -> &'static str {
    match s {
        "lease_ended" => "lease_ended",
        "ttl_expired" => "ttl_expired",
        _ => "holder_pid_gone",
    }
}

impl ScopeRecord {
    pub fn ttl(&self) -> Ttl {
        Ttl::from_secs(self.lease.ttl_secs)
    }

    /// The lease's state at `now`, without writing anything.
    pub fn state(&self, now: u64) -> LeaseState {
        if let (Some(at), Some(why)) = (&self.lease.ended_at, &self.lease.end_reason) {
            return LeaseState::Expired {
                why: reason_str(why),
                since: parse_rfc3339(at).unwrap_or(now),
            };
        }
        if let Some(exp) = self.lease.expires_at.as_deref().and_then(parse_rfc3339)
            && now >= exp
        {
            return LeaseState::Expired {
                why: "ttl_expired",
                since: exp,
            };
        }
        if self.holder.pid_gone() {
            return LeaseState::Expired {
                why: "holder_pid_gone",
                since: now,
            };
        }
        LeaseState::Live
    }

    /// Like `state`, and records when a lease was first seen ended, so that
    /// graces count from then. Returns whether the record changed.
    pub fn observe(&mut self, now: u64) -> (LeaseState, bool) {
        let st = self.state(now);
        if let LeaseState::Expired { why, since } = st
            && self.lease.ended_at.is_none()
        {
            self.lease.ended_at = Some(rfc3339(since));
            self.lease.end_reason = Some(why.to_string());
            return (st, true);
        }
        (st, false)
    }

    /// Starts a new lease period for the current holder.
    pub fn renew(&mut self, now: u64) {
        self.lease.renewed_at = rfc3339(now);
        self.lease.expires_at = self.lease.ttl_secs.map(|t| rfc3339(now.saturating_add(t)));
        self.lease.ended_at = None;
        self.lease.end_reason = None;
    }

    pub fn set_ttl(&mut self, ttl: Ttl, source: TtlSource) {
        self.lease.ttl_secs = ttl.secs();
        self.lease.ttl_source = source.as_str().to_string();
    }

    pub fn is_held_by(&self, caller: &Caller) -> bool {
        self.holder.agent == caller.agent
            && match (self.holder.pid, self.holder.pid_start_time) {
                (Some(pid), Some(start)) => caller.agent_pid == Some((pid, start)),
                _ => true,
            }
    }

    /// `lease` as `whoami` and `ps --json` show it.
    pub fn lease_json(&self, now: u64) -> Value {
        let state = match self.state(now) {
            LeaseState::Live => "live",
            LeaseState::Expired { .. } => "expired",
        };
        json!({
            "state": state,
            "ttl": self.ttl().display(),
            "ttl_source": self.lease.ttl_source,
            "renewed_at": self.lease.renewed_at,
            "expires_at": self.lease.expires_at,
        })
    }
}

/// How a command uses the caller's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `run`, `claim`, `renew`: create the scope record if there is none;
    /// renew, take over, or fail with `E_SCOPE_HELD`.
    Create,
    /// `stop`, `rm`, `keep`, `disown`: renew, take over, or `E_SCOPE_HELD`;
    /// no record is created.
    Mutate,
    /// `whoami`: renew or take over, but report a live foreign holder
    /// instead of failing.
    Observe,
    /// `ps`, `logs`: renew only if the caller holds the scope and the lease
    /// is more than 60 s old; never take over.
    Peek,
}

/// What `touch` found and did.
#[derive(Debug, Clone, Default)]
pub struct Touched {
    pub record: Option<ScopeRecord>,
    pub you_hold: bool,
    /// `{from, why}` when this call took the scope over.
    pub takeover: Option<Value>,
    /// Peek only: the caller could take the scope over.
    pub takeover_available: bool,
    pub warnings: Vec<Warning>,
}

/// The caller's agent identity: `TABA_AGENT_PID`'s process, when alive.
pub fn agent_pid_from_env() -> Option<(i32, u64)> {
    let pid: i32 = std::env::var("TABA_AGENT_PID").ok()?.trim().parse().ok()?;
    let info = crate::proc::info(pid)?;
    (!info.zombie).then_some((pid, info.start_time))
}

pub fn scope_held(scope: &str, rec: &ScopeRecord, now: u64) -> TabaError {
    let h = &rec.holder;
    let pid = h.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
    let until = rec
        .lease
        .expires_at
        .clone()
        .unwrap_or_else(|| "no expiry by time".into());
    TabaError::new(
        "E_SCOPE_HELD",
        format!(
            "scope {scope} is held by {}{pid} under a live lease (renewed {}, expires {until}); \
             it passes to you automatically when the holder's process ends",
            h.agent, rec.lease.renewed_at
        ),
    )
    .details(json!({
        "scope": scope,
        "holder": h.agent,
        "holder_pid": h.pid,
        "renewed_at": rec.lease.renewed_at,
        "expires_at": rec.lease.expires_at,
        "lease": rec.lease_json(now),
    }))
    .next(Next::just(
        "wait: the scope passes to you automatically once the holder's process ends",
    ))
    .next(Next::just("ask the coordinator or the human"))
}

fn new_record(
    caller: &Caller,
    scope: &str,
    cfg: &Config,
    ttl_flag: Option<Ttl>,
    now: u64,
) -> Result<ScopeRecord> {
    let (ttl, source) = cfg.resolve_ttl(ttl_flag, caller.is_human())?;
    let mut rec = ScopeRecord {
        scope: scope.to_string(),
        source: caller.source.map(|s| s.as_str().to_string()),
        worktree: caller.worktree.clone(),
        branch: caller.branch.clone(),
        holder: Holder::of(caller),
        lease: LeaseRec {
            ttl_secs: None,
            ttl_source: String::new(),
            renewed_at: String::new(),
            expires_at: None,
            ended_at: None,
            end_reason: None,
        },
        keep: false,
        keep_reason: None,
        created_at: rfc3339(now),
        moves: vec![
            json!({ "at": rfc3339(now), "from": null, "to": caller.agent, "how": "created" }),
        ],
        notices: Vec::new(),
    };
    rec.set_ttl(ttl, source);
    rec.renew(now);
    Ok(rec)
}

/// Renews, creates or takes over the caller's scope `scope` according to
/// `access`. Call under the global lock.
pub fn touch(
    store: &Store,
    cfg: &Config,
    caller: &Caller,
    scope: &str,
    access: Access,
    ttl_flag: Option<Ttl>,
) -> Result<Touched> {
    let now = now_secs();
    let mut out = Touched::default();
    let Some(mut rec) = store.get_scope(scope)? else {
        if access == Access::Create {
            let rec = new_record(caller, scope, cfg, ttl_flag, now)?;
            store.put_scope(&rec)?;
            store.append_event(&json!({
                "at": rfc3339(now), "event": "created", "scope": scope,
                "agent": caller.agent, "cwd": caller.cwd,
            }))?;
            out.record = Some(rec);
            out.you_hold = true;
        } else {
            // No record (a scope never used since v0.1, or dropped): the
            // caller acts as holder without creating one.
            out.you_hold = true;
        }
        return Ok(out);
    };
    let (state, mut changed) = rec.observe(now);
    if rec.is_held_by(caller) {
        out.you_hold = true;
        let stale = parse_rfc3339(&rec.lease.renewed_at).is_none_or(|t| now >= t + 60);
        if access != Access::Peek || (stale && state.is_live()) {
            if let Some(t) = ttl_flag {
                rec.set_ttl(t, TtlSource::Flag);
            }
            if caller.agent_pid.is_some() {
                rec.holder = Holder::of(caller);
            }
            rec.renew(now);
            changed = true;
        }
    } else if let LeaseState::Expired { why, .. } = state {
        if access == Access::Peek {
            out.takeover_available = true;
        } else {
            let from = rec.holder.agent.clone();
            let (ttl, source) = cfg.resolve_ttl(ttl_flag, caller.is_human())?;
            rec.holder = Holder::of(caller);
            rec.set_ttl(ttl, source);
            rec.renew(now);
            rec.moves.push(json!({
                "at": rfc3339(now), "from": from, "to": caller.agent, "how": "auto_take", "why": why,
            }));
            changed = true;
            out.you_hold = true;
            out.takeover = Some(json!({ "from": from, "why": why }));
            store.append_event(&json!({
                "at": rfc3339(now), "event": "moved", "how": "auto_take", "scope": scope,
                "from": from, "to": caller.agent, "why": why, "cwd": caller.cwd,
            }))?;
            let services: Vec<String> = store
                .list(Some(scope))?
                .iter()
                .map(|s| match s.url() {
                    Some(u) => format!("{} ({u})", s.id()),
                    None => s.id(),
                })
                .collect();
            out.warnings.push(Warning {
                code: "W_TOOK_OVER",
                message: format!(
                    "took over scope {scope} from {from} ({}); its services are yours now: {}",
                    why.replace('_', " "),
                    if services.is_empty() {
                        "none".to_string()
                    } else {
                        services.join(", ")
                    }
                ),
            });
        }
    } else if access != Access::Observe && access != Access::Peek {
        return Err(scope_held(scope, &rec, now));
    }
    if out.you_hold {
        if let (Some(b), Some(old)) = (&caller.branch, &rec.branch)
            && b != old
            && caller.worktree.is_some()
            && access != Access::Peek
        {
            out.warnings.push(Warning {
                code: "W_BRANCH_CHANGED",
                message: format!("the worktree's branch changed from {old} to {b}"),
            });
            rec.branch = Some(b.clone());
            changed = true;
        }
        if (access != Access::Peek || state.is_live()) && !rec.notices.is_empty() {
            for n in rec.notices.drain(..) {
                out.warnings.push(Warning {
                    code: notice_code(&n.code),
                    message: n.message,
                });
            }
            changed = true;
        }
    }
    if changed {
        store.put_scope(&rec)?;
    }
    out.record = Some(rec);
    Ok(out)
}

fn notice_code(c: &str) -> &'static str {
    match c {
        "W_HANDED_OVER" => "W_HANDED_OVER",
        "W_BORROW_BROKEN" => "W_BORROW_BROKEN",
        "W_OWNER_GONE" => "W_OWNER_GONE",
        "W_FORCED" => "W_FORCED",
        _ => "W_NOTICE",
    }
}

/// Queues a warning for the next call in `scope` (if it has a record).
pub fn notify(store: &Store, scope: &str, code: &str, message: String) -> Result<()> {
    if let Some(mut rec) = store.get_scope(scope)? {
        rec.notices.push(Notice {
            code: code.to_string(),
            message,
        });
        store.put_scope(&rec)?;
    }
    Ok(())
}

/// The lease as `whoami` shows it when there is no record yet: what a new
/// lease would get.
pub fn prospective_lease(cfg: &Config, caller: &Caller) -> Result<Value> {
    let (ttl, source) = cfg.resolve_ttl(None, caller.is_human())?;
    Ok(json!({
        "state": "none",
        "ttl": ttl.display(),
        "ttl_source": source.as_str(),
        "renewed_at": null,
        "expires_at": null,
    }))
}

// ---------------------------------------------------------------- commands

/// `taba renew [--scope S] [--ttl D]`.
pub fn cmd_renew(caller: &Caller, cfg: &Config, ttl: Option<Ttl>) -> Result<crate::error::Output> {
    let scope = caller.require_scope()?;
    let store = Store::open()?;
    let _lock = store.lock()?;
    let t = touch(&store, cfg, caller, &scope, Access::Create, ttl)?;
    let rec = t.record.expect("created");
    let now = now_secs();
    let lease = rec.lease_json(now);
    let text = format!(
        "renewed {scope}: ttl {} ({}), expires {}",
        rec.ttl().display(),
        rec.lease.ttl_source,
        rec.lease
            .expires_at
            .clone()
            .unwrap_or_else(|| "never".into())
    );
    let mut out = crate::error::Output::new(json!({ "scope": scope, "lease": lease }), text);
    out.warnings = t.warnings;
    Ok(out)
}

/// `taba lease end [--scope S | --agent A]`: marks the agent's leases as
/// ended, and nothing else (it must fit a SessionEnd hook's budget).
pub fn cmd_lease_end(caller: &Caller, scope_flag: bool) -> Result<crate::error::Output> {
    let store = Store::open()?;
    let _lock = store.lock()?;
    let now = now_secs();
    let mut ended = Vec::new();
    for mut rec in store.list_scopes()? {
        if !rec.is_held_by(caller) {
            continue;
        }
        if scope_flag && caller.scope.as_deref() != Some(rec.scope.as_str()) {
            continue;
        }
        if rec.lease.end_reason.as_deref() == Some("lease_ended") {
            continue;
        }
        rec.lease.ended_at = Some(rfc3339(now));
        rec.lease.end_reason = Some("lease_ended".into());
        store.put_scope(&rec)?;
        store.append_event(&json!({
            "at": rfc3339(now), "event": "lease_ended", "scope": rec.scope, "agent": caller.agent,
        }))?;
        ended.push(rec.scope.clone());
    }
    let text = if ended.is_empty() {
        format!("{} holds no live lease", caller.agent)
    } else {
        format!(
            "ended the lease of {} on: {}",
            caller.agent,
            ended.join(", ")
        )
    };
    Ok(crate::error::Output::new(
        json!({ "agent": caller.agent, "ended": ended }),
        text,
    ))
}

/// `taba keep [--scope S] [--off] [--reason R] [--force]`.
pub fn cmd_keep(
    caller: &Caller,
    cfg: &Config,
    off: bool,
    reason: Option<String>,
    force: bool,
) -> Result<crate::error::Output> {
    let scope = caller.require_scope()?;
    let reason = reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    if force && reason.is_none() {
        return Err(TabaError::new(
            "E_FORCE_NEEDS_REASON",
            "--force needs --reason TEXT; the reason is recorded in events.jsonl",
        ));
    }
    if !off && !caller.is_human() && reason.is_none() {
        return Err(TabaError::new(
            "E_FORCE_NEEDS_REASON",
            "an agent keeps its scope only with --reason TEXT, which is recorded and shown in ps",
        )
        .next(Next::run(
            "say why the human needs it",
            format!("taba keep --scope {scope} --reason \"<why>\""),
        )));
    }
    let store = Store::open()?;
    let _lock = store.lock()?;
    let now = now_secs();
    let mut warnings = Vec::new();
    let t = if force {
        None
    } else {
        Some(touch(&store, cfg, caller, &scope, Access::Create, None)?)
    };
    let mut rec = match store.get_scope(&scope)? {
        Some(r) => r,
        None => {
            let r = new_record(caller, &scope, cfg, None, now)?;
            store.put_scope(&r)?;
            r
        }
    };
    if let Some(t) = t {
        warnings = t.warnings;
    }
    rec.keep = !off;
    rec.keep_reason = if off { None } else { reason.clone() };
    store.put_scope(&rec)?;
    store.append_event(&json!({
        "at": rfc3339(now), "event": "keep_set", "scope": scope, "keep": !off,
        "reason": reason, "agent": caller.agent, "forced": force,
    }))?;
    let text = if off {
        format!("{scope}: keep removed; gc may drop it once its lease ends")
    } else {
        format!("{scope}: kept; gc will not hand over or drop it")
    };
    let mut out = crate::error::Output::new(
        json!({ "scope": scope, "keep": !off, "reason": rec.keep_reason }),
        text,
    );
    out.warnings = warnings;
    Ok(out)
}
