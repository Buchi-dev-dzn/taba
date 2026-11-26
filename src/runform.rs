//! The two forms of `taba run`, and the name rule of the short one.
//!
//! - **Explicit form** (`--name` given): exactly the flags given, nothing
//!   added. This is what agents and scripts use.
//! - **Short form** (no `--name`): a human shorthand. The name comes from
//!   the command ([`derive_name`]) and `run` waits (30 s, or `--wait=SECS`)
//!   for the service to listen on a port, which it reports. Without
//!   `--port`, the app chooses its own port; taba sets none.
//!
//! In both forms, `taba run [taba flags] <command...>`: taba's flags come
//! first, and everything from the first token that is not a taba flag
//! belongs to the command (`--` before it is optional).
//!
//! Kept separate from `cmd_run` so that other entry points that start
//! services (such as a `taba.toml` runner) resolve names and defaults the
//! same way.

use crate::error::{Next, Result, TabaError};
use crate::state::Service;
use crate::util::tilde;
use serde_json::json;
use std::path::Path;

/// How long the short form waits for a port, in seconds (the same as a
/// bare `--wait`).
pub const SHORT_FORM_WAIT_SECS: f64 = 30.0;

/// A name derived from a command, and the words of the command it came
/// from (shown to the user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub name: String,
    pub from: Vec<String>,
}

impl Derived {
    /// `"npm run dev"`.
    pub fn source_text(&self) -> String {
        self.from.join(" ")
    }
}

/// What `run` starts with, after applying the form's defaults.
#[derive(Debug, Clone)]
pub struct RunForm {
    pub name: String,
    /// Set for the short form: where the name came from.
    pub derived: Option<Derived>,
    pub wait: Option<f64>,
    /// Wait for (and report) the first listening port, rather than run a
    /// ready check: the short form without `--ready`.
    pub wait_for_port: bool,
}

/// Resolves the name and wait of a `run`. With `name`, everything is kept
/// as given (the explicit form). Without it, the name is derived from
/// `command`, and `run` waits for a listening port (unless `ready`: a
/// ready check was given, which is then used as in the explicit form).
pub fn resolve(
    name: Option<String>,
    wait: Option<f64>,
    ready: bool,
    command: &[String],
) -> Result<RunForm> {
    if let Some(name) = name {
        return Ok(RunForm {
            name,
            derived: None,
            wait,
            wait_for_port: false,
        });
    }
    let derived = derive_name(command).ok_or_else(|| {
        TabaError::usage(format!(
            "cannot derive a service name from the command {:?}; pass --name NAME",
            command.join(" ")
        ))
    })?;
    Ok(RunForm {
        name: derived.name.clone(),
        derived: Some(derived),
        wait: Some(wait.unwrap_or(SHORT_FORM_WAIT_SECS)),
        wait_for_port: !ready,
    })
}

// ------------------------------------------------------------ the command

/// taba's `run` flags that take a separate value (`--name web`). `--wait`
/// takes its value only as `--wait=SECS`.
pub const RUN_VALUE_FLAGS: &[&str] = &[
    "--scope",
    "--agent",
    "--name",
    "--port",
    "--ready",
    "--ttl",
    "--service-ttl",
];
/// taba's `run` flags without a separate value.
pub const RUN_SWITCHES: &[&str] = &["--wait", "--json", "--help", "-h"];

/// How the command was written on the `run` command line, read from the
/// raw arguments (clap cannot tell these apart).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    /// taba options came before the command.
    pub options_first: bool,
    /// A `--` separated taba's options from the command.
    pub separator: bool,
}

/// Is `arg` one of `run`'s flags (`--name`, `--name=web`, `--wait=5`)?
fn is_run_flag(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
    RUN_VALUE_FLAGS.contains(&flag) || RUN_SWITCHES.contains(&flag)
}

/// Whether `arg` is a value flag given without `=`, so the next argument
/// is its value.
fn takes_next(arg: &str) -> bool {
    RUN_VALUE_FLAGS.contains(&arg)
}

/// Prepares the raw arguments (with the program name first) of a `taba
/// run` for clap, and records the command's [`Shape`]. Other commands
/// pass through unchanged.
///
/// A quoted command (one argument with whitespace, no `--` before it) may
/// be followed by taba options: `taba run "npm run dev" --name web`. They
/// are moved in front of it here, so clap sees
/// `taba run --name web -- "npm run dev"`. Anything after the quotes that
/// is not a taba option is an error that says to move it inside them.
pub fn prepare(raw: Vec<String>) -> Result<(Vec<String>, Shape)> {
    let Some(run_at) = raw
        .iter()
        .skip(1)
        .position(|a| !a.starts_with('-'))
        .map(|i| i + 1)
    else {
        return Ok((raw, Shape::default()));
    };
    if raw[run_at] != "run" {
        return Ok((raw, Shape::default()));
    }
    let mut shape = Shape::default();
    let mut i = run_at + 1;
    let start = loop {
        let Some(a) = raw.get(i) else {
            return Ok((raw, shape));
        };
        if a == "--" {
            shape.separator = true;
            return Ok((raw, shape));
        }
        if !a.starts_with('-') {
            break i;
        }
        shape.options_first = true;
        i += if takes_next(a) { 2 } else { 1 };
    };
    let command = &raw[start];
    if !command.chars().any(char::is_whitespace) || start + 1 == raw.len() {
        return Ok((raw, shape));
    }
    // A quoted command followed by more arguments: they must be taba's.
    let tail = &raw[start + 1..];
    let mut options = Vec::new();
    let mut strays = Vec::new();
    let mut j = 0;
    while j < tail.len() {
        let a = &tail[j];
        if a != "--" && a.starts_with('-') && is_run_flag(a) {
            options.push(a.clone());
            if takes_next(a) {
                match tail.get(j + 1) {
                    Some(v) => options.push(v.clone()),
                    None => {
                        return Err(TabaError::usage(format!("{a} needs a value")));
                    }
                }
                j += 1;
            }
        } else {
            strays.push(a.clone());
        }
        j += 1;
    }
    if let Some(first) = strays.first() {
        let mut inside = command.clone();
        if is_script_run(command) && !command.split_whitespace().any(|w| w == "--") {
            inside.push_str(" --");
        }
        for s in &strays {
            inside.push(' ');
            inside.push_str(s);
        }
        let mut fixed = vec!["taba".to_string()];
        fixed.extend(raw[run_at..start].iter().cloned());
        fixed.push(inside);
        fixed.extend(options);
        let fixed = display_argv(&fixed);
        return Err(TabaError::usage(format!(
            "{first} is not a taba option; if it is for your command, put it inside the \
             quotes: {fixed}"
        ))
        .details(json!({ "arguments": strays }))
        .next(Next::run("put it inside the quotes", fixed)));
    }
    let mut args = raw[..start].to_vec();
    args.extend(options);
    args.push("--".into());
    args.push(command.clone());
    Ok((args, shape))
}

/// `npm|pnpm|yarn|bun run ...`: arguments for the script need `--`.
fn is_script_run(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let program = words.next().map(program_stem).unwrap_or_default();
    SCRIPT_RUNNERS.contains(&program.to_ascii_lowercase().as_str()) && words.next() == Some("run")
}

/// The shell that runs a quoted command, and its flag.
#[cfg(not(windows))]
const SHELL: [&str; 2] = ["/bin/sh", "-c"];
#[cfg(windows)]
const SHELL: [&str; 2] = ["cmd", "/C"];

/// The command to start, as taba understood it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The argv to start (before `{port}` substitution). For a quoted
    /// command: the shell, its flag and the command line.
    pub template: Vec<String>,
    /// The words the name is derived from.
    pub words: Vec<String>,
}

/// Applies the command rules to the command clap collected:
///
/// 1. No taba options: everything is the command (`taba run npm run dev`).
/// 2. One argument with whitespace (and no `--`): a command line, run by
///    the shell (`/bin/sh -c`, Windows `cmd /C`), so `&&` and `|` work.
/// 3. After `--`, the arguments are the command, untouched.
/// 4. taba options before an unquoted command of several words need `--`;
///    the error carries the corrected command line (`details.corrected`).
///
/// `raw`: the original arguments, program name first.
pub fn command_spec(shape: Shape, command: Vec<String>, raw: &[String]) -> Result<CommandSpec> {
    if !shape.separator && command.len() == 1 && command[0].chars().any(char::is_whitespace) {
        let line = command[0].clone();
        let mut template: Vec<String> = SHELL.iter().map(|s| s.to_string()).collect();
        template.push(line.clone());
        return Ok(CommandSpec {
            template,
            words: line.split_whitespace().map(String::from).collect(),
        });
    }
    if shape.options_first && !shape.separator && command.len() > 1 {
        let fixed = with_separator(raw, &command);
        return Err(TabaError::usage(format!(
            "with taba options, put -- before the command (or quote the command): {fixed}"
        ))
        .details(json!({ "corrected": fixed }))
        .next(Next::run("run it with --", fixed)));
    }
    Ok(CommandSpec {
        words: command.clone(),
        template: command,
    })
}

/// The corrected command line for rule 4: `--` inserted before the
/// command. `raw` is the original arguments, program name first.
fn with_separator(raw: &[String], command: &[String]) -> String {
    let at = raw.len() - command.len();
    let mut fixed = vec!["taba".to_string()];
    fixed.extend(raw[1..at].iter().cloned());
    fixed.push("--".into());
    fixed.extend(command.iter().cloned());
    display_argv(&fixed)
}

/// `taba run --name NAME` for the same command: `-- argv`, or the quoted
/// command line.
pub fn with_name(name: &str, template: &[String]) -> String {
    match shell_line(template) {
        Some(line) => format!(
            "taba run --name {name} {}",
            display_argv(&[line.to_string()])
        ),
        None => format!("taba run --name {name} -- {}", display_argv(template)),
    }
}

/// The command line of a template that runs a quoted command through the
/// shell, if it is one.
pub fn shell_line(template: &[String]) -> Option<&str> {
    match template {
        [sh, flag, line] if sh == SHELL[0] && flag == SHELL[1] => Some(line),
        _ => None,
    }
}

/// How `run` shows a command: `npm run dev -- --open`, or for a quoted
/// one, `npm run dev && echo done (via sh)`.
pub fn display_command(template: &[String]) -> String {
    match shell_line(template) {
        Some(line) => {
            let shell = if cfg!(windows) { "cmd" } else { "sh" };
            format!("{line} (via {shell})")
        }
        None => display_argv(template),
    }
}

/// Words joined by spaces, each quoted for a POSIX shell when needed.
pub fn display_argv(words: &[String]) -> String {
    let quote = |w: &String| {
        let plain = !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,{}".contains(c));
        if plain {
            w.clone()
        } else if !w.contains('"') && !w.contains('$') && !w.contains('`') && !w.contains('\\') {
            format!("\"{w}\"")
        } else {
            format!("'{}'", w.replace('\'', "'\\''"))
        }
    };
    words.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// Package managers whose `run X` names a package script.
const SCRIPT_RUNNERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];

/// Derives a service name from a command:
///
/// - `npm|pnpm|yarn|bun run X` → `X`, and `pnpm X` → `X`
/// - `flutter run -d D` (or `--device-id D`) → `flutter-D`
/// - otherwise the program's basename (`./bin/api.exe` → `api`)
///
/// Only the words right after the program count; options elsewhere are
/// part of the command, not of the name. The result is lowercased, and
/// characters a name cannot hold become `-`. Returns `None` when nothing
/// usable remains.
pub fn derive_name(command: &[String]) -> Option<Derived> {
    let program = command.first()?;
    let stem = program_stem(program).to_ascii_lowercase();
    let arg = |i: usize| command.get(i).map(String::as_str);
    let is_word = |s: &str| !s.starts_with('-');
    let derived = |raw: &str, words: usize| {
        sanitize(raw).map(|name| Derived {
            name,
            from: command[..words].to_vec(),
        })
    };

    if SCRIPT_RUNNERS.contains(&stem.as_str())
        && arg(1) == Some("run")
        && let Some(script) = arg(2).filter(|s| is_word(s))
    {
        return derived(script, 3);
    }
    if stem == "pnpm"
        && let Some(script) = arg(1).filter(|s| is_word(s) && *s != "run")
    {
        return derived(script, 2);
    }
    if stem == "flutter" && arg(1) == Some("run") {
        let mut i = 2;
        while let Some(a) = arg(i) {
            if a == "--" {
                break;
            }
            if (a == "-d" || a == "--device-id")
                && let Some(device) = arg(i + 1)
            {
                return derived(&format!("flutter-{device}"), i + 2);
            }
            if let Some(device) = a
                .strip_prefix("--device-id=")
                .or_else(|| a.strip_prefix("-d"))
                .filter(|d| !d.is_empty())
            {
                return derived(&format!("flutter-{device}"), i + 1);
            }
            i += 1;
        }
        return derived("flutter", 2);
    }
    derived(program_stem(program), 1)
}

/// The basename of a program, without a Windows executable extension.
fn program_stem(program: &str) -> &str {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if base.len() > ext.len() && base[base.len() - ext.len()..].eq_ignore_ascii_case(ext) {
            return &base[..base.len() - ext.len()];
        }
    }
    base
}

/// Lowercases, turns every character other than `a-z`, `0-9`, `-` and `_`
/// into `-`, collapses runs of `-` and trims them from both ends.
fn sanitize(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(64);
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

/// A short-form run must not take over a service of the same derived name
/// that was started from another command or directory (for example
/// `npm run dev` in two packages of one worktree). Returns the error, or
/// `None` when `existing` is the same service.
pub fn derived_name_clash(
    existing: &Service,
    derived: &Derived,
    template: &[String],
    cwd: &Path,
) -> Option<TabaError> {
    if existing.template() == template && existing.cwd == cwd {
        return None;
    }
    let id = existing.id();
    let name = &existing.name;
    Some(
        TabaError::new(
            "E_CONFLICT",
            format!(
                "name {name:?} (from {:?}) is already used by {id}: \
                 {:?} in {}; this run: {:?} in {}. Set another name with --name",
                derived.source_text(),
                existing.template().join(" "),
                tilde(&existing.cwd),
                template.join(" "),
                tilde(cwd),
            ),
        )
        .details(json!({
            "service": id, "command": existing.template(), "directory": existing.cwd,
            "port": existing.port, "port_request": existing.port_request(),
            "instance": existing.instance(), "derived_name": name,
            "requested_command": template, "requested_directory": cwd,
        }))
        .next(Next::run(
            "set another name",
            with_name(&format!("{name}-2"), template),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(cmd: &str) -> Vec<String> {
        cmd.split_whitespace().map(String::from).collect()
    }

    fn derive(cmd: &str) -> Option<(String, String)> {
        derive_name(&words(cmd)).map(|d| (d.name.clone(), d.source_text()))
    }

    #[test]
    fn derives_names_from_commands() {
        for (cmd, name, from) in [
            ("npm run dev", "dev", "npm run dev"),
            ("npm run dev -- --port 3000", "dev", "npm run dev"),
            ("pnpm run dev", "dev", "pnpm run dev"),
            ("yarn run dev", "dev", "yarn run dev"),
            ("bun run dev", "dev", "bun run dev"),
            ("npm.cmd run dev", "dev", "npm.cmd run dev"),
            ("npm run build:watch", "build-watch", "npm run build:watch"),
            ("pnpm dev", "dev", "pnpm dev"),
            ("pnpm dev --host", "dev", "pnpm dev"),
            ("flutter run", "flutter", "flutter run"),
            (
                "flutter run -d chrome",
                "flutter-chrome",
                "flutter run -d chrome",
            ),
            (
                "flutter run -d macos",
                "flutter-macos",
                "flutter run -d macos",
            ),
            (
                "flutter run --release -d macos --web-port 8080",
                "flutter-macos",
                "flutter run --release -d macos",
            ),
            (
                "flutter run --device-id=chrome",
                "flutter-chrome",
                "flutter run --device-id=chrome",
            ),
            (
                "flutter run --device-id emulator-5554",
                "flutter-emulator-5554",
                "flutter run --device-id emulator-5554",
            ),
            (
                "flutter run -dchrome",
                "flutter-chrome",
                "flutter run -dchrome",
            ),
            (
                "flutter.bat run -d windows",
                "flutter-windows",
                "flutter.bat run -d windows",
            ),
            ("flutter run --flavor dev", "flutter", "flutter run"),
            ("flutter run -- -d chrome", "flutter", "flutter run"),
            // Everything else: the program's basename.
            ("flutter test", "flutter", "flutter"),
            ("npm start", "npm", "npm"),
            ("npm --prefix web run dev", "npm", "npm"),
            ("npm run --silent dev", "npm", "npm"),
            ("yarn dev", "yarn", "yarn"),
            ("bun dev", "bun", "bun"),
            ("npx vite", "npx", "npx"),
            ("python3 -m http.server 8000", "python3", "python3"),
            ("node server.js --port 4000 --json", "node", "node"),
            ("vite", "vite", "vite"),
            (
                "./node_modules/.bin/vite --port 3000",
                "vite",
                "./node_modules/.bin/vite",
            ),
            (
                "/usr/local/bin/redis-server",
                "redis-server",
                "/usr/local/bin/redis-server",
            ),
            (r"C:\tools\Caddy.EXE run", "caddy", r"C:\tools\Caddy.EXE"),
            ("my_tool.v2", "my_tool-v2", "my_tool.v2"),
        ] {
            assert_eq!(
                derive(cmd),
                Some((name.to_string(), from.to_string())),
                "{cmd}"
            );
        }
    }

    #[test]
    fn derived_names_are_valid_and_bounded() {
        assert_eq!(derive("..."), None);
        assert_eq!(derive("npm run ::"), None);
        let long = "x".repeat(100);
        assert_eq!(derive(&long).unwrap().0.len(), 64);
        for cmd in ["npm run dev", "./a b c", "Über-Tool", "flutter run -d ../x"] {
            let (name, _) = derive(cmd).unwrap();
            crate::scope::check_ident("name", &name).unwrap();
        }
    }

    #[test]
    fn short_form_waits_for_a_port_and_the_explicit_form_adds_nothing() {
        let cmd = words("npm run dev");
        let short = resolve(None, None, false, &cmd).unwrap();
        assert_eq!(short.name, "dev");
        assert_eq!(short.derived.unwrap().source_text(), "npm run dev");
        assert_eq!(short.wait, Some(SHORT_FORM_WAIT_SECS));
        assert!(short.wait_for_port);

        let short = resolve(None, Some(5.0), false, &cmd).unwrap();
        assert_eq!(short.wait, Some(5.0));
        // A ready check replaces the port wait.
        assert!(!resolve(None, Some(5.0), true, &cmd).unwrap().wait_for_port);

        let explicit = resolve(Some("web".into()), None, false, &cmd).unwrap();
        assert_eq!(explicit.name, "web");
        assert!(explicit.derived.is_none());
        assert!(explicit.wait.is_none());
        assert!(!explicit.wait_for_port);

        let err = resolve(None, None, false, &words("...")).unwrap_err();
        assert_eq!(err.code, "E_USAGE");
        assert!(err.message.contains("--name"), "{}", err.message);
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prepare_moves_options_after_a_quoted_command_in_front_of_it() {
        let shape = |raw: &[&str]| prepare(strings(raw)).unwrap();
        // Unchanged: other commands, unquoted commands, `--`.
        for raw in [
            &["taba", "ps", "--json"][..],
            &["taba", "run", "npm", "run", "dev", "--", "--host"],
            &[
                "taba",
                "run",
                "--name",
                "web",
                "--",
                "npm run dev",
                "--json",
            ],
            &["taba", "run", "--name", "web", "npm run dev"],
        ] {
            assert_eq!(shape(raw).0, strings(raw), "{raw:?}");
        }
        assert_eq!(shape(&["taba", "run", "npm", "run"]).1, Shape::default());
        assert_eq!(
            shape(&["taba", "run", "--name", "w", "--", "x"]).1,
            Shape {
                options_first: true,
                separator: true
            }
        );
        assert_eq!(
            shape(&["taba", "run", "--name", "w", "x", "y"]).1,
            Shape {
                options_first: true,
                separator: false
            }
        );
        // Options after a quoted command go in front of it.
        let (args, s) = shape(&[
            "taba",
            "run",
            "npm run dev",
            "--name",
            "web",
            "--wait=5",
            "--json",
        ]);
        assert_eq!(
            args,
            strings(&[
                "taba",
                "run",
                "--name",
                "web",
                "--wait=5",
                "--json",
                "--",
                "npm run dev"
            ])
        );
        assert_eq!(s, Shape::default());

        let err = prepare(strings(&[
            "taba",
            "run",
            "npm run dev",
            "--name",
            "web",
            "--open",
        ]))
        .unwrap_err();
        assert_eq!(
            err.message,
            "--open is not a taba option; if it is for your command, put it inside the quotes: \
             taba run \"npm run dev -- --open\" --name web"
        );
        // No `--` is added for commands other than package scripts.
        let err = prepare(strings(&["taba", "run", "flutter run", "-d", "chrome"])).unwrap_err();
        assert!(
            err.message.ends_with("taba run \"flutter run -d chrome\""),
            "{}",
            err.message
        );
        let err = prepare(strings(&["taba", "run", "a b", "--name"])).unwrap_err();
        assert_eq!(err.message, "--name needs a value");
    }

    #[test]
    fn command_spec_applies_the_four_rules() {
        let raw = |w: &[&str]| strings(w);
        // 1. No options: argv as given.
        let spec = command_spec(
            Shape::default(),
            strings(&["npm", "run", "dev", "--", "--host"]),
            &raw(&["taba", "run", "npm", "run", "dev", "--", "--host"]),
        )
        .unwrap();
        assert_eq!(
            spec.template,
            strings(&["npm", "run", "dev", "--", "--host"])
        );
        assert_eq!(display_command(&spec.template), "npm run dev -- --host");
        // 2. A quoted command: the shell.
        let spec = command_spec(
            Shape::default(),
            strings(&["npm run dev && echo ok"]),
            &raw(&["taba", "run", "npm run dev && echo ok"]),
        )
        .unwrap();
        assert_eq!(spec.template[2], "npm run dev && echo ok");
        assert_eq!(shell_line(&spec.template), Some("npm run dev && echo ok"));
        assert_eq!(
            spec.words,
            strings(&["npm", "run", "dev", "&&", "echo", "ok"])
        );
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert_eq!(
            display_command(&spec.template),
            format!("npm run dev && echo ok (via {shell})")
        );
        // 3. After `--`, untouched, even one argument with spaces.
        let sep = Shape {
            options_first: true,
            separator: true,
        };
        let spec = command_spec(
            sep,
            strings(&["my app"]),
            &raw(&["taba", "run", "--json", "--", "my app"]),
        )
        .unwrap();
        assert_eq!(spec.template, strings(&["my app"]));
        assert_eq!(display_command(&spec.template), "\"my app\"");
        // 4. Options and several unquoted words: the corrected line.
        let opts = Shape {
            options_first: true,
            separator: false,
        };
        let err = command_spec(
            opts,
            strings(&["npm", "run", "dev"]),
            &raw(&["taba", "run", "--name", "web", "npm", "run", "dev"]),
        )
        .unwrap_err();
        assert_eq!(err.code, "E_USAGE");
        assert_eq!(
            err.details.unwrap()["corrected"],
            "taba run --name web -- npm run dev"
        );
        // ... but one word is fine.
        assert!(
            command_spec(
                opts,
                strings(&["vite"]),
                &raw(&["taba", "run", "--json", "vite"])
            )
            .is_ok()
        );
    }

    #[test]
    fn suggestions_with_another_name_follow_the_rules() {
        assert_eq!(
            with_name("dev-2", &strings(&["npm", "run", "dev"])),
            "taba run --name dev-2 -- npm run dev"
        );
        let mut shell: Vec<String> = SHELL.iter().map(|s| s.to_string()).collect();
        shell.push("npm run dev && echo ok".into());
        assert_eq!(
            with_name("dev-2", &shell),
            "taba run --name dev-2 \"npm run dev && echo ok\""
        );
    }

    #[test]
    fn display_argv_quotes_only_when_needed() {
        assert_eq!(
            display_argv(&strings(&[
                "a", "--port", "{port}", "b c", "it's", "$HOME", ""
            ])),
            "a --port {port} \"b c\" \"it's\" '$HOME' \"\""
        );
    }
}
