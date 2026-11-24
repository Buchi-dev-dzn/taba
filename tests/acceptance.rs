//! Integration tests for the acceptance criteria in docs/plan.md.
//!
//! Each test uses its own temporary TABA_HOME and small `python3 -m
//! http.server` processes on free high ports.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_taba");

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A temporary TABA_HOME plus scratch directories. On drop, every scope used
/// is stopped through taba and the directory is removed.
struct Env {
    root: PathBuf,
    scopes: Vec<String>,
}

impl Env {
    fn new(test: &str) -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir()
            .join(format!("taba-test-{}-{test}-{n}", std::process::id()))
            .canonicalize_or_create();
        Env {
            root,
            scopes: Vec::new(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn dir(&self, name: &str) -> PathBuf {
        let d = self.root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn taba(&mut self, cwd: &Path, args: &[&str]) -> Output {
        if let Some(i) = args.iter().position(|a| *a == "--scope") {
            let s = args[i + 1].to_string();
            if !self.scopes.contains(&s) {
                self.scopes.push(s);
            }
        }
        Command::new(BIN)
            .args(args)
            .current_dir(cwd)
            .env("TABA_HOME", self.home())
            .env_remove("TABA_SCOPE")
            .output()
            .expect("run taba")
    }

    fn ps_json(&mut self, cwd: &Path) -> Vec<serde_json::Value> {
        let out = self.taba(cwd, &["ps", "--json"]);
        assert!(out.status.success(), "ps failed: {}", text(&out));
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for s in self.scopes.clone() {
            let _ = Command::new(BIN)
                .args(["stop", "--scope", &s, "--timeout", "2"])
                .env("TABA_HOME", self.home())
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

trait CanonOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonOrCreate for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn free_port() -> u16 {
    // Ask the OS for a free ephemeral (high) port.
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_ok(port: u16) -> bool {
    let Ok(mut s) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200")
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn server_cmd(port: u16) -> Vec<String> {
    vec![
        "python3".into(),
        "-m".into(),
        "http.server".into(),
        port.to_string(),
        "--bind".into(),
        "127.0.0.1".into(),
    ]
}

fn run_args<'a>(scope: &'a str, name: &'a str, port: &'a str, cmd: &'a [String]) -> Vec<&'a str> {
    let mut v = vec![
        "run", "--scope", scope, "--name", name, "--port", port, "--",
    ];
    v.extend(cmd.iter().map(|s| s.as_str()));
    v
}

fn find<'a>(rows: &'a [serde_json::Value], service: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["service"] == service)
        .unwrap_or_else(|| panic!("{service} not in ps output: {rows:?}"))
}

/// An http.server not started by taba; killed on drop.
struct Unmanaged(Child);

impl Unmanaged {
    fn start(port: u16, cwd: &Path) -> Unmanaged {
        let child = Command::new("python3")
            .args(server_cmd(port).iter().skip(1))
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("unmanaged server", || http_ok(port));
        Unmanaged(child)
    }
}

impl Drop for Unmanaged {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Criteria 1 and 2: two checkouts serve on different ports, and `ps` from a
/// third directory maps each URL to its checkout directory.
#[test]
fn two_checkouts_on_different_ports_are_listed_by_directory() {
    let mut env = Env::new("checkouts");
    let a = env.dir("checkout-a");
    let b = env.dir("checkout-b");
    let elsewhere = env.dir("third-terminal");
    let (pa, pb) = (free_port(), free_port());
    let (ca, cb) = (server_cmd(pa), server_cmd(pb));

    let out = env.taba(&a, &run_args("feat-a", "web", &pa.to_string(), &ca));
    assert!(out.status.success(), "{}", text(&out));
    let out = env.taba(&b, &run_args("feat-b", "web", &pb.to_string(), &cb));
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server a", || http_ok(pa));
    wait_until("server b", || http_ok(pb));

    let rows = env.ps_json(&elsewhere);
    let ra = find(&rows, "feat-a/web");
    let rb = find(&rows, "feat-b/web");
    assert_eq!(ra["status"], "running");
    assert_eq!(rb["status"], "running");
    assert_eq!(ra["url"], format!("http://localhost:{pa}"));
    assert_eq!(rb["url"], format!("http://localhost:{pb}"));
    assert_eq!(ra["directory"], a.to_str().unwrap());
    assert_eq!(rb["directory"], b.to_str().unwrap());

    // Table output has the expected columns and the URL -> directory pairs.
    let out = env.taba(&elsewhere, &["ps"]);
    let table = String::from_utf8_lossy(&out.stdout).to_string();
    let header = table.lines().next().unwrap();
    for col in ["SERVICE", "STATUS", "URL", "DIRECTORY"] {
        assert!(header.contains(col), "missing {col}: {table}");
    }
    let line_a = table.lines().find(|l| l.starts_with("feat-a/web")).unwrap();
    assert!(
        line_a.contains(&format!("http://localhost:{pa}")) && line_a.contains(a.to_str().unwrap())
    );

    // --scope filter.
    let out = env.taba(&elsewhere, &["ps", "--scope", "feat-b", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["service"], "feat-b/web");
}

/// Criterion 3: re-running the same start does not add a server; conflicting
/// names and ports are rejected without starting or stopping anything.
#[test]
fn rerun_is_idempotent_and_conflicts_are_rejected() {
    let mut env = Env::new("rerun");
    let a = env.dir("checkout-a");
    let b = env.dir("checkout-b");
    let port = free_port();
    let cmd = server_cmd(port);
    let ps = port.to_string();

    let out = env.taba(&a, &run_args("s", "web", &ps, &cmd));
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server", || http_ok(port));
    let pid = find(&env.ps_json(&a), "s/web")["pid"].clone();

    // Same scope/name/command/dir: returns the existing entry.
    let out = env.taba(&a, &run_args("s", "web", &ps, &cmd));
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("already running"));
    let rows = env.ps_json(&a);
    assert_eq!(rows.len(), 1);
    assert_eq!(find(&rows, "s/web")["pid"], pid);

    // Same name, different directory: error.
    let out = env.taba(&b, &run_args("s", "web", &ps, &cmd));
    assert!(!out.status.success(), "{}", text(&out));
    // Same name, different command: error.
    let mut other = cmd.clone();
    other.push("--directory".into());
    other.push(".".into());
    let out = env.taba(&a, &run_args("s", "web", &ps, &other));
    assert!(!out.status.success(), "{}", text(&out));

    // Port held by another taba service: error naming it, nothing started.
    let out = env.taba(&b, &run_args("s2", "web", &ps, &cmd));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("s/web"),
        "{}",
        text(&out)
    );

    // Port held by an unmanaged process: error, nothing started.
    let up = free_port();
    let _unmanaged = Unmanaged::start(up, &b);
    let out = env.taba(&b, &run_args("s2", "api", &up.to_string(), &server_cmd(up)));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not managed by taba"),
        "{}",
        text(&out)
    );

    let rows = env.ps_json(&a);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(find(&rows, "s/web")["pid"], pid);
    assert!(http_ok(port) && http_ok(up));
}

/// Criterion 4: after the launching terminal is gone, logs are still
/// readable; stopping one service leaves the other taba service and an
/// unmanaged server running.
#[test]
fn survives_terminal_close_and_stop_is_targeted() {
    let mut env = Env::new("survive");
    let a = env.dir("checkout-a");
    let b = env.dir("checkout-b");
    let (pa, pb, pu) = (free_port(), free_port(), free_port());
    let home = env.home();
    env.scopes.push("feat-a".into());
    env.scopes.push("feat-b".into());

    // Simulate a terminal: a shell in its own session that launches taba
    // and then stays around, which we then hang up and kill as a group.
    let script = format!(
        "{BIN} run --scope feat-a --name web --port {pa} -- python3 -m http.server {pa} --bind 127.0.0.1 && \
         cd {b} && {BIN} run --scope feat-b --name web --port {pb} -- python3 -m http.server {pb} --bind 127.0.0.1 && \
         exec sleep 600",
        b = b.display()
    );
    let mut terminal = Command::new("/bin/sh");
    terminal
        .args(["-c", &script])
        .current_dir(&a)
        .env("TABA_HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        terminal.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut terminal = terminal.spawn().unwrap();
    wait_until("server a", || http_ok(pa));
    wait_until("server b", || http_ok(pb));
    let tpg = terminal.id() as i32;
    unsafe {
        libc::kill(-tpg, libc::SIGHUP);
        libc::kill(-tpg, libc::SIGKILL);
    }
    terminal.wait().unwrap();

    thread::sleep(Duration::from_millis(300));
    assert!(http_ok(pa), "server a died with its terminal");
    assert!(http_ok(pb), "server b died with its terminal");

    let unmanaged = Unmanaged::start(pu, &a);

    // Logs are readable from a different terminal.
    let out = env.taba(&b, &["logs", "feat-a/web"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("GET / HTTP"),
        "{}",
        text(&out)
    );

    // Stop one service.
    let out = env.taba(&b, &["stop", "feat-a/web"]);
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server a to stop", || !http_ok(pa));
    assert!(http_ok(pb), "other taba service was affected");
    assert!(http_ok(pu), "unmanaged server was affected");

    let rows = env.ps_json(&b);
    assert_eq!(find(&rows, "feat-a/web")["status"], "exited");
    assert_eq!(find(&rows, "feat-b/web")["status"], "running");

    // Stopping does not delete the log or the record.
    let out = env.taba(&b, &["logs", "feat-a/web"]);
    assert!(out.status.success(), "{}", text(&out));

    // stop --scope stops only that scope.
    let out = env.taba(&b, &["stop", "--scope", "feat-b"]);
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server b to stop", || !http_ok(pb));
    assert!(http_ok(pu), "unmanaged server was affected");
    drop(unmanaged);
}

/// `stop` refuses to signal a process whose identity no longer matches the
/// record (e.g. the pid was reused).
#[test]
fn stop_ignores_process_with_mismatched_identity() {
    let mut env = Env::new("identity");
    let a = env.dir("checkout-a");
    let port = free_port();
    let out = env.taba(
        &a,
        &run_args("s", "web", &port.to_string(), &server_cmd(port)),
    );
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server", || http_ok(port));

    let record = env.home().join("services/s/web.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    let real_start = v["start_time"].as_u64().unwrap();
    v["start_time"] = serde_json::json!(real_start + 1);
    std::fs::write(&record, serde_json::to_vec(&v).unwrap()).unwrap();

    let out = env.taba(&a, &["stop", "s/web"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"));
    thread::sleep(Duration::from_millis(500));
    assert!(
        http_ok(port),
        "process with mismatched identity was signalled"
    );
    assert_eq!(find(&env.ps_json(&a), "s/web")["status"], "exited");

    // Restore the record so cleanup can stop it.
    v["start_time"] = serde_json::json!(real_start);
    std::fs::write(&record, serde_json::to_vec(&v).unwrap()).unwrap();
    let out = env.taba(&a, &["stop", "s/web"]);
    assert!(out.status.success(), "{}", text(&out));
    wait_until("server to stop", || !http_ok(port));
}

/// The scope falls back to $TABA_SCOPE, and is required otherwise.
#[test]
fn scope_falls_back_to_env() {
    let mut env = Env::new("envscope");
    let a = env.dir("a");
    env.scopes.push("from-env".into());
    let out = env.taba(&a, &["run", "--name", "x", "--", "sleep", "30"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("TABA_SCOPE"));

    let out = Command::new(BIN)
        .args(["run", "--name", "x", "--", "sleep", "30"])
        .current_dir(&a)
        .env("TABA_HOME", env.home())
        .env("TABA_SCOPE", "from-env")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let rows = env.ps_json(&a);
    assert_eq!(find(&rows, "from-env/x")["status"], "running");
    assert!(rows[0]["url"].is_null());
}
