//! `taba ui`: a keyboard-driven terminal UI over the same state as the CLI.
//!
//! Every scope is a collapsible tree of its services, refreshed once per
//! second from the same snapshot code as `taba ps` (in-process, without
//! `ps`'s lease peek, so watching changes nothing). `x` stops or removes
//! through the same functions as `taba stop` / `taba rm`, as the caller
//! resolved like any CLI call (`$TABA_SCOPE`, `$TABA_AGENT`, git worktree),
//! and never with `--force`. Quitting never stops anything.
//!
//! Built on ratatui with the crossterm backend (macOS, Linux and Windows
//! terminals). The state and key handling live in `app.rs` and drawing in
//! `view.rs`, both testable without a terminal.

pub mod app;
pub mod view;

use crate::error::{Next, Output, Result, TabaError};
use crate::scope::{self, Caller};
use crate::state::Store;
use app::{Action, App, Effect, Mode, Row};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use view::Colors;

/// How often state is re-read.
const REFRESH: Duration = Duration::from_secs(1);
/// How often a followed log and a running action are checked.
const FAST: Duration = Duration::from_millis(100);
/// `taba stop`'s default `--timeout`.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How much of a log is read when it is opened.
const LOG_START: u64 = 1024 * 1024;

pub fn command() -> Result<Option<Output>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TabaError::usage(
            "taba ui needs an interactive terminal; programs can read `taba ps --watch --json`",
        )
        .next(Next::run(
            "stream the services as JSON",
            "taba ps --watch --json",
        )));
    }
    let caller = scope::resolve(None, None)?;
    let store = Store::open()?;
    let mut app = App::new(caller.scope.clone());
    refresh(&store, &mut app)?;
    let colors = Colors::detect();
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            ratatui::restore();
            return Err(TabaError::state(format!("cannot set up the terminal: {e}")));
        }
    };
    let res = event_loop(&mut terminal, &mut app, &store, &caller, colors);
    let _ = terminal.show_cursor();
    ratatui::restore();
    res.map(|()| None)
}

/// Re-reads state into `app`.
fn refresh(store: &Store, app: &mut App) -> Result<()> {
    let snap = crate::ps_snapshot(store, None, false)?;
    app.set_rows(Row::from_snapshot(snap), crate::util::now_secs());
    Ok(())
}

/// Runs a confirmed `x` action: the same functions as `taba stop SCOPE/NAME`,
/// `taba rm SCOPE/NAME` and, for a scope line, `taba stop` of each of its
/// services, so ownership checks, leases, borrows and the audit log behave
/// as on the command line. Never forced.
pub fn execute(store: &Store, caller: &Caller, action: &Action) -> Result<Output> {
    match action {
        Action::Stop { scope, name } => crate::stop_services(
            store,
            caller,
            &[(scope.clone(), name.clone())],
            true,
            STOP_TIMEOUT,
            None,
        ),
        Action::Remove { scope, name } => crate::rm_services(store, caller, None, |_| {
            Ok((vec![(scope.clone(), name.clone())], true))
        }),
        Action::StopScope { targets, .. } => {
            crate::stop_services(store, caller, targets, false, STOP_TIMEOUT, None)
        }
    }
}

type Running = (JoinHandle<()>, Receiver<Result<Output>>, Action);

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    store: &Store,
    caller: &Caller,
    colors: Colors,
) -> Result<()> {
    let io_err = |e: io::Error| TabaError::state(format!("terminal: {e}"));
    let mut running: Option<Running> = None;
    let mut tail: Option<LogTail> = None;
    let mut next_refresh = Instant::now() + REFRESH;
    loop {
        terminal
            .draw(|f| view::render(f, app, colors))
            .map_err(io_err)?;
        let now = Instant::now();
        let mut wait = next_refresh.saturating_duration_since(now);
        if tail.is_some() || running.is_some() {
            wait = wait.min(FAST);
        }
        // Windows reports key releases too; they are ignored.
        if event::poll(wait).map_err(io_err)?
            && let Event::Key(k) = event::read().map_err(io_err)?
            && k.kind != KeyEventKind::Release
        {
            match app.handle_key(k) {
                Effect::Quit => return Ok(()),
                Effect::Run(action) => {
                    let (tx, rx) = channel();
                    let (caller, a) = (caller.clone(), action.clone());
                    let handle = thread::spawn(move || {
                        let res = Store::open()
                            .map_err(TabaError::from)
                            .and_then(|store| execute(&store, &caller, &a));
                        let _ = tx.send(res);
                    });
                    running = Some((handle, rx, action));
                }
                Effect::OpenLog(path) => tail = Some(LogTail::new(path)),
                Effect::None => {}
            }
        }
        if app.mode != Mode::Logs {
            tail = None;
        }
        if let (Some(t), Some(log)) = (tail.as_mut(), app.log.as_mut()) {
            let bytes = t.poll();
            if !bytes.is_empty() {
                log.push(&bytes);
            }
        }
        if let Some((handle, rx, action)) = running.take() {
            match rx.try_recv() {
                Ok(res) => {
                    app.finish(&action, res);
                    next_refresh = Instant::now();
                }
                Err(_) if handle.is_finished() => {
                    // The worker died without an answer (a panic).
                    app.finish(
                        &action,
                        Err(TabaError::state("the action failed unexpectedly")),
                    );
                    next_refresh = Instant::now();
                }
                Err(_) => running = Some((handle, rx, action)),
            }
        }
        if Instant::now() >= next_refresh {
            if let Err(e) = refresh(store, app) {
                app.set_error(&e);
            }
            next_refresh = Instant::now() + REFRESH;
        }
    }
}

/// Follows a log file like `taba logs -f`: the last [`LOG_START`] bytes
/// first, then whatever is appended.
struct LogTail {
    path: PathBuf,
    file: Option<File>,
    pos: u64,
}

impl LogTail {
    fn new(path: PathBuf) -> LogTail {
        LogTail {
            path,
            file: None,
            pos: 0,
        }
    }

    /// New bytes since the last call (none if the file cannot be read).
    fn poll(&mut self) -> Vec<u8> {
        if self.file.is_none() {
            let Ok(f) = File::open(&self.path) else {
                return Vec::new();
            };
            let len = f.metadata().map_or(0, |m| m.len());
            self.pos = len.saturating_sub(LOG_START);
            self.file = Some(f);
        }
        let Some(f) = self.file.as_mut() else {
            return Vec::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < self.pos {
            self.pos = 0; // truncated
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.pos)).is_ok() && f.read_to_end(&mut buf).is_ok() {
            self.pos += buf.len() as u64;
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    //! The `x` rules end to end against a real (temporary) state directory:
    //! the TUI's actions go through the CLI's functions, so they are refused
    //! and reported exactly as `taba stop` / `taba rm` would.
    use super::app::tests::key;
    use super::*;
    use crate::scope::ScopeSource;
    use crate::state::Service;
    use ratatui::crossterm::event::KeyCode;
    use std::process::{Command, Stdio};

    struct Home {
        dir: PathBuf,
        store: Store,
        spawned: Vec<crate::proc::Spawned>,
    }

    impl Home {
        fn new(test: &str) -> Home {
            let dir = std::env::temp_dir().join(format!("taba-ui-{}-{test}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Store::at(dir.clone());
            Home {
                dir,
                store,
                spawned: Vec::new(),
            }
        }

        fn caller(&self, scope: &str) -> Caller {
            Caller {
                scope: Some(scope.into()),
                source: Some(ScopeSource::Flag),
                worktree: None,
                branch: None,
                agent: "human".into(),
                agent_pid: None,
                cwd: self.dir.clone(),
            }
        }

        /// A service record; `running` starts a real detached process.
        fn add(&mut self, scope: &str, name: &str, running: bool) {
            let (pid, pgid, start_time) = if running {
                let mut cmd = if cfg!(windows) {
                    let mut c = Command::new("ping");
                    c.args(["-n", "60", "127.0.0.1"]);
                    c
                } else {
                    let mut c = Command::new("sleep");
                    c.arg("60");
                    c
                };
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let sp = crate::proc::spawn_detached(cmd).expect("spawn");
                self.spawned.push(sp);
                (sp.pid, sp.pgid, sp.start_time)
            } else {
                (999_999, 999_999, 1)
            };
            let log_path = self.store.log_path(scope, name);
            std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
            std::fs::write(&log_path, "hello\n").unwrap();
            self.store
                .put(&Service {
                    scope: scope.into(),
                    name: name.into(),
                    command: vec!["sleep".into()],
                    cwd: self.dir.clone(),
                    port: None,
                    pid,
                    pgid,
                    start_time,
                    log_path,
                    command_template: None,
                    port_request: None,
                    instance: None,
                    started_by: None,
                    ready: None,
                    service_ttl_secs: None,
                    service_expires_at: None,
                    moves: Vec::new(),
                    stop_requested_at: None,
                    env_keys: Vec::new(),
                    env_hash: None,
                    discovered_ports: Vec::new(),
                })
                .unwrap();
        }

        /// An app as `scope` sees it.
        fn app(&self, scope: &str) -> App {
            let mut app = App::new(Some(scope.into()));
            refresh(&self.store, &mut app).unwrap();
            app
        }

        /// Selects `target`, presses `x` then `y`, runs the action as
        /// `caller_scope`, and records the outcome.
        fn x(&self, app: &mut App, target: app::Item, caller_scope: &str) -> Action {
            // Unfold the item's scope, then select the item.
            let (app::Item::Service(s, _) | app::Item::Scope(s)) = &target;
            app.cursor = Some(app::Item::Scope(s.clone()));
            app.handle_key(key(KeyCode::Right));
            assert!(app.items().contains(&target), "{target:?} not shown");
            app.cursor = Some(target);
            app.handle_key(key(KeyCode::Char('x')));
            let Effect::Run(action) = app.handle_key(key(KeyCode::Char('y'))) else {
                panic!("x did not ask to run anything: {:?}", app.notice);
            };
            let res = execute(&self.store, &self.caller(caller_scope), &action);
            app.finish(&action, res);
            refresh(&self.store, app).unwrap();
            action
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            for sp in &self.spawned {
                if crate::proc::group_is_ours(sp.pid, sp.pgid, sp.start_time) {
                    let _ = crate::proc::stop_group(
                        sp.pid,
                        sp.pgid,
                        sp.start_time,
                        Duration::from_millis(200),
                    );
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn svc(s: &str, n: &str) -> app::Item {
        app::Item::Service(s.into(), n.into())
    }

    #[test]
    fn x_stops_an_own_running_service() {
        let mut h = Home::new("stop");
        h.add("mine", "web", true);
        let mut app = h.app("mine");
        assert_eq!(app.rows[0].status, "running");
        let action = h.x(&mut app, svc("mine", "web"), "mine");
        assert_eq!(
            action,
            Action::Stop {
                scope: "mine".into(),
                name: "web".into()
            }
        );
        let notice = app.notice.clone().unwrap();
        assert!(!notice.error, "{notice:?}");
        assert!(notice.text.starts_with("mine/web: stopped"), "{notice:?}");
        // The next refreshes show it exited (on Windows the terminated job
        // can take a moment to empty).
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.rows[0].status != "exited" && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            refresh(&h.store, &mut app).unwrap();
        }
        assert_eq!(app.rows[0].status, "exited");
        assert!(app.reasons.is_empty());
    }

    #[test]
    fn x_on_an_exited_service_removes_it() {
        let mut h = Home::new("rm");
        h.add("mine", "old", false);
        let mut app = h.app("mine");
        let action = h.x(&mut app, svc("mine", "old"), "mine");
        assert!(matches!(action, Action::Remove { .. }));
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "mine/old: removed (record and log)"
        );
        assert!(app.rows.is_empty());
        assert!(h.store.get("mine", "old").unwrap().is_none());
    }

    #[test]
    fn another_scopes_service_is_refused_with_the_cli_reason() {
        let mut h = Home::new("not_owner");
        h.add("theirs", "api", true);
        h.add("theirs", "old", false);
        let mut app = h.app("mine");
        h.x(&mut app, svc("theirs", "api"), "mine");
        let reason = app.reasons["theirs/api"].clone();
        // Exactly what `taba stop theirs/api` prints.
        let cli = crate::stop_services(
            &h.store,
            &h.caller("mine"),
            &[("theirs".into(), "api".into())],
            true,
            STOP_TIMEOUT,
            None,
        )
        .err()
        .unwrap();
        assert_eq!(cli.code, "E_NOT_OWNER");
        assert_eq!(reason, cli.message);
        assert!(reason.contains("taba does not stop another scope's service"));
        assert!(app.notice.as_ref().unwrap().error);
        assert_eq!(app.row("theirs", "api").unwrap().status, "running");
        // Removing another scope's exited service is refused too.
        h.x(&mut app, svc("theirs", "old"), "mine");
        assert!(app.reasons["theirs/old"].contains("taba does not rm another scope's service"));
        assert!(h.store.get("theirs", "old").unwrap().is_some());
    }

    #[test]
    fn a_borrowed_service_is_refused() {
        let mut h = Home::new("borrowed");
        h.add("mine", "db", true);
        // Scope `other` borrows mine/db (its lease record is created live).
        let cfg = crate::config::Config::load(h.store.root()).unwrap();
        crate::borrows::cmd(
            &h.store,
            &cfg,
            &h.caller("other"),
            crate::borrows::Request {
                target: "mine/db".into(),
                check: false,
                exclusive: false,
                ttl: None,
                wait: 0.0,
                command: Vec::new(),
                json: true,
            },
        )
        .unwrap();
        let mut app = h.app("mine");
        assert_eq!(app.rows[0].borrowers, ["other"]);
        h.x(&mut app, svc("mine", "db"), "mine");
        assert_eq!(app.reasons["mine/db"], "mine/db is borrowed by other");
        assert_eq!(app.rows[0].status, "running");
    }

    #[test]
    fn x_on_a_scope_stops_each_service_and_reports_refusals_per_row() {
        let mut h = Home::new("scope");
        h.add("theirs", "a", true);
        h.add("theirs", "b", true);
        let mut app = h.app("mine");
        let action = h.x(&mut app, app::Item::Scope("theirs".into()), "mine");
        assert!(matches!(action, Action::StopScope { .. }));
        assert!(app.reasons["theirs/a"].starts_with("theirs/a belongs to scope theirs"));
        assert!(app.reasons["theirs/b"].starts_with("theirs/b belongs to scope theirs"));
        let n = app.notice.clone().unwrap();
        assert!(
            n.error && n.text.contains("2 of 2 service(s) were not stopped"),
            "{n:?}"
        );
        assert!(app.rows.iter().all(|r| r.status == "running"));
    }

    #[test]
    fn log_tail_reads_and_follows() {
        let h = Home::new("tail");
        let path = h.dir.join("x.log");
        std::fs::create_dir_all(&h.dir).unwrap();
        let mut t = LogTail::new(path.clone());
        assert!(t.poll().is_empty()); // no file yet
        std::fs::write(&path, "one\n").unwrap();
        assert_eq!(t.poll(), b"one\n");
        assert!(t.poll().is_empty());
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"two\n").unwrap();
        drop(f);
        assert_eq!(t.poll(), b"two\n");
        std::fs::write(&path, "new\n").unwrap(); // truncated and rewritten
        assert_eq!(t.poll(), b"new\n");
    }
}
