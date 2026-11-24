//! Shared fixture for the milestone acceptance tests (M1 onwards): a
//! throwaway TABA_HOME and a throwaway git repository with two worktrees,
//! `wt-a` and `wt-b`, plus a directory outside any repository.
#![allow(dead_code)]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_taba");

/// The Python interpreter: `python3` on Unix; on Windows `python3` is often
/// only the Microsoft Store stub, so `python`.
pub const PY: &str = if cfg!(windows) { "python" } else { "python3" };

pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `canonicalize`, without the `\\?\` prefix on Windows (as taba prints paths).
pub fn canon(p: &Path) -> PathBuf {
    let c = p.canonicalize().unwrap();
    let s = c.to_string_lossy().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => c,
    }
}

pub static COUNTER: AtomicU32 = AtomicU32::new(0);
pub static SERIAL: Mutex<()> = Mutex::new(());

pub struct Fixture {
    pub root: PathBuf,
    pub wt_a: PathBuf,
    pub wt_b: PathBuf,
    pub outside: PathBuf,
    _serial: MutexGuard<'static, ()>,
}

pub fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=taba-test",
            "-c",
            "user.email=taba-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            &format!("core.hooksPath={NULL_DEVICE}"),
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", text(&out));
}

impl Fixture {
    pub fn new(test: &str) -> Fixture {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("taba-m1-{}-{test}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = canon(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["worktree", "add", "-q", "../wt-a", "-b", "a"]);
        git(&repo, &["worktree", "add", "-q", "../wt-b", "-b", "b"]);
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        Fixture {
            wt_a: root.join("wt-a"),
            wt_b: root.join("wt-b"),
            outside,
            root,
            _serial: serial,
        }
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn cmd(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(cwd)
            .env("TABA_HOME", self.home())
            .env("PYTHONPATH", python_path())
            .env_remove("TABA_SCOPE")
            .env_remove("TABA_AGENT")
            .env_remove("TABA_OUTPUT");
        c
    }

    pub fn taba(&self, cwd: &Path, args: &[&str]) -> Output {
        self.cmd(cwd, args).output().expect("run taba")
    }

    /// The scope taba derives for `dir`.
    pub fn scope_of(&self, dir: &Path) -> String {
        let v = json(&self.taba(dir, &["whoami", "--json"]));
        v["scope"].as_str().expect("a scope").to_string()
    }

    pub fn ps(&self, args: &[&str]) -> Vec<Value> {
        let mut all = vec!["ps", "--json"];
        all.extend_from_slice(args);
        let out = self.taba(&self.outside, &all);
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["services"]
            .as_array()
            .unwrap()
            .clone()
    }

    pub fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("one JSON object per line"))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(dirs) = std::fs::read_dir(self.home().join("services")) {
            for d in dirs.flatten() {
                let s = d.file_name().to_string_lossy().into_owned();
                let _ = Command::new(BIN)
                    .args(["stop", "--scope", &s, "--timeout", "2"])
                    .env("TABA_HOME", self.home())
                    .output();
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

pub fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", text(out)))
}

pub fn exit(out: &Output) -> i32 {
    out.status.code().expect("exited normally")
}

pub fn http_ok(port: u16) -> bool {
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

pub fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
        && TcpListener::bind(("0.0.0.0", port)).is_ok()
        && TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))
            .is_err()
}

pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

pub const SERVER: [&str; 6] = [PY, "-m", "http.server", "{port}", "--bind", "127.0.0.1"];

/// `run --name NAME --port auto --wait --json -- python3 -m http.server {port}`
pub fn run_auto(name: &str) -> Vec<&str> {
    let mut v = vec![
        "run", "--name", name, "--port", "auto", "--wait", "--json", "--",
    ];
    v.extend_from_slice(&SERVER);
    v
}

pub const SLEEP_30: &str = "import time; time.sleep(30)";

/// A directory for `PYTHONPATH` with a `sitecustomize.py` that skips the
/// reverse DNS lookup `http.server` does on startup (`socket.getfqdn`),
/// which takes over 30 s on GitHub's macOS runners.
pub fn python_path() -> &'static str {
    static DIR: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("taba-pythonpath");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(format!("sitecustomize.py.{}", std::process::id()));
        std::fs::write(
            &tmp,
            "import socket\nsocket.getfqdn = lambda name='': name or 'localhost'\n",
        )
        .unwrap();
        std::fs::rename(&tmp, dir.join("sitecustomize.py")).unwrap();
        dir.to_str().unwrap().to_string()
    });
    &DIR
}

pub fn port_of(v: &Value) -> u16 {
    v["port"].as_u64().expect("a port") as u16
}

/// An http.server not started by taba; killed on drop.
pub struct Unmanaged(pub Child);

impl Unmanaged {
    pub fn start(port: u16, cwd: &Path) -> Unmanaged {
        let child = Command::new(PY)
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(cwd)
            .env("PYTHONPATH", python_path())
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
