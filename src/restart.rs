//! `taba restart S/N`: stop one service and start it again with the settings
//! it was started with, as one operation under the per-service lock.
//!
//! The checks are `stop`'s (owner, exclusive borrower or `--force --reason`;
//! `E_BORROWED` for other scopes' shared borrows), and the start is `run`'s
//! (`launch` and `await_ready`), so a restart behaves like a replacing `run`
//! under `borrow --mut`: shared borrows end up dangling, the caller's own
//! exclusive borrow follows the new instance.

use crate::error::{Next, Output, Result, TabaError, Warning};
use crate::ports::{self, PortSpec, RunRequest};
use crate::ready::{self, ReadySpec};
use crate::state::{OpLock, Service, Store};
use crate::{borrows, config, lease, proc, scope};
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};

/// `taba restart` flags.
#[derive(clap::Args)]
pub struct Args {
    /// SCOPE/NAME, or NAME in the caller's scope
    pub target: String,
    /// The caller's scope (default: $TABA_SCOPE, else the git worktree)
    #[arg(long)]
    pub scope: Option<String>,
    /// Agent acting (default: $TABA_AGENT, else "human")
    #[arg(long)]
    pub agent: Option<String>,
    /// Seconds to wait after SIGTERM (Windows: Ctrl+Break) before SIGKILL
    /// (Windows: TerminateJobObject)
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    /// Block until the new instance is ready (default: as it was run, else
    /// 30 seconds when given without a value)
    #[arg(long, value_name = "SECS", num_args = 0..=1, require_equals = true,
          default_missing_value = "30")]
    pub wait: Option<f64>,
    /// Ready check for --wait (default: the one it was run with, else tcp
    /// with a port)
    #[arg(long, value_parser = ready::parse_ready_spec, requires = "wait",
          value_name = "tcp|http:PATH|log:REGEX")]
    pub ready: Option<ReadySpec>,
    /// Restart a service of another scope (needs --reason; recorded)
    #[arg(long)]
    pub force: bool,
    /// Why --force is needed (recorded in events.jsonl)
    #[arg(long, requires = "force")]
    pub reason: Option<String>,
    /// Print one JSON document (also: TABA_OUTPUT=json)
    #[arg(long)]
    pub json: bool,
}

/// How long a restart waits for the old instance's port to be released
/// before it gives up on keeping it.
const PORT_RELEASE_WAIT: Duration = Duration::from_secs(3);

pub fn cmd(r: Args) -> Result<Output> {
    if !r.timeout.is_finite() || r.timeout < 0.0 {
        return Err(TabaError::usage(
            "--timeout must be a non-negative number of seconds",
        ));
    }
    let timeout = Duration::from_secs_f64(r.timeout);
    let wait_flag = match r.wait {
        None => None,
        Some(s) if s.is_finite() && s > 0.0 => Some(Duration::from_secs_f64(s)),
        Some(_) => {
            return Err(TabaError::usage(
                "--wait takes a positive number of seconds",
            ));
        }
    };
    let reason = crate::check_force(r.force, r.reason)?;
    let caller = scope::resolve(r.scope, r.agent)?;
    let (s, n) = crate::resolve_target(&r.target, &caller)?;
    let store = Store::open()?;
    restart_one(
        &proc::System,
        &store,
        &caller,
        &s,
        &n,
        timeout,
        wait_flag,
        r.ready,
        reason.as_deref(),
    )
}

/// The ready check and timeout: the flags, else what the instance was
/// started with (`run --wait --ready`).
fn wait_settings(
    old: &Service,
    wait_flag: Option<Duration>,
    ready_flag: Option<ReadySpec>,
) -> Result<(Option<ReadySpec>, Option<Duration>)> {
    let recorded = old.wait_request.as_ref();
    let wait = wait_flag.or_else(|| recorded.map(|w| Duration::from_millis(w.ms)));
    let ready = match ready_flag {
        Some(r) => Some(r),
        None => match recorded.and_then(|w| w.ready.as_deref()) {
            Some("port") => Some(ReadySpec::Port),
            Some(r) => ready::parse_ready_spec(r).ok(),
            // Older short-run records did not save the discovery check in
            // wait_request, but a successful wait recorded ready.check.
            None if old.ready.as_ref().is_some_and(|r| r.check == "port") => Some(ReadySpec::Port),
            None => None,
        },
    };
    if let Some(r) = &ready
        && r.needs_port()
        && old.port_request().is_none()
    {
        return Err(TabaError::usage(format!(
            "--ready {} needs a port, and {} has none (use log:REGEX)",
            r.name(),
            old.id()
        )));
    }
    let check = wait.and_then(|_| ready.or_else(|| old.port.map(|_| ReadySpec::Tcp)));
    Ok((check, wait))
}

#[allow(clippy::too_many_arguments)]
fn restart_one(
    ops: &impl proc::Ops,
    store: &Store,
    caller: &scope::Caller,
    s: &str,
    n: &str,
    timeout: Duration,
    wait_flag: Option<Duration>,
    ready_flag: Option<ReadySpec>,
    force_reason: Option<&str>,
) -> Result<Output> {
    // 1. Checks and the operation lock, under the global lock (as `stop`).
    let (old, preserve_exclusive, check, wait, mut warnings) = {
        let _lock = store.lock()?;
        let old = store.get(s, n)?.ok_or_else(|| crate::not_found(s, n))?;
        let owner = caller.scope.as_deref() == Some(old.scope.as_str());
        let mut warnings: Vec<Warning> = Vec::new();
        if owner && force_reason.is_none() {
            let cfg = config::Config::load(store.root())?;
            warnings = lease::touch(store, &cfg, caller, s, lease::Access::Mutate, None)?.warnings;
        }
        let exclusive = !owner
            && borrows::live(store, &old)?
                .iter()
                .any(|b| b.exclusive && b.owned(caller));
        if !owner && !exclusive && force_reason.is_none() {
            return Err(crate::not_owner(caller, &old, "restart"));
        }
        let preserve_exclusive =
            borrows::authorise(store, &old, caller, force_reason.is_some(), true)?
                && force_reason.is_none();
        if let Some(op) = store.live_op(s, n)? {
            return Err(crate::busy(&old.id(), &op));
        }
        store.clear_stale_op(s, n)?;
        let (check, wait) = wait_settings(&old, wait_flag, ready_flag)?;
        store.put_op(s, n, &OpLock::mine("restart", &caller.agent))?;
        if let Some(reason) = force_reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            crate::record_force(store, "force_restart", caller, &old, reason, overrode)?;
        }
        (old, preserve_exclusive, check, wait, warnings)
    };
    let release = |e: TabaError| -> TabaError {
        match store.lock() {
            Ok(_lock) => match store.release_op(s, n) {
                Ok(()) => e,
                Err(io) => TabaError::state(format!("{}; and: {io}", e.message)),
            },
            Err(_) => e,
        }
    };

    // 2. Stop the old instance, without the global lock.
    let was_running = ops.is_running(&old);
    let stopped = if was_running {
        match ops.stop_group(&old, timeout) {
            Ok(proc::StopResult::Terminated) => "stopped",
            Ok(proc::StopResult::Killed) => "killed",
            Err(e) => {
                return Err(release(
                    TabaError::new(
                        "E_SIGNAL_FAILED",
                        format!(
                            "{}: cannot signal process group {}: {e}; it was not restarted",
                            old.id(),
                            old.pgid
                        ),
                    )
                    .details(json!({ "service": old.id(), "pgid": old.pgid })),
                ));
            }
        }
    } else {
        "not_running"
    };
    // stop_group bounds its post-kill wait, so a successful signal does
    // not guarantee exit. Never detach borrows or overwrite the only
    // record of a group that is still running.
    if ops.is_running(&old) {
        return Err(release(
            TabaError::new(
                "E_SIGNAL_FAILED",
                format!(
                    "{}: process group {} is still running after stop; it was not restarted",
                    old.id(),
                    old.pgid
                ),
            )
            .details(json!({ "service": old.id(), "pgid": old.pgid })),
        ));
    }
    if let Some(p) = old.port {
        // A just-stopped server can take a moment to release its port.
        let deadline = Instant::now() + PORT_RELEASE_WAIT;
        while ports::port_in_use(p) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }

    // 3. Start the new instance with the recorded settings, under the
    //    global lock (as a replacing `run`).
    let started = (|| -> Result<(Service, u64)> {
        let lock = store.lock()?;
        borrows::break_all(store, &old, preserve_exclusive)?;
        let mine = if preserve_exclusive {
            borrows::live(store, &old)?
                .into_iter()
                .find(|b| b.exclusive && b.owned(caller))
        } else {
            None
        };
        let template = old.template().to_vec();
        let port_request = old.port_request();
        let req = RunRequest {
            scope_flag: Some(s),
            name: n,
            template: &template,
        };
        let port = match port_request
            .as_deref()
            .map(ports::parse_port_spec)
            .transpose()
            .map_err(|e| TabaError::state(format!("{}: recorded port: {e}", old.id())))?
        {
            None => None,
            Some(PortSpec::Fixed(p)) => {
                ports::check_port_free(store, p, s, &req)?;
                Some(p)
            }
            Some(PortSpec::Auto { first, last, .. }) => match old.port {
                // Keep the URL when the old port is free again.
                Some(p) if ports::check_port_free(store, p, s, &req).is_ok() => Some(p),
                _ => Some(ports::pick_auto(store, first, last, s, n)?),
            },
        };
        let mut wait_request = old.wait_request.clone();
        if let Some(w) = &mut wait_request
            && w.ready.is_none()
            && old.ready.as_ref().is_some_and(|r| r.check == "port")
        {
            // Upgrade legacy short-run requests before ready is reset, so
            // even a discovery timeout on this restart keeps the next wait.
            w.ready = Some(ReadySpec::Port.request());
        }
        let (svc, log_offset) = crate::restart_launch(
            store,
            crate::RestartLaunch {
                scope: s,
                name: n,
                template,
                cwd: &old.cwd,
                env: old.env.clone(),
                port,
                port_request,
                agent: &caller.agent,
                service_ttl: old.service_ttl_secs,
                moves: old.moves.clone(),
                wait_request,
            },
        )?;
        store.put(&svc)?;
        if let Some(b) = &mine {
            borrows::rebind(store, b, &svc)?;
        }
        drop(lock);
        Ok((svc, log_offset))
    })();
    let (svc, log_offset) = started.map_err(|e| {
        release(e.next(Next::run(
            &format!("{} was stopped but not started again; retry", old.id()),
            format!("taba restart {}", old.id()),
        )))
    })?;
    if old.port.is_some() && svc.port != old.port {
        warnings.push(Warning {
            code: "W_PORT_CHANGED",
            message: format!(
                "port {} was not free again; {} now listens on {}",
                old.port.unwrap_or(0),
                svc.id(),
                svc.url().unwrap_or_default()
            ),
        });
    }

    // 4. Wait for it (without the global lock) and report as `run` does.
    let (svc, outcome) = crate::restart_await_ready(store, svc, check.as_ref(), wait, log_offset)?;
    let mut out = crate::run_result(
        svc,
        "restarted",
        check.as_ref(),
        wait,
        outcome,
        warnings,
        None,
    )?;
    out.body.insert(
        "previous".into(),
        json!({
            "instance": old.instance(),
            "pid": old.pid,
            "port": old.port,
            "result": stopped,
        }),
    );
    if force_reason.is_some() {
        out.body.insert("forced".into(), json!(true));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WaitRequest;

    fn svc(port: Option<u16>, wait: Option<WaitRequest>) -> Service {
        Service {
            scope: "s1".into(),
            name: "web".into(),
            command: vec!["x".into()],
            cwd: std::env::temp_dir(),
            port,
            pid: 1,
            pgid: 1,
            start_time: 1,
            log_path: std::env::temp_dir().join("web.log"),
            command_template: None,
            port_request: None,
            instance: None,
            started_by: None,
            ready: None,
            service_ttl_secs: None,
            service_expires_at: None,
            moves: Vec::new(),
            stop_requested_at: None,
            wait_request: wait,
            env: Vec::new(),
            discovered_ports: Vec::new(),
        }
    }

    #[test]
    fn wait_settings_reuse_the_recorded_ones_unless_given() {
        let rec = Some(WaitRequest {
            ms: 7000,
            ready: Some("http:/health".into()),
        });
        let (check, wait) = wait_settings(&svc(Some(4100), rec.clone()), None, None).unwrap();
        assert_eq!(wait, Some(Duration::from_secs(7)));
        assert!(matches!(check, Some(ReadySpec::Http(p)) if p == "/health"));

        let (check, wait) = wait_settings(
            &svc(Some(4100), rec),
            Some(Duration::from_secs(2)),
            Some(ReadySpec::Tcp),
        )
        .unwrap();
        assert_eq!(wait, Some(Duration::from_secs(2)));
        assert!(matches!(check, Some(ReadySpec::Tcp)));

        // Not started with --wait: no wait, as `run` without it.
        let (check, wait) = wait_settings(&svc(Some(4100), None), None, None).unwrap();
        assert!(check.is_none() && wait.is_none());
        // --wait with a port and no --ready: tcp.
        let (check, _) =
            wait_settings(&svc(Some(4100), None), Some(Duration::from_secs(1)), None).unwrap();
        assert!(matches!(check, Some(ReadySpec::Tcp)));
    }

    #[test]
    fn wait_settings_preserve_discovery_without_changing_explicit_portless_runs() {
        let mut old = svc(
            None,
            Some(WaitRequest {
                ms: 3000,
                ready: Some("port".into()),
            }),
        );
        let (check, wait) = wait_settings(&old, None, None).unwrap();
        assert!(matches!(check, Some(ReadySpec::Port)));
        assert_eq!(wait, Some(Duration::from_secs(3)));

        // Legacy short-run records saved only the successful ready check.
        old.wait_request.as_mut().unwrap().ready = None;
        old.ready = Some(crate::state::Ready {
            check: "port".into(),
            ready_at: 1,
        });
        assert!(matches!(
            wait_settings(&old, None, None).unwrap().0,
            Some(ReadySpec::Port)
        ));
        let (check, wait) = wait_settings(
            &old,
            Some(Duration::from_secs(1)),
            Some(ready::parse_ready_spec("log:up").unwrap()),
        )
        .unwrap();
        assert!(matches!(check, Some(ReadySpec::Log(_))));
        assert_eq!(wait, Some(Duration::from_secs(1)));

        // An explicit no-port run with --wait never requested discovery.
        old.ready = None;
        assert!(wait_settings(&old, None, None).unwrap().0.is_none());
    }

    #[test]
    fn a_stop_that_leaves_the_group_running_preserves_record_and_borrows() {
        struct StillRunning(bool);
        impl proc::Ops for StillRunning {
            fn is_running(&self, _: &Service) -> bool {
                true
            }
            fn stop_group(&self, _: &Service, _: Duration) -> std::io::Result<proc::StopResult> {
                Ok(if self.0 {
                    proc::StopResult::Killed
                } else {
                    proc::StopResult::Terminated
                })
            }
        }

        for killed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::at(dir.path().to_path_buf());
            let old = svc(None, None);
            store.put(&old).unwrap();
            let caller = scope::Caller {
                scope: Some("borrower".into()),
                source: None,
                worktree: None,
                branch: None,
                agent: "human".into(),
                agent_pid: None,
                cwd: dir.path().to_path_buf(),
            };
            let cfg = config::Config::load(store.root()).unwrap();
            let touched = lease::touch(
                &store,
                &cfg,
                &caller,
                "borrower",
                lease::Access::Create,
                None,
            )
            .unwrap();
            let borrow = borrows::Borrow {
                id: "shared".into(),
                service: old.id(),
                instance: old.instance(),
                scope: "borrower".into(),
                holder: touched.record.unwrap().holder,
                exclusive: false,
                expires_at: None,
                process: None,
                broken: false,
            };
            let path = store.root().join("borrows/shared.json");
            crate::state::write_json(&path, &borrow).unwrap();
            let before = serde_json::to_value(&old).unwrap();
            let borrow_before = std::fs::read(&path).unwrap();

            let err = restart_one(
                &StillRunning(killed),
                &store,
                &caller,
                "s1",
                "web",
                Duration::ZERO,
                None,
                None,
                Some("test stuck group"),
            )
            .err()
            .expect("a live old group must prevent replacement");
            assert_eq!(err.code, "E_SIGNAL_FAILED");
            assert!(err.message.contains("still running"), "{}", err.message);
            assert_eq!(
                serde_json::to_value(store.get("s1", "web").unwrap().unwrap()).unwrap(),
                before
            );
            assert_eq!(std::fs::read(&path).unwrap(), borrow_before);
            assert_eq!(borrows::live(&store, &old).unwrap().len(), 1);
            assert!(store.live_op("s1", "web").unwrap().is_none());
            assert!(
                !store.log_path("s1", "web").exists(),
                "must not launch a replacement"
            );
        }
    }

    #[test]
    fn a_port_check_needs_a_port() {
        let e = wait_settings(
            &svc(None, None),
            Some(Duration::from_secs(1)),
            Some(ReadySpec::Tcp),
        )
        .unwrap_err();
        assert_eq!(e.code, "E_USAGE");
    }
}
