//! FIFO queues for services whose owner may leave.
use crate::{
    config::Config,
    error::{Output, Result, TabaError},
    lease::{self, Access, Holder},
    scope::Caller,
    state::{Service, Store},
    util::{now_secs, rfc3339},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimQueue {
    pub service: String,
    pub queue: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub scope: String,
    #[serde(flatten)]
    pub holder: Holder,
    pub at: String,
}

impl Entry {
    fn alive(&self, store: &Store) -> Result<bool> {
        Ok(!self.holder.pid_gone()
            && store
                .get_scope(&self.scope)?
                .is_some_and(|r| r.holder == self.holder && r.state(now_secs()).is_live()))
    }
}

pub fn rows(store: &Store, queue: &ClaimQueue) -> Result<Vec<Value>> {
    queue.queue.iter().map(|e| Ok(json!({"scope": e.scope, "agent": e.holder.agent, "kind": "explicit", "at": e.at, "alive": e.alive(store)?}))).collect()
}

pub fn claim(
    store: &Store,
    cfg: &Config,
    caller: &Caller,
    target: &str,
    cancel: bool,
) -> Result<Output> {
    let scope = caller.require_scope()?;
    let (s, n) = crate::resolve_target(target, caller)?;
    if s == scope {
        return Err(TabaError::usage("a scope cannot claim its own service"));
    }
    let _lock = store.lock()?;
    let svc = store.get(&s, &n)?.ok_or_else(|| crate::not_found(&s, &n))?;
    if let Some(op) = store.live_op(&s, &n)? {
        return Err(crate::busy(&svc.id(), &op));
    }
    if !cancel && store.get(&scope, &n)?.is_some() {
        return Err(TabaError::new(
            "E_CONFLICT",
            format!("{scope}/{n} already exists"),
        ));
    }
    let touched = lease::touch(store, cfg, caller, &scope, Access::Create, None)?;
    let holder = touched.record.expect("created").holder;
    let mut q = store.get_claims(&s, &n)?;
    let existing = q
        .queue
        .iter()
        .position(|e| e.scope == scope && e.holder == holder);
    let at = rfc3339(now_secs());
    let changed = if cancel {
        existing.map(|i| q.queue.remove(i)).is_some()
    } else if existing.is_none() {
        q.queue.push(Entry {
            scope: scope.clone(),
            holder,
            at: at.clone(),
        });
        true
    } else {
        false
    };
    if changed {
        store.put_claims(&s, &n, &q)?;
        store.append_event(&json!({"at": at, "event": if cancel {"claim_cancelled"} else {"claimed"}, "service": svc.id(), "scope": scope, "agent": caller.agent}))?;
    }
    let mut out = Output::new(
        json!({"service":svc.id(), "position": (!cancel).then(|| existing.map_or(q.queue.len(), |i| i+1)), "claims":rows(store, &q)?}),
        format!(
            "{}: {}",
            svc.id(),
            if cancel {
                "claim cancelled"
            } else {
                "claim queued"
            }
        ),
    );
    out.warnings = touched.warnings;
    Ok(out)
}

/// Select and optionally transfer to the first eligible claimant. Caller holds the global lock.
pub fn offer(store: &Store, svc: &Service, how: &str, dry: bool) -> Result<Option<Value>> {
    if !svc.is_running() {
        return Ok(None);
    }
    let mut q = store.get_claims(&svc.scope, &svc.name)?;
    let mut skipped = Vec::new();
    let mut dead = Vec::new();
    let mut chosen = None;
    for (i, e) in q.queue.iter().enumerate() {
        if !e.alive(store)? {
            skipped.push(json!({"scope":e.scope, "why":"agent_gone"}));
            dead.push(i);
        } else if store.get(&e.scope, &svc.name)?.is_some()
            || store.live_op(&e.scope, &svc.name)?.is_some()
        {
            skipped.push(json!({"scope":e.scope, "why":"name_conflict"}));
        } else {
            chosen = Some((i, e.scope.clone()));
            break;
        }
    }
    let borrower = if chosen.is_none() {
        let mut candidate = None;
        for b in crate::borrows::live(store, svc)? {
            if b.scope != svc.scope
                && store.get(&b.scope, &svc.name)?.is_none()
                && store.live_op(&b.scope, &svc.name)?.is_none()
            {
                candidate = Some(b.scope);
                break;
            }
        }
        candidate
    } else {
        None
    };
    if how == "disown" && chosen.is_none() && borrower.is_none() {
        return Ok(None);
    }
    if !dry {
        for &i in dead.iter().rev() {
            let e = q.queue.remove(i);
            store.append_event(&json!({"at":rfc3339(now_secs()), "event":"claim_skipped", "service":svc.id(), "scope":e.scope, "why":"agent_gone"}))?;
        }
        if let Some((i, _)) = &chosen {
            q.queue.remove(i - dead.len());
        }
        store.put_claims(&svc.scope, &svc.name, &q)?;
    }
    let (target, how) = match (chosen, borrower) {
        (Some((_, target)), _) => (target, how),
        (None, Some(target)) => (target, if how == "disown" { how } else { "borrower" }),
        _ => return Ok(None),
    };
    let to = format!("{target}/{}", svc.name);
    if !dry {
        transfer(store, svc, &target, how)?;
    }
    Ok(Some(
        json!({"service":svc.id(), "to":to, "how":how, "skipped":skipped, "instance":svc.instance(), "url":svc.url()}),
    ))
}

pub fn transfer(store: &Store, svc: &Service, target: &str, how: &str) -> Result<()> {
    crate::scope::check_ident("scope", target)?;
    crate::scope::check_ident("scope", &svc.scope)?;
    crate::scope::check_ident("name", &svc.name)?;
    let mut moved = svc.clone();
    moved.scope = target.into();
    moved.log_path = store.log_path(target, &svc.name);
    let event = json!({"at":rfc3339(now_secs()), "event":"service_moved", "how":how, "from":svc.id(), "to":moved.id(), "instance":svc.instance()});
    moved.moves.push(event.clone());
    store.move_service(&svc.scope, &moved)?;
    crate::borrows::moved(store, svc, target)?;
    store.append_event(&event)?;
    lease::notify(
        store,
        target,
        "W_HANDED_OVER",
        format!("{} was handed over from {}", moved.id(), svc.scope),
    )?;
    Ok(())
}

pub fn discard(store: &Store, svc: &Service) -> Result<()> {
    let mut q = store.get_claims(&svc.scope, &svc.name)?;
    if !q.queue.is_empty() {
        q.queue.clear();
        store.put_claims(&svc.scope, &svc.name, &q)?;
        store.append_event(
            &json!({"at":rfc3339(now_secs()),"event":"claims_discarded", "service":svc.id()}),
        )?;
    }
    Ok(())
}

pub fn disown(
    store: &Store,
    cfg: &Config,
    caller: &Caller,
    target: Option<String>,
    bulk: bool,
) -> Result<Output> {
    let _lock = store.lock()?;
    let (targets, single) = crate::targets(store, target, bulk, caller, "disown")?;
    let mut results = Vec::new();
    let mut failure = None;
    for (s, n) in targets {
        let res = (|| {
            let svc = store.get(&s, &n)?.ok_or_else(|| crate::not_found(&s, &n))?;
            if caller.scope.as_deref() != Some(s.as_str()) {
                return Err(crate::not_owner(caller, &svc, "disown"));
            }
            lease::touch(store, cfg, caller, &s, Access::Mutate, None)?;
            if let Some(op) = store.live_op(&s, &n)? {
                return Err(crate::busy(&svc.id(), &op));
            }
            offer(store, &svc, "disown", false)?.ok_or_else(|| {
                TabaError::new("E_NO_CLAIMANT", format!("{}: nobody is waiting", svc.id()))
            })
        })();
        match res {
            Ok(v) => results.push(v),
            Err(e) if single => return Err(e),
            Err(e) => {
                results.push(
                    json!({"service":format!("{s}/{n}"),"result":"refused","error":e.to_json()}),
                );
                failure = Some(e);
            }
        }
    }
    let mut out = Output::new(
        if single {
            results.remove(0)
        } else {
            json!({"results":results})
        },
        "disown finished".into(),
    );
    out.failure = failure;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway state directory whose lifetime is limited to one test.
    struct Tmp(Store, PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    fn store(test: &str) -> Tmp {
        let dir =
            std::env::temp_dir().join(format!("taba-claims-unit-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Tmp(Store::at(dir.clone()), dir)
    }

    fn caller(scope: &str) -> Caller {
        Caller {
            scope: Some(scope.into()),
            source: None,
            worktree: None,
            branch: None,
            agent: "human".into(),
            agent_pid: None,
            cwd: PathBuf::from("."),
        }
    }

    fn service(store: &Store, scope: &str, name: &str) {
        // Use the test process as a live service without spawning or signaling anything.
        let pid = std::process::id() as i32;
        store
            .put(&Service {
                scope: scope.into(),
                name: name.into(),
                command: vec!["dummy".into()],
                cwd: PathBuf::from("."),
                port: None,
                pid,
                pgid: crate::proc::pgid(pid).unwrap_or(pid),
                start_time: crate::proc::info(pid).unwrap().start_time,
                log_path: store.log_path(scope, name),
                command_template: None,
                port_request: None,
                instance: None,
                started_by: None,
                ready: None,
                service_ttl_secs: None,
                service_expires_at: None,
                moves: Vec::new(),
                stop_requested_at: None,
                discovered_ports: Vec::new(),
            })
            .unwrap();
    }

    #[test]
    fn cancel_removes_only_the_callers_queued_claim() {
        let t = store("cancel");
        let cfg = Config::load(t.1.as_path()).unwrap();
        service(&t.0, "owner", "web");
        let next = caller("next");
        let later = caller("later");

        let queued = claim(&t.0, &cfg, &next, "owner/web", false).unwrap();
        assert_eq!(queued.body["position"], 1);
        let queued = claim(&t.0, &cfg, &later, "owner/web", false).unwrap();
        assert_eq!(queued.body["position"], 2);
        let remaining = json!([queued.body["claims"][1]]);
        assert_eq!(remaining[0]["scope"], "later");

        let cancelled = claim(&t.0, &cfg, &next, "owner/web", true).unwrap();
        assert!(cancelled.body["position"].is_null());
        assert_eq!(cancelled.body["claims"], remaining);
        let queue = || serde_json::to_value(t.0.get_claims("owner", "web").unwrap()).unwrap();
        let after_cancel = queue();
        assert_eq!(after_cancel["queue"].as_array().unwrap().len(), 1);
        assert_eq!(after_cancel["queue"][0]["scope"], "later");
        let event_log = t.1.join("events.jsonl");
        let events_after_cancel = std::fs::read(&event_log).unwrap();

        // Cancelling an absent claim is intentionally idempotent.
        let again = claim(&t.0, &cfg, &next, "owner/web", true).unwrap();
        assert!(again.body["position"].is_null());
        assert_eq!(again.body["claims"], remaining);
        assert_eq!(queue(), after_cancel);
        assert_eq!(std::fs::read(event_log).unwrap(), events_after_cancel);
    }

    #[test]
    fn disown_without_an_eligible_claimant_leaves_the_service_in_place() {
        let t = store("disown-none");
        let cfg = Config::load(t.1.as_path()).unwrap();
        service(&t.0, "owner", "web");
        let waiting = caller("waiting");
        let queued = claim(&t.0, &cfg, &waiting, "owner/web", false).unwrap();
        assert_eq!(queued.body["claims"][0]["alive"], true);
        // A live claimant is ineligible once it has a service with the same name.
        service(&t.0, "waiting", "web");
        assert!(t.0.get("owner", "web").unwrap().unwrap().is_running());
        let snapshot = || {
            (
                serde_json::to_value(t.0.get("owner", "web").unwrap()).unwrap(),
                serde_json::to_value(t.0.get("waiting", "web").unwrap()).unwrap(),
                serde_json::to_value(t.0.get_claims("owner", "web").unwrap()).unwrap(),
                std::fs::read(t.1.join("events.jsonl")).unwrap(),
            )
        };
        let before = snapshot();

        let err = match disown(
            &t.0,
            &cfg,
            &caller("owner"),
            Some("owner/web".into()),
            false,
        ) {
            Ok(_) => panic!("disown without an eligible claimant unexpectedly succeeded"),
            Err(err) => err,
        };
        assert_eq!(err.code, "E_NO_CLAIMANT");
        assert_eq!(err.exit_code(), 3);
        assert_eq!(snapshot(), before);
    }
}
