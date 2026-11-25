//! Explicit and bounded lazy cleanup. Never wait while holding the state lock.
use crate::{
    claims,
    config::{Config, format_duration},
    error::{Output, Result},
    lease::LeaseState,
    proc,
    state::{OpLock, Store},
    util::{now_secs, parse_rfc3339, rfc3339},
};
use serde_json::json;
use std::time::Duration;

/// The longest lazy cleanup waits for one service after its graceful stop
/// request; a slower service is forced by a later cleanup, after `stop_timeout`.
const LAZY_WAIT: Duration = Duration::from_secs(1);
/// Seconds past `stop_timeout` after which an earlier stop request is
/// ignored and a fresh grace period starts.
const STALE_REQUEST: u64 = 600;

enum Stop {
    NotRunning,
    /// Asked to exit; still running.
    Requested,
    Done(proc::StopResult),
}

pub fn collect(
    store: &Store,
    cfg: &Config,
    dry: bool,
    limit: usize,
    protected: Option<&str>,
) -> Result<Output> {
    let now = now_secs();
    // `taba gc`, as opposed to the bounded lazy pass inside other commands.
    let explicit = limit == usize::MAX;
    if !dry && explicit {
        let _lock = store.lock()?;
        crate::borrows::sweep(store)?;
    }
    let mut moved = Vec::new();
    let mut dropped = Vec::new();
    let mut kept = Vec::new();
    let mut budget = limit;
    for mut scope in store.list_scopes()? {
        if protected == Some(scope.scope.as_str()) {
            continue;
        }
        if scope.keep {
            kept.push(json!({"scope":scope.scope,"reason":scope.keep_reason.as_deref().unwrap_or("keep")}));
            continue;
        }
        let (state, changed) = scope.observe(now);
        if changed && !dry {
            let _lock = store.lock()?;
            if let Some(mut current) = store.get_scope(&scope.scope)?
                && current.observe(now).1
            {
                store.put_scope(&current)?;
            }
        }
        let mut services = Vec::new();
        let mut errors = Vec::new();
        for item in store.list(Some(&scope.scope))? {
            if budget == 0 {
                break;
            }
            let lock = store.lock()?;
            let Some(current_scope) = store.get_scope(&scope.scope)? else {
                continue;
            };
            if current_scope.keep {
                continue;
            }
            let Some(svc) = store.get(&item.scope, &item.name)? else {
                continue;
            };
            if store.live_op(&svc.scope, &svc.name)?.is_some() {
                continue;
            }
            let current_state = current_scope.state(now);
            if let LeaseState::Expired { since, .. } = current_state
                && now.saturating_sub(since) >= cfg.claim_grace
                && let Some(result) = claims::offer(store, &svc, "claim", dry)?
            {
                moved.push(result);
                budget -= 1;
                continue;
            }
            let reason = if let LeaseState::Expired { why, since } = current_state
                && now.saturating_sub(since) >= cfg.gc_grace.max(cfg.claim_grace)
            {
                Some(why)
            } else if svc
                .service_expires_at
                .as_deref()
                .and_then(parse_rfc3339)
                .is_some_and(|t| now >= t)
            {
                Some("service_ttl_expired")
            } else {
                None
            };
            let Some(reason) = reason else {
                if svc.stop_requested_at.is_some() && !dry {
                    // No longer due (renewed, moved or kept): forget the request.
                    let mut current = svc;
                    current.stop_requested_at = None;
                    store.put(&current)?;
                }
                continue;
            };
            if !crate::borrows::live(store, &svc)?.is_empty() {
                kept.push(json!({"service":svc.id(),"reason":"borrowed"}));
                continue;
            }
            // A graceful stop request an earlier cleanup sent this instance.
            let elapsed = svc
                .stop_requested_at
                .map(|t| now.saturating_sub(t))
                .filter(|e| *e <= cfg.stop_timeout.saturating_add(STALE_REQUEST));
            if !explicit && elapsed.is_some_and(|e| e < cfg.stop_timeout) && svc.is_running() {
                // Still within its grace period; a later cleanup forces it.
                continue;
            }
            budget -= 1;
            if dry {
                services.push(svc.id());
                continue;
            }
            store.put_op(&svc.scope, &svc.name, &OpLock::mine("stop", "gc"))?;
            drop(lock);
            let grace = Duration::from_secs(cfg.stop_timeout);
            // Re-verify identity immediately before signalling.
            let stopped = if !svc.is_running() {
                Ok(Stop::NotRunning)
            } else if explicit || elapsed.is_some() {
                // `taba gc`, or the grace period of an earlier request has
                // passed: wait out what is left of it, then force.
                let left = grace.saturating_sub(Duration::from_secs(elapsed.unwrap_or(0)));
                proc::stop_group(svc.pid, svc.pgid, svc.start_time, left).map(Stop::Done)
            } else {
                // Lazy cleanup inside another command: ask, wait briefly, and
                // leave a slow service to a later cleanup instead of forcing it.
                proc::request_stop(svc.pid, svc.pgid, svc.start_time, grace.min(LAZY_WAIT)).map(
                    |gone| {
                        if gone {
                            Stop::Done(proc::StopResult::Terminated)
                        } else {
                            Stop::Requested
                        }
                    },
                )
            };
            if let Ok(Stop::Done(_)) = stopped {
                // The leader's handle can outlive its job briefly on Windows.
                proc::wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
                    !svc.is_running()
                });
            }
            let _lock = store.lock()?;
            store.release_op(&svc.scope, &svc.name)?;
            match stopped {
                Ok(Stop::Requested) => {
                    if let Some(mut current) = store.get(&svc.scope, &svc.name)?
                        && current.instance() == svc.instance()
                    {
                        current.stop_requested_at = Some(now_secs());
                        store.put(&current)?;
                    }
                    errors.push(json!({"service":svc.id(),"message":format!("still running after the graceful stop request; forced after {} unless it exits", format_duration(cfg.stop_timeout))}));
                }
                Ok(result) if !svc.is_running() => {
                    let signal = match result {
                        Stop::Done(proc::StopResult::Terminated) => Some("TERM"),
                        Stop::Done(proc::StopResult::Killed) => Some("KILL"),
                        _ => None,
                    };
                    store.remove_record_keep_log(&svc.scope, &svc.name)?;
                    store.append_event(&json!({"at":rfc3339(now_secs()),"event":"gc_drop","service":svc.id(),"reason":reason,"signal":signal}))?;
                    services.push(svc.id());
                }
                Ok(_) => errors.push(json!({"service":svc.id(),"message":"service still running"})),
                Err(e) => errors.push(json!({"service":svc.id(),"message":e.to_string()})),
            }
        }
        if !services.is_empty() || !errors.is_empty() || (dry && !state.is_live()) {
            dropped.push(json!({"scope":scope.scope,"reason":match state { LeaseState::Expired {why,..} => why, LeaseState::Live => "service_ttl_expired" },"services":services,"ended_borrows":[],"errors":errors}));
        }
        if !dry && !state.is_live() {
            let _lock = store.lock()?;
            if store
                .get_scope(&scope.scope)?
                .is_some_and(|r| !r.keep && !r.state(now).is_live())
                && store.list(Some(&scope.scope))?.is_empty()
            {
                store.remove_scope(&scope.scope)?;
            }
        }
    }
    if !dry && limit == usize::MAX {
        let _lock = store.lock()?;
        for log in store.orphan_logs()? {
            if log
                .symlink_metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= cfg.log_retention)
            {
                store.remove_log_file(&log)?;
            }
        }
    }
    let text = format!(
        "{}: {} scope(s) with drops, {} hand-over(s), {} kept",
        if dry { "gc dry run" } else { "gc" },
        dropped.len(),
        moved.len(),
        kept.len()
    );
    Ok(Output::new(
        json!({"dry_run":dry,"dropped":dropped,"handed_over":moved,"kept":kept}),
        text,
    ))
}
