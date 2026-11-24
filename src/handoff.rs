//! Explicit transfers of a scope lease or an individual service.
use crate::{
    claims,
    config::Config,
    error::{Output, Result, TabaError},
    lease::{self, Holder, LeaseState},
    scope::{self, Caller},
    state::Store,
    util::{now_secs, rfc3339},
};
use serde_json::json;

pub struct Request {
    pub target: Option<String>,
    pub to: Option<String>,
    pub take: bool,
    pub force: bool,
    pub reason: Option<String>,
}

pub fn execute(store: &Store, cfg: &Config, caller: &Caller, args: Request) -> Result<Output> {
    let reason = crate::check_force(args.force, args.reason)?;
    let _lock = store.lock()?;
    let now = now_secs();
    let at = rfc3339(now);
    if let Some(id) = args.target {
        let to = args
            .to
            .ok_or_else(|| TabaError::usage("moving a service requires --to SCOPE"))?;
        scope::check_ident("scope", &to)?;
        let (from, name) = crate::resolve_target(&id, caller)?;
        if from == to {
            return Err(TabaError::usage("the receiving scope must be different"));
        }
        let svc = store
            .get(&from, &name)?
            .ok_or_else(|| crate::not_found(&from, &name))?;
        let old = store.get_scope(&from)?;
        let owner = caller.scope.as_deref() == Some(from.as_str())
            && old.as_ref().is_none_or(|r| r.is_held_by(caller));
        let expired = old.as_ref().is_some_and(|r| !r.state(now).is_live());
        if !owner && !expired && reason.is_none() {
            return Err(crate::not_owner(caller, &svc, "move"));
        }
        if store.get_scope(&to)?.is_none() {
            return Err(TabaError::new(
                "E_NOT_FOUND",
                format!("no scope record for {to}"),
            ));
        }
        if store.get(&to, &name)?.is_some() {
            return Err(TabaError::new(
                "E_CONFLICT",
                format!("{to}/{name} already exists"),
            ));
        }
        for s in [&from, &to] {
            if let Some(op) = store.live_op(s, &name)? {
                return Err(crate::busy(&format!("{s}/{name}"), &op));
            }
        }
        let active = crate::borrows::live(store, &svc)?;
        if let Some(reason) = reason.as_deref() {
            crate::record_force(
                store,
                "force_move",
                caller,
                &svc,
                reason,
                if owner || expired {
                    &[]
                } else {
                    &["E_NOT_OWNER"]
                },
            )?;
        }
        claims::transfer(store, &svc, &to, "give")?;
        let new_id = format!("{to}/{name}");
        Ok(Output::new(
            json!({"service":new_id,"from":svc.id(),"how":"give","instance":svc.instance(),"url":svc.url(),"borrows":{"rewritten":active.iter().filter(|b|b.scope!=to).map(|b|&b.id).collect::<Vec<_>>(),"ended":active.iter().filter(|b|b.scope==to).map(|b|&b.id).collect::<Vec<_>>()}}),
            format!("moved {} to {new_id}", svc.id()),
        ))
    } else {
        let scope = caller.require_scope()?;
        let mut rec = store
            .get_scope(&scope)?
            .ok_or_else(|| TabaError::new("E_NOT_FOUND", format!("no scope record for {scope}")))?;
        let from = rec.holder.agent.clone();
        let (holder, how) = if args.take {
            if rec.state(now) == LeaseState::Live && reason.is_none() {
                return Err(TabaError::new(
                    "E_LEASE_LIVE",
                    format!("{scope} is held by {from} under a live lease"),
                )
                .details(json!({"holder":from,"lease":rec.lease_json(now)})));
            }
            (
                Holder::of(caller),
                if reason.is_some() {
                    "take_forced"
                } else {
                    "take_expired"
                },
            )
        } else {
            let to = args
                .to
                .ok_or_else(|| TabaError::usage("use --to AGENT or --take"))?;
            scope::validate_agent(&to)?;
            if !rec.is_held_by(caller) && reason.is_none() {
                return Err(lease::scope_held(&scope, &rec, now));
            }
            if to == from {
                return Err(TabaError::usage("the receiving agent must be different"));
            }
            (
                Holder {
                    agent: to,
                    pid: None,
                    pid_start_time: None,
                },
                "give",
            )
        };
        let services = store.list(Some(&scope))?;
        for svc in &services {
            if let Some(op) = store.live_op(&scope, &svc.name)? {
                return Err(crate::busy(&svc.id(), &op));
            }
        }
        let to = holder.agent.clone();
        let (ttl, source) = cfg.resolve_ttl(None, to == "human")?;
        rec.holder = holder;
        rec.set_ttl(ttl, source);
        rec.renew(now);
        let event = json!({"event":"moved","at":at,"scope":scope,"from":from,"to":to,"how":how,"agent":caller.agent,"reason":reason});
        rec.moves.push(event.clone());
        store.put_scope(&rec)?;
        store.append_event(&event)?;
        Ok(Output::new(
            json!({"scope":scope,"from":from,"to":to,"how":how,"services":services.iter().map(|s|json!({"service":s.id(),"status":crate::status_of(store,s),"instance":s.instance(),"url":s.url()})).collect::<Vec<_>>(),"lease":rec.lease_json(now)}),
            format!("moved {scope} from {from} to {to}"),
        ))
    }
}
