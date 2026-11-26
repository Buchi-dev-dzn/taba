//! Instance-bound shared and exclusive borrows. Mutations hold the state lock.
use crate::{
    config::{Config, Ttl},
    error::{Next, Output, Result, TabaError, Warning},
    lease::{self, Access, Holder},
    scope::Caller,
    state::{self, Service, Store},
    util::{now_secs, parse_rfc3339, rfc3339, shell_quote},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    io,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Borrow {
    pub id: String,
    pub service: String,
    pub instance: String,
    pub scope: String,
    pub holder: Holder,
    pub exclusive: bool,
    pub expires_at: Option<String>,
    pub process: Option<(i32, u64)>,
    #[serde(default)]
    pub broken: bool,
}

fn path(store: &Store, id: &str) -> PathBuf {
    store.root().join("borrows").join(format!("{id}.json"))
}
fn save(store: &Store, b: &Borrow) -> io::Result<()> {
    state::write_json(&path(store, &b.id), b)
}
fn remove(store: &Store, b: &Borrow) -> io::Result<()> {
    state::remove_file_and_empty_parent(&path(store, &b.id))
}
fn all(store: &Store) -> Result<Vec<Borrow>> {
    let entries = match std::fs::read_dir(store.root().join("borrows")) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::<Borrow>::new();
    for e in entries {
        let p = e?.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Some(b) = state::read_json(&p)?
        {
            out.push(b);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
impl Borrow {
    fn alive(&self, store: &Store) -> Result<bool> {
        Ok(self
            .process
            .is_none_or(|(pid, start)| crate::proc::is_alive(pid, start))
            && self
                .expires_at
                .as_deref()
                .and_then(parse_rfc3339)
                .is_none_or(|e| now_secs() < e)
            && store
                .get_scope(&self.scope)?
                .is_some_and(|r| r.holder == self.holder && r.state(now_secs()).is_live()))
    }
    pub fn owned(&self, caller: &Caller) -> bool {
        caller.scope.as_deref() == Some(self.scope.as_str()) && self.holder == Holder::of(caller)
    }
}
pub fn live(store: &Store, svc: &Service) -> Result<Vec<Borrow>> {
    let mut out = Vec::new();
    for b in all(store)? {
        if b.service == svc.id() && b.instance == svc.instance() && !b.broken && b.alive(store)? {
            out.push(b);
        }
    }
    Ok(out)
}
pub fn sweep(store: &Store) -> Result<()> {
    for b in all(store)? {
        if !b.alive(store)? {
            remove(store, &b)?;
        }
    }
    Ok(())
}
pub fn moved(store: &Store, svc: &Service, to: &str) -> Result<()> {
    for mut b in all(store)? {
        if b.service == svc.id() && b.instance == svc.instance() {
            if b.scope == to {
                remove(store, &b)?;
            } else {
                b.service = format!("{to}/{}", svc.name);
                save(store, &b)?;
            }
        }
    }
    Ok(())
}
pub fn break_all(store: &Store, svc: &Service, preserve_exclusive: bool) -> Result<()> {
    for mut b in live(store, svc)? {
        if preserve_exclusive && b.exclusive {
            continue;
        }
        b.broken = true;
        save(store, &b)?;
        lease::notify_warning(store, &b.scope, broken_warning(&b.id, &b.service))?;
        store.append_event(&json!({"at":rfc3339(now_secs()),"event":"borrow_broken","service":svc.id(),"borrow":b.id}))?;
    }
    Ok(())
}
fn broken_warning(id: &str, service: &str) -> Warning {
    let release_command = format!("taba release {}", shell_quote(id));
    Warning {
        code: "W_BORROW_BROKEN",
        message: format!(
            "borrow {}: {} is no longer available; run {release_command} to release this dangling borrow",
            id, service,
        ),
        details: Some(json!({"borrow":id,"service":service,"release_command":release_command})),
    }
}

pub fn rebind(store: &Store, b: &Borrow, new: &Service) -> Result<()> {
    let mut b = b.clone();
    b.instance = new.instance();
    save(store, &b)?;
    Ok(())
}

pub fn borrowed(svc: &Service, bs: &[Borrow]) -> TabaError {
    let scopes: Vec<_> = bs.iter().map(|b| b.scope.as_str()).collect();
    TabaError::new(
        "E_BORROWED",
        format!("{} is borrowed by {}", svc.id(), scopes.join(", ")),
    )
    .details(json!({"service":svc.id(),"borrowers":scopes}))
    .next(Next::run(
        "give the service to its next owner",
        format!("taba disown {}", svc.id()),
    ))
    .next(Next::run(
        "transfer ownership to a borrower",
        format!("taba move {} --to {}", svc.id(), scopes[0]),
    ))
}
fn locked(svc: &Service, b: &Borrow) -> TabaError {
    TabaError::new("E_LOCKED",format!("{} is locked by {}",svc.id(),b.scope)).details(json!({"service":svc.id(),"scope":b.scope,"holder":b.holder.agent,"expires_at":b.expires_at})).next(Next::just("retry later or use --wait-lock SECS"))
}
pub fn authorise(
    store: &Store,
    svc: &Service,
    caller: &Caller,
    force: bool,
    exclusive_ok: bool,
) -> Result<bool> {
    let bs = live(store, svc)?;
    let own = exclusive_ok && bs.iter().any(|b| b.exclusive && b.owned(caller));
    if force || own {
        return Ok(own);
    }
    if let Some(b) = bs.iter().find(|b| b.exclusive) {
        return Err(locked(svc, b));
    }
    let other: Vec<_> = bs
        .into_iter()
        .filter(|b| caller.scope.as_deref() != Some(b.scope.as_str()))
        .collect();
    if !other.is_empty() {
        return Err(borrowed(svc, &other));
    }
    Ok(false)
}

pub struct Request {
    pub target: String,
    pub check: bool,
    pub exclusive: bool,
    pub ttl: Option<Ttl>,
    pub wait: f64,
    pub command: Vec<String>,
    pub json: bool,
}
fn response(b: &Borrow, svc: &Service, other: &[Borrow]) -> Output {
    let mut out = Output::new(
        json!({"borrow":b.id,"kind":if b.exclusive{"exclusive"}else{"shared"},"service":svc.id(),"url":svc.url(),"port":svc.port,"instance":svc.instance(),"expires_at":b.expires_at,"other_borrows":{"shared":other.iter().filter(|x|!x.exclusive && x.id!=b.id).count(),"exclusive":other.iter().find(|x|x.exclusive && x.id!=b.id).map(|x|&x.scope)}}),
        format!(
            "{}: {}{}",
            b.id,
            svc.id(),
            svc.url().map_or(String::new(), |u| format!(" {u}"))
        ),
    );
    if !b.exclusive && b.scope == svc.scope {
        out.warnings.push(Warning {
            code: "W_SAME_SCOPE_BORROW",
            message: format!(
                "borrow {} is in the same scope as {}; it does not prevent the owner from stopping or replacing the service; borrow from another scope if stop protection is needed",
                b.id, svc.id(),
            ),
            details: Some(json!({"borrow":b.id,"service":svc.id()})),
        });
    }
    out
}
fn locate(store: &Store, caller: &Caller, target: &str) -> Result<Vec<Borrow>> {
    let mut out = Vec::new();
    let key = if target.starts_with("b-") && !target.contains('/') {
        target.to_string()
    } else {
        let (s, n) = crate::resolve_target(target, caller)?;
        format!("{s}/{n}")
    };
    for b in all(store)? {
        if (b.id == key || b.service == key) && b.owned(caller) && b.alive(store)? {
            out.push(b);
        }
    }
    if out.is_empty() {
        return Err(TabaError::new(
            "E_NOT_FOUND",
            format!("no borrow for {target}"),
        ));
    }
    Ok(out)
}
pub fn cmd(store: &Store, cfg: &Config, caller: &Caller, a: Request) -> Result<Output> {
    let scope = caller.require_scope()?;
    if a.check {
        let _lock = store.lock()?;
        let mut bs = locate(store, caller, &a.target)?;
        let b = bs.remove(0);
        let (s, n) = crate::resolve_target(&b.service, caller)?;
        let current = store.get(&s, &n)?;
        if let Some(svc) = current
            .as_ref()
            .filter(|svc| !b.broken && svc.instance() == b.instance && svc.is_running())
        {
            return Ok(response(&b, svc, &live(store, svc)?));
        }
        return Err(TabaError::new("E_DANGLING",format!("borrow {} no longer points to a live instance",b.id)).details(json!({"current":current.filter(Service::is_running).map(|s|json!({"instance":s.instance(),"url":s.url()}))})).next(Next::run("borrow again",format!("taba borrow {}", b.service))));
    }
    if !a.wait.is_finite()
        || !(0.0..=86_400.0).contains(&a.wait)
        || (!a.exclusive && (!a.command.is_empty() || a.wait > 0.0))
    {
        return Err(TabaError::usage(
            "--wait-lock and a trailing command require --mut; wait must be between 0 and 86400 seconds",
        ));
    }
    let (s, n) = crate::resolve_target(&a.target, caller)?;
    let deadline = Instant::now() + Duration::from_secs_f64(a.wait);
    let (b, mut out) = loop {
        let lock = store.lock()?;
        let svc = store.get(&s, &n)?.ok_or_else(|| crate::not_found(&s, &n))?;
        if !svc.is_running() {
            return Err(crate::not_found(&s, &n));
        }
        if let Some(op) = store.live_op(&s, &n)? {
            return Err(crate::busy(&svc.id(), &op));
        }
        let other = live(store, &svc)?;
        if a.exclusive
            && let Some(existing) = other.iter().find(|b| b.exclusive)
        {
            if Instant::now() >= deadline {
                return Err(locked(&svc, existing));
            }
            drop(lock);
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let touched = lease::touch(store, cfg, caller, &scope, Access::Create, None)?;
        let record = touched.record.expect("created");
        let t = if a.exclusive && a.command.is_empty() {
            a.ttl.unwrap_or(Ttl::Secs(900)).secs().or(Some(900))
        } else {
            a.ttl.and_then(Ttl::secs)
        };
        let expires_at = t
            .map(|t| rfc3339(now_secs().saturating_add(t)))
            .or(record.lease.expires_at);
        let b = Borrow {
            id: format!("b-{:020}-{}", crate::util::now_micros(), std::process::id()),
            scope: scope.clone(),
            holder: record.holder,
            instance: svc.instance(),
            service: svc.id(),
            exclusive: a.exclusive,
            expires_at,
            process: (!a.command.is_empty()).then(crate::proc::self_identity),
            broken: false,
        };
        save(store, &b)?;
        let mut out = response(&b, &svc, &other);
        out.warnings.extend(touched.warnings);
        break (b, out);
    };
    if let Some((program, args)) = a.command.split_first() {
        let mut c = Command::new(crate::proc::resolve_program(program));
        c.args(args).current_dir(&caller.cwd).env(
            "TABA_BORROWED_URL",
            out.body["url"].as_str().unwrap_or_default(),
        );
        if a.json {
            c.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        let result = if a.json {
            c.output().map(|v| {
                out.body
                    .insert("stdout".into(), json!(String::from_utf8_lossy(&v.stdout)));
                out.body
                    .insert("stderr".into(), json!(String::from_utf8_lossy(&v.stderr)));
                v.status
            })
        } else {
            c.status()
        };
        let _lock = store.lock()?;
        remove(store, &b)?;
        let status = result?;
        out.body.insert("exit_code".into(), json!(status.code()));
        if !status.success() {
            out.failure = Some(TabaError::new(
                "E_BORROW_COMMAND",
                format!("borrowed command exited with {status}"),
            ));
        }
    }
    Ok(out)
}
pub fn release(store: &Store, caller: &Caller, target: &str) -> Result<Output> {
    let _lock = store.lock()?;
    let bs = locate(store, caller, target)?;
    let ids: Vec<_> = bs.iter().map(|b| &b.id).collect();
    for b in &bs {
        remove(store, b)?;
    }
    // A released borrow no longer needs its queued cleanup instruction.
    if let Some(mut rec) = store.get_scope(&caller.require_scope()?)? {
        let before = rec.notices.len();
        rec.notices.retain(|n| {
            n.code != "W_BORROW_BROKEN"
                || !n.details.as_ref().is_some_and(|d| {
                    ids.iter()
                        .any(|id| d["borrow"].as_str() == Some(id.as_str()))
                })
        });
        if rec.notices.len() != before {
            store.put_scope(&rec)?;
        }
    }
    Ok(Output::new(
        json!({"released":ids}),
        format!("released {} borrow(s)", ids.len()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangling_release_command_quotes_the_borrow_id() {
        let id = "b-odd'; echo $HOME";
        let warning = broken_warning(id, "owner/db");
        let command = format!("taba release {}", shell_quote(id));
        assert_eq!(warning.to_json()["details"]["release_command"], command);
        assert_eq!(warning.to_json()["details"]["borrow"], id);
        assert!(warning.message.contains(&command));
        assert_ne!(command, format!("taba release {id}"));
    }
}
