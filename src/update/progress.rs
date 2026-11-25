//! What `taba update` shows on a terminal while it works: a download bar
//! and one line per verification, on stderr. Nothing is shown with `--json`,
//! when stderr is not a terminal, or with `TERM=dumb`; stdout and the JSON
//! output never change. Only plain text and `\r` are used (no escape
//! sequences), so it renders the same in every terminal.

use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Width of the `#`/`-` bar.
const BAR: usize = 30;
/// Width of a cleared line.
const LINE: usize = 72;

pub struct Progress {
    out: Option<RefCell<Box<dyn Write>>>,
    /// The bar currently drawn: (done, total, last drawn percent or bytes).
    state: RefCell<Option<(u64, Option<u64>, u64)>>,
}

/// Set while a bar is drawn, so Ctrl+C can clear it first.
static BAR_ACTIVE: AtomicBool = AtomicBool::new(false);

impl Progress {
    pub fn disabled() -> Progress {
        Progress {
            out: None,
            state: RefCell::new(None),
        }
    }

    /// Progress written to `out` (tests).
    pub fn to(out: Box<dyn Write>) -> Progress {
        Progress {
            out: Some(RefCell::new(out)),
            state: RefCell::new(None),
        }
    }

    /// Progress on stderr when a person watches it: not with `--json`, not
    /// when stderr is not a terminal, not with `TERM=dumb`.
    pub fn for_terminal(json: bool) -> Progress {
        let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
        if !enabled(json, io::stderr().is_terminal(), dumb) {
            return Progress::disabled();
        }
        install_interrupt_cleanup();
        Progress::to(Box::new(io::stderr()))
    }

    pub fn is_enabled(&self) -> bool {
        self.out.is_some()
    }

    fn write(&self, s: &str) {
        if let Some(out) = &self.out {
            let mut out = out.borrow_mut();
            let _ = out.write_all(s.as_bytes());
            let _ = out.flush();
        }
    }

    /// Starts a download of `name`; `total` is its size when known.
    pub fn start(&self, name: &str, total: Option<u64>) {
        if !self.is_enabled() {
            return;
        }
        self.write(&format!("Downloading {name}\n"));
        *self.state.borrow_mut() = Some((0, total, u64::MAX));
        BAR_ACTIVE.store(true, Ordering::SeqCst);
        self.draw(0);
    }

    /// `done` bytes have arrived.
    pub fn advance(&self, done: u64) {
        if self.is_enabled() && self.state.borrow().is_some() {
            self.draw(done);
        }
    }

    fn draw(&self, done: u64) {
        let Some((_, total, last)) = *self.state.borrow() else {
            return;
        };
        // Redraw only when the visible text changes.
        let key = match total {
            Some(t) if t > 0 => done.min(t) * 100 / t,
            _ => done / (64 * 1024),
        };
        if key == last {
            return;
        }
        *self.state.borrow_mut() = Some((done, total, key));
        self.write(&format!("\r{}", bar_line(done, total)));
    }

    /// The download finished: the bar is completed and ended with a newline.
    pub fn done(&self) {
        let state = self.state.borrow_mut().take();
        if let Some((done, total, _)) = state {
            let done = total.unwrap_or(done);
            self.write(&format!("\r{}\n", bar_line(done, total.or(Some(done)))));
            BAR_ACTIVE.store(false, Ordering::SeqCst);
        }
    }

    /// The download failed or was abandoned: the bar line is cleared.
    pub fn abandon(&self) {
        if self.state.borrow_mut().take().is_some() {
            self.write(&clear_line());
            BAR_ACTIVE.store(false, Ordering::SeqCst);
        }
    }

    /// One line for a verification step and its result.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", test)),
        allow(dead_code)
    )]
    pub fn verified(&self, what: &str, ok: bool) {
        self.write(&format!(
            "Verifying {what} ... {}\n",
            if ok { "ok" } else { "failed" }
        ));
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.abandon();
    }
}

pub fn enabled(json: bool, stderr_is_terminal: bool, term_dumb: bool) -> bool {
    !json && stderr_is_terminal && !term_dumb
}

fn clear_line() -> String {
    format!("\r{}\r", " ".repeat(LINE))
}

/// `1.5 MB`, `640 KB`, `12 B` (decimal units).
pub fn size(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{} KB", n / 1_000)
    } else {
        format!("{n} B")
    }
}

/// `[##########--------------------]  33%  1.1/3.4 MB`, or with an unknown
/// size `1.1 MB downloaded`; padded so a shorter redraw leaves nothing
/// behind.
pub fn bar_line(done: u64, total: Option<u64>) -> String {
    let s = match total {
        Some(t) if t > 0 => {
            let done = done.min(t);
            let filled = (done as usize * BAR) / t as usize;
            let pct = done * 100 / t;
            let (d, t) = if t >= 1_000_000 {
                (
                    format!("{:.1}", done as f64 / 1e6),
                    format!("{:.1} MB", t as f64 / 1e6),
                )
            } else {
                (size(done), size(t))
            };
            format!(
                "[{}{}] {pct:>3}%  {d}/{t}",
                "#".repeat(filled),
                "-".repeat(BAR - filled)
            )
        }
        _ => format!("{} downloaded", size(done)),
    };
    format!("{s:<width$}", width = LINE)
}

/// Ctrl+C while a bar is drawn: clear the bar line, then let SIGINT end
/// taba as before. (`taba update` only downloads on Linux and macOS.)
#[cfg(unix)]
fn install_interrupt_cleanup() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    extern "C" fn on_sigint(_: libc::c_int) {
        if BAR_ACTIVE.load(Ordering::SeqCst) {
            // A fixed buffer: write(2) is async-signal-safe, formatting is not.
            const CLEAR: &[u8] =
                b"\r                                                                        \r";
            // SAFETY: write, signal and raise are async-signal-safe.
            unsafe {
                libc::write(2, CLEAR.as_ptr().cast(), CLEAR.len());
            }
        }
        // SAFETY: restores the default action and re-raises SIGINT.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::raise(libc::SIGINT);
        }
    }
    ONCE.call_once(|| {
        let handler: extern "C" fn(libc::c_int) = on_sigint;
        // SAFETY: installs a handler that only uses async-signal-safe calls.
        unsafe {
            libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        }
    });
}

#[cfg(not(unix))]
fn install_interrupt_cleanup() {}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::rc::Rc;

    /// A writer whose bytes the test can read back.
    #[derive(Clone, Default)]
    pub struct Shared(pub Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn shown_only_for_a_person_at_a_terminal() {
        assert!(enabled(false, true, false));
        assert!(!enabled(true, true, false)); // --json
        assert!(!enabled(false, false, false)); // pipe, CI, agent
        assert!(!enabled(false, true, true)); // TERM=dumb
    }

    #[test]
    fn bar_lines() {
        assert_eq!(
            bar_line(2_100_000, Some(3_400_000)).trim_end(),
            "[##################------------]  61%  2.1/3.4 MB"
        );
        assert_eq!(
            bar_line(0, Some(500)).trim_end(),
            "[------------------------------]   0%  0 B/500 B"
        );
        assert_eq!(bar_line(1_234_567, None).trim_end(), "1.2 MB downloaded");
        assert_eq!(bar_line(5, Some(5)).len(), LINE);
    }

    #[test]
    fn disabled_prints_nothing() {
        let p = Progress::disabled();
        p.start("x", Some(10));
        p.advance(5);
        p.done();
        p.verified("x", true);
        assert!(!p.is_enabled());
    }

    #[test]
    fn known_and_unknown_sizes_and_clean_ends() {
        let buf = Shared::default();
        let p = Progress::to(Box::new(buf.clone()));
        p.start("taba.tar.gz", Some(4_000_000));
        p.advance(1_000_000);
        p.advance(1_000_001); // same percent: no redraw
        p.advance(4_000_000);
        p.done();
        p.verified("SHA256SUMS", true);
        p.start("other.tar.gz", None);
        p.advance(70_000);
        p.abandon();
        drop(p);
        let out = String::from_utf8(buf.0.borrow().clone()).unwrap();
        let frames: Vec<&str> = out.split(['\r', '\n']).map(str::trim_end).collect();
        assert_eq!(
            frames,
            [
                "Downloading taba.tar.gz",
                "",
                "[------------------------------]   0%  0.0/4.0 MB",
                "[#######-----------------------]  25%  1.0/4.0 MB",
                "[##############################] 100%  4.0/4.0 MB",
                "[##############################] 100%  4.0/4.0 MB",
                "Verifying SHA256SUMS ... ok",
                "Downloading other.tar.gz",
                "",
                "0 B downloaded",
                "70 KB downloaded",
                "",
                "",
            ]
        );
    }
}
