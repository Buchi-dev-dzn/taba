//! `taba ui` (issue #15) outside a terminal. The interactive part is tested
//! headlessly by the unit tests in `src/ui/`.

mod common;

use common::*;
use std::process::Stdio;

#[test]
fn needs_an_interactive_terminal() {
    let fx = Fixture::new("ui_no_tty");
    let out = fx
        .cmd(&fx.outside, &["ui"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("taba ui needs an interactive terminal"),
        "{err}"
    );
    assert!(err.contains("taba ps --watch --json"), "{err}");
    assert!(out.stdout.is_empty());

    // With TABA_OUTPUT=json the refusal is the usual JSON document.
    let out = fx
        .cmd(&fx.outside, &["ui"])
        .env("TABA_OUTPUT", "json")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert_eq!(json(&out)["error"]["code"], "E_USAGE");
}

#[test]
fn is_listed_in_help() {
    let fx = Fixture::new("ui_help");
    let out = fx.taba(&fx.outside, &["--help"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ui "),
        "{}",
        text(&out)
    );
}
