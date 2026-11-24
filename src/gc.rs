//! Explicit and bounded lazy cleanup. Never wait while holding the state lock.
use crate::{
    claims,
    config::Config,
    error::{Output, Result},
    lease::LeaseState,
    proc,
    state::{OpLock, Store},
    util::{now_secs, parse_rfc3339, rfc3339},
};
use serde_json::json;
use std::time::Duration;

pub fn collect(
    store: &Store,
    cfg: &Config,
    dry: bool,
    limit: usize,
    protected: Option<&str>,
) -> Result<Output> {
    let now = now_secs();
    if !dry && limit == usize::MAX {
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
                continue;
            };
            if !crate::borrows::live(store, &svc)?.is_empty() {
                kept.push(json!({"service":svc.id(),"reason":"borrowed"}));
                continue;
            }
            budget -= 1;
            if dry {
                services.push(svc.id());
                continue;
            }
            store.put_op(&svc.scope, &svc.name, &OpLock::mine("stop", "gc"))?;
            drop(lock);
            let stopped = if svc.is_running() {
                proc::stop_group(svc.pid, svc.pgid, svc.start_time, Duration::from_secs(1))
                    .map(|_| ())
            } else {
                Ok(())
            };
            let _lock = store.lock()?;
            store.release_op(&svc.scope, &svc.name)?;
            if stopped.is_ok() && !svc.is_running() {
                store.remove_record_keep_log(&svc.scope, &svc.name)?;
                store.append_event(&json!({"at":rfc3339(now_secs()),"event":"gc_drop","service":svc.id(),"reason":reason}))?;
                services.push(svc.id());
            } else {
                errors.push(json!({"service":svc.id(),"message":stopped.err().map_or_else(|| "service still running".into(), |e| e.to_string())}));
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
