//! Argument errors in JSON mode and `--help`; no service is started.
mod common;
use common::*;
use serde_json::Value;

fn usage_error(f: &Fixture, args: &[&str]) -> Value {
    let out = f
        .cmd(&f.outside, args)
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert_eq!(exit(&out), 2, "{}", text(&out));
    assert!(out.stderr.is_empty(), "{}", text(&out));
    let v = json(&out);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "E_USAGE", "{v}");
    v
}

#[test]
fn json_usage_errors_name_the_argument_and_the_subcommand_help() {
    let f = Fixture::new("cli");
    // The issue's example: the missing argument is named, and next points
    // to the subcommand's help rather than a placeholder.
    let v = usage_error(&f, &["run", "--", "x"]);
    let e = &v["error"];
    assert!(e["message"].as_str().unwrap().contains("--name"), "{v}");
    assert_eq!(e["details"]["arguments"][0], "--name <NAME>", "{v}");
    assert!(
        e["details"]["usage"]
            .as_str()
            .unwrap()
            .starts_with("taba run "),
        "{v}"
    );
    assert_eq!(e["next"][0]["command"], "taba run --help", "{v}");

    for (args, arg, help) in [
        (
            &["stop", "--timeout", "soon", "x"][..],
            "--timeout",
            "taba stop --help",
        ),
        (
            &["lease", "end", "--bogus"][..],
            "--bogus",
            "taba lease end --help",
        ),
        (
            &["run", "--name", "w", "--port", "nope", "--", "x"][..],
            "--port",
            "taba run --help",
        ),
    ] {
        let v = usage_error(&f, args);
        let e = &v["error"];
        assert!(
            e["message"].as_str().unwrap().contains(arg),
            "{args:?}: {v}"
        );
        assert!(
            e["details"]["arguments"][0]
                .as_str()
                .unwrap()
                .starts_with(arg),
            "{v}"
        );
        assert_eq!(e["next"][0]["command"], help, "{args:?}: {v}");
    }
    let v = usage_error(&f, &["stopp", "--json"]);
    assert_eq!(v["error"]["details"]["subcommand"], "stopp", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("'stop'"),
        "{v}"
    );
    assert_eq!(v["error"]["next"][0]["command"], "taba --help", "{v}");
}

#[test]
fn help_describes_every_flag() {
    let f = Fixture::new("help");
    let out = f.taba(&f.outside, &["run", "--help"]);
    assert!(out.status.success(), "{}", text(&out));
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--ttl <TTL>", "--service-ttl <SERVICE_TTL>"] {
        let line = help
            .lines()
            .position(|l| l.trim_start().starts_with(flag))
            .unwrap_or_else(|| panic!("{flag} missing:\n{help}"));
        let description = help.lines().nth(line + 1).unwrap_or("").trim();
        assert!(
            !description.is_empty() && !description.starts_with('-'),
            "{flag} has no description:\n{help}"
        );
    }
    // Help stays plain text with exit 0, also when JSON output is requested.
    let out = f
        .cmd(&f.outside, &["gc", "--help"])
        .env("TABA_OUTPUT", "json")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("--dry-run"));
}
