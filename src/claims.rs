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
