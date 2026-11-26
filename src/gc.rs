//! Explicit and bounded lazy cleanup. Never wait while holding the state lock.
use crate::{
    claims,
    config::{Config, format_duration},
    error::{Output, Result},
    lease::LeaseState,
    proc,
    state::{OpLock, Store},
    util::{parse_rfc3339, rfc3339},
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
    collect_with(&proc::System, store, cfg, dry, limit, protected)
}

/// `collect` with the clock and process operations of `ops`.
fn collect_with(
    ops: &impl proc::Ops,
    store: &Store,
    cfg: &Config,
    dry: bool,
    limit: usize,
    protected: Option<&str>,
) -> Result<Output> {
    let now = ops.now();
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
            if !explicit && elapsed.is_some_and(|e| e < cfg.stop_timeout) && ops.is_running(&svc) {
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
            let stopped = if !ops.is_running(&svc) {
                Ok(Stop::NotRunning)
            } else if explicit || elapsed.is_some() {
                // `taba gc`, or the grace period of an earlier request has
                // passed: wait out what is left of it, then force.
                let left = grace.saturating_sub(Duration::from_secs(elapsed.unwrap_or(0)));
                ops.stop_group(&svc, left).map(Stop::Done)
            } else {
                // Lazy cleanup inside another command: ask, wait briefly, and
                // leave a slow service to a later cleanup instead of forcing it.
                ops.request_stop(&svc, grace.min(LAZY_WAIT)).map(|gone| {
                    if gone {
                        Stop::Done(proc::StopResult::Terminated)
                    } else {
                        Stop::Requested
                    }
                })
            };
            if let Ok(Stop::Done(_)) = stopped {
                // The leader's handle can outlive its job briefly on Windows.
                proc::wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
                    !ops.is_running(&svc)
                });
            }
            let _lock = store.lock()?;
            store.release_op(&svc.scope, &svc.name)?;
            match stopped {
                Ok(Stop::Requested) => {
                    if let Some(mut current) = store.get(&svc.scope, &svc.name)?
                        && current.instance() == svc.instance()
                    {
                        current.stop_requested_at = Some(ops.now());
                        store.put(&current)?;
                    }
                    errors.push(json!({"service":svc.id(),"message":format!("still running after the graceful stop request; forced after {} unless it exits", format_duration(cfg.stop_timeout))}));
                }
                Ok(result) if !ops.is_running(&svc) => {
                    let signal = match result {
                        Stop::Done(proc::StopResult::Terminated) => Some("TERM"),
                        Stop::Done(proc::StopResult::Killed) => Some("KILL"),
                        _ => None,
                    };
                    store.remove_record_keep_log(&svc.scope, &svc.name)?;
                    store.append_event(&json!({"at":rfc3339(ops.now()),"event":"gc_drop","service":svc.id(),"reason":reason,"signal":signal}))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{Holder, LeaseRec, ScopeRecord};
    use crate::proc::fake::FakeOps;
    use crate::state::Service;
    use serde_json::Value;
    use std::path::PathBuf;

    const T0: u64 = 1_800_000_000;
    const SCOPE: &str = "wt-a-1234";

    /// A throwaway store; removed on drop.
    struct Tmp(Store, PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    fn store(test: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("taba-gc-unit-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Tmp(Store::at(dir.clone()), dir)
    }

    fn cfg() -> Config {
        Config {
            present: true,
            lease_ttl: None,
            claim_grace: 120,
            gc_grace: 600,
            log_retention: 86_400,
            stop_timeout: 5,
            unknown: Vec::new(),
        }
    }

    /// No grace at all: due as soon as the lease ended.
    fn no_grace() -> Config {
        Config {
            gc_grace: 0,
            claim_grace: 0,
            ..cfg()
        }
    }

    /// A scope record held by `a1` without a pid; its lease ended at
    /// `ended` when given, else it is live with no TTL.
    fn scope(store: &Store, ended: Option<u64>) {
        store
            .put_scope(&ScopeRecord {
                scope: SCOPE.into(),
                source: Some("git".into()),
                worktree: None,
                branch: None,
                holder: Holder {
                    agent: "a1".into(),
                    pid: None,
                    pid_start_time: None,
                },
                lease: LeaseRec {
                    ttl_secs: None,
                    ttl_source: "default".into(),
                    renewed_at: rfc3339(T0),
                    expires_at: None,
                    ended_at: ended.map(rfc3339),
                    end_reason: ended.map(|_| "lease_ended".into()),
                },
                keep: false,
                keep_reason: None,
                created_at: rfc3339(T0),
                moves: Vec::new(),
                notices: Vec::new(),
            })
            .unwrap();
    }

    /// A service record. Its pid and start time match no real process; the
    /// fake decides whether it is running.
    fn service(store: &Store, name: &str) -> Service {
        let log_path = store.log_path(SCOPE, name);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, "log\n").unwrap();
        let svc = Service {
            scope: SCOPE.into(),
            name: name.into(),
            command: vec!["dummy".into()],
            cwd: PathBuf::from("."),
            port: None,
            pid: i32::MAX,
            pgid: i32::MAX,
            start_time: 1,
            log_path,
            command_template: None,
            port_request: None,
            instance: None,
            started_by: Some("a1".into()),
            ready: None,
            service_ttl_secs: None,
            service_expires_at: None,
            moves: Vec::new(),
            stop_requested_at: None,
            input: None,
        };
        store.put(&svc).unwrap();
        svc
    }

    fn get(store: &Store, name: &str) -> Option<Service> {
        store.get(SCOPE, name).unwrap()
    }

    fn drops(store: &Store) -> Vec<Value> {
        std::fs::read_to_string(store.root().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["event"] == "gc_drop")
            .collect()
    }

    fn explicit(ops: &FakeOps, store: &Store, cfg: &Config) -> Output {
        collect_with(ops, store, cfg, false, usize::MAX, None).unwrap()
    }

    fn lazy(ops: &FakeOps, store: &Store, cfg: &Config) -> Output {
        collect_with(ops, store, cfg, false, 5, None).unwrap()
    }

    fn first_error(out: &Output) -> String {
        out.body["dropped"][0]["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn an_ended_lease_is_dropped_only_once_gc_grace_has_passed() {
        let t = store("grace");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 599);
        let out = explicit(&ops, &t.0, &cfg());
        assert_eq!(out.body["dropped"], serde_json::json!([]));
        assert!(ops.calls().is_empty());
        assert!(get(&t.0, "web").is_some());

        ops.now.set(T0 + 600);
        let out = explicit(&ops, &t.0, &cfg());
        // `taba gc` waits out the whole stop_timeout before forcing.
        assert_eq!(ops.calls(), ["stop web 5000"]);
        assert_eq!(out.body["dropped"][0]["services"][0], "wt-a-1234/web");
        assert_eq!(out.body["dropped"][0]["reason"], "lease_ended");
        assert!(get(&t.0, "web").is_none());
        assert!(t.0.live_op(SCOPE, "web").unwrap().is_none());
        let d = drops(&t.0);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["signal"], "TERM");
        assert_eq!(d[0]["at"], rfc3339(T0 + 600));
        // The emptied, expired scope is removed too.
        assert!(t.0.get_scope(SCOPE).unwrap().is_none());
    }

    #[test]
    fn claim_grace_longer_than_gc_grace_delays_the_drop() {
        let t = store("claimgrace");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let c = Config {
            gc_grace: 0,
            claim_grace: 120,
            ..cfg()
        };
        let ops = FakeOps::at(T0 + 119);
        explicit(&ops, &t.0, &c);
        assert!(ops.calls().is_empty());
        ops.now.set(T0 + 120);
        explicit(&ops, &t.0, &c);
        assert_eq!(ops.calls(), ["stop web 5000"]);
    }

    #[test]
    fn a_dry_run_lists_without_signalling_or_writing() {
        let t = store("dry");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 3_600);
        let out = collect_with(&ops, &t.0, &cfg(), true, usize::MAX, None).unwrap();
        assert_eq!(out.body["dropped"][0]["services"][0], "wt-a-1234/web");
        assert!(ops.calls().is_empty());
        assert!(get(&t.0, "web").is_some());
        assert!(drops(&t.0).is_empty());
    }

    #[test]
    fn lazy_cleanup_asks_then_forces_only_after_stop_timeout() {
        let t = store("lazy");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 10);
        // The first lazy pass only asks, waiting at most one second.
        let out = lazy(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["request web 1000"]);
        let msg = first_error(&out);
        assert!(msg.contains("forced after 5s"), "{msg}");
        assert_eq!(get(&t.0, "web").unwrap().stop_requested_at, Some(T0 + 10));
        assert!(t.0.live_op(SCOPE, "web").unwrap().is_none());

        // Within stop_timeout of the request, later lazy passes leave it.
        ops.now.set(T0 + 14);
        lazy(&ops, &t.0, &no_grace());
        assert!(ops.calls().is_empty());
        assert!(get(&t.0, "web").is_some());

        // Once stop_timeout has passed, it is forced with no further wait.
        ops.now.set(T0 + 15);
        ops.kill.set(true);
        lazy(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["stop web 0"]);
        assert!(get(&t.0, "web").is_none());
        assert_eq!(drops(&t.0)[0]["signal"], "KILL");
    }

    #[test]
    fn explicit_gc_waits_only_the_rest_of_an_earlier_request() {
        let t = store("rest");
        scope(&t.0, Some(T0));
        let mut svc = service(&t.0, "web");
        svc.stop_requested_at = Some(T0 + 1_000);
        t.0.put(&svc).unwrap();
        let ops = FakeOps::at(T0 + 1_002);
        explicit(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["stop web 3000"]);
        assert!(get(&t.0, "web").is_none());
    }

    #[test]
    fn a_stale_stop_request_starts_a_fresh_grace_period() {
        let t = store("stale");
        scope(&t.0, Some(T0));
        let mut svc = service(&t.0, "web");
        let now = T0 + 10_000;
        // Older than stop_timeout + STALE_REQUEST: ignored.
        svc.stop_requested_at = Some(now - (5 + STALE_REQUEST + 1));
        t.0.put(&svc).unwrap();
        let ops = FakeOps::at(now);
        lazy(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["request web 1000"]);
        assert_eq!(get(&t.0, "web").unwrap().stop_requested_at, Some(now));
    }

    #[test]
    fn a_service_that_exits_on_request_is_dropped_with_term() {
        let t = store("quick");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 10);
        ops.exits_on_request.set(true);
        lazy(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["request web 1000"]);
        assert!(get(&t.0, "web").is_none());
        assert_eq!(drops(&t.0)[0]["signal"], "TERM");
    }

    #[test]
    fn an_exited_service_is_dropped_without_a_signal() {
        let t = store("exited");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 600);
        ops.running.set(false);
        explicit(&ops, &t.0, &cfg());
        assert!(ops.calls().is_empty());
        assert!(get(&t.0, "web").is_none());
        assert_eq!(drops(&t.0)[0]["signal"], Value::Null);
        // The log is kept for log_retention.
        assert!(t.0.log_path(SCOPE, "web").exists());
    }

    #[test]
    fn service_ttl_expires_by_the_injected_clock_under_a_live_lease() {
        let t = store("svcttl");
        scope(&t.0, None);
        let mut svc = service(&t.0, "web");
        svc.service_ttl_secs = Some(60);
        svc.service_expires_at = Some(rfc3339(T0 + 60));
        t.0.put(&svc).unwrap();
        let ops = FakeOps::at(T0 + 59);
        explicit(&ops, &t.0, &cfg());
        assert!(ops.calls().is_empty());
        ops.now.set(T0 + 60);
        let out = explicit(&ops, &t.0, &cfg());
        assert_eq!(ops.calls(), ["stop web 5000"]);
        assert_eq!(out.body["dropped"][0]["reason"], "service_ttl_expired");
        assert_eq!(drops(&t.0)[0]["reason"], "service_ttl_expired");
        // The live scope stays.
        assert!(t.0.get_scope(SCOPE).unwrap().is_some());
    }

    #[test]
    fn a_renewed_lease_forgets_an_earlier_stop_request() {
        let t = store("renewed");
        scope(&t.0, None);
        let mut svc = service(&t.0, "web");
        svc.stop_requested_at = Some(T0);
        t.0.put(&svc).unwrap();
        let ops = FakeOps::at(T0 + 3);
        lazy(&ops, &t.0, &cfg());
        assert!(ops.calls().is_empty());
        assert_eq!(get(&t.0, "web").unwrap().stop_requested_at, None);
    }

    #[test]
    fn a_kept_scope_is_never_signalled() {
        let t = store("kept");
        scope(&t.0, Some(T0));
        let mut rec = t.0.get_scope(SCOPE).unwrap().unwrap();
        rec.keep = true;
        rec.keep_reason = Some("demo".into());
        t.0.put_scope(&rec).unwrap();
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 86_400);
        let out = explicit(&ops, &t.0, &cfg());
        assert!(ops.calls().is_empty());
        assert_eq!(out.body["kept"][0]["reason"], "demo");
        assert!(get(&t.0, "web").is_some());
    }

    #[test]
    fn a_failed_signal_keeps_the_record_and_releases_the_operation() {
        let t = store("sigfail");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 600);
        ops.fail.set(Some(std::io::ErrorKind::PermissionDenied));
        let out = explicit(&ops, &t.0, &cfg());
        assert_eq!(ops.calls(), ["stop web 5000"]);
        assert_eq!(
            out.body["dropped"][0]["errors"][0]["service"],
            "wt-a-1234/web"
        );
        assert!(
            first_error(&out).contains("fake signal failure"),
            "{:?}",
            out.body
        );
        assert_eq!(out.body["dropped"][0]["services"], serde_json::json!([]));
        assert!(get(&t.0, "web").is_some());
        assert!(t.0.live_op(SCOPE, "web").unwrap().is_none());
        assert!(drops(&t.0).is_empty());
        assert!(t.0.get_scope(SCOPE).unwrap().is_some());

        // The lazy pass reports a failed request and records no request.
        let out = lazy(&ops, &t.0, &no_grace());
        assert_eq!(ops.calls(), ["request web 1000"]);
        assert!(
            first_error(&out).contains("fake signal failure"),
            "{:?}",
            out.body
        );
        assert_eq!(get(&t.0, "web").unwrap().stop_requested_at, None);
        assert!(t.0.live_op(SCOPE, "web").unwrap().is_none());

        // Once signalling works again, the next gc drops it.
        ops.fail.set(None);
        explicit(&ops, &t.0, &cfg());
        assert!(get(&t.0, "web").is_none());
    }

    #[test]
    fn the_lazy_budget_bounds_signalled_services() {
        let t = store("budget");
        scope(&t.0, Some(T0));
        for n in ["a", "b", "c"] {
            service(&t.0, n);
        }
        let ops = FakeOps::at(T0 + 600);
        // The first service exits on request; the fake then reports the
        // others as exited too, so they need no signal but still count.
        ops.exits_on_request.set(true);
        collect_with(&ops, &t.0, &cfg(), false, 2, None).unwrap();
        assert_eq!(ops.calls().len(), 1);
        assert_eq!(drops(&t.0).len(), 2);
        assert_eq!(t.0.list(Some(SCOPE)).unwrap().len(), 1, "the third waits");
    }

    #[test]
    fn the_protected_scope_is_skipped() {
        let t = store("protected");
        scope(&t.0, Some(T0));
        service(&t.0, "web");
        let ops = FakeOps::at(T0 + 600);
        collect_with(&ops, &t.0, &cfg(), false, usize::MAX, Some(SCOPE)).unwrap();
        assert!(ops.calls().is_empty());
        assert!(get(&t.0, "web").is_some());
    }
}
