//! Optional shell guardrail. It never runs a supplied command or signals a process.
use crate::{
    error::{Result, TabaError},
    proc, scope,
    state::Store,
    util,
};
use clap::ValueEnum;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Harness {
    ClaudeCode,
    Codex,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Event {
    PreToolUse,
    SessionStart,
    SessionEnd,
}

/// Marks a `$(...)` or backtick substitution inside a word: `SUB index SUB`.
/// Input containing it is rejected, so it can't be forged.
const SUB: char = '\u{0}';
/// Nesting limit for substitutions; deeper input is not inspected.
const MAX_DEPTH: usize = 8;
type Commands = Vec<Vec<String>>;
/// A command after `strip_wrappers` (`None` if it failed) and its assignments.
type Stripped = (Option<Vec<String>>, Vec<(String, String)>);

/// Reads a substitution body up to its closing `)` or backtick.
fn substitution(iter: &mut std::iter::Peekable<std::str::Chars>, close: char) -> Option<String> {
    let mut body = String::new();
    let mut depth = 0usize;
    let mut quote = None;
    loop {
        let c = iter.next()?;
        if close == '`' {
            match c {
                '\\' => {
                    let next = iter.next()?;
                    if next != '`' {
                        body.push('\\');
                    }
                    body.push(next);
                }
                '`' => return Some(body),
                _ => body.push(c),
            }
            continue;
        }
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('\'') => {}
            Some(_) if c == '\\' => {
                body.push(c);
                body.push(iter.next()?);
                continue;
            }
            Some(_) if c == '"' => quote = None,
            Some(_) => {}
            None => match c {
                '\\' => {
                    body.push(c);
                    body.push(iter.next()?);
                    continue;
                }
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' if depth == 0 => return Some(body),
                ')' => depth -= 1,
                _ => {}
            },
        }
        body.push(c);
    }
}

/// Splits a shell command line into simple commands without evaluating it.
/// Substitutions are parsed into `subs` and left as `SUB n SUB` in the word.
/// Returns `None` for anything it can't follow (variables, globs, subshells,
/// here-documents, unbalanced quotes).
fn words(s: &str, subs: &mut Vec<Commands>, depth: usize) -> Option<Commands> {
    if depth > MAX_DEPTH || s.contains(SUB) {
        return None;
    }
    let mut commands = Vec::new();
    let mut args = Vec::new();
    let mut word = String::new();
    let mut started = false;
    // The next word is a redirection target, not an argument.
    let mut redirect = false;
    let mut quote = None;
    let mut iter = s.chars().peekable();
    macro_rules! flush {
        () => {
            if started {
                let w = std::mem::take(&mut word);
                if !std::mem::take(&mut redirect) {
                    args.push(w);
                }
                started = false;
            }
        };
    }
    while let Some(c) = iter.next() {
        if c == '\\' && quote != Some('\'') {
            let next = iter.next()?;
            if next != '\n' {
                word.push(next);
                started = true;
            }
            continue;
        }
        let sub = match (quote, c) {
            (Some('\''), _) => None,
            (_, '`') => Some(substitution(&mut iter, '`')?),
            (_, '$') if iter.peek() == Some(&'(') => {
                iter.next();
                if iter.peek() == Some(&'(') {
                    return None;
                }
                Some(substitution(&mut iter, ')')?)
            }
            (_, '$') => return None,
            _ => None,
        };
        if let Some(body) = sub {
            let inner = words(&body, subs, depth + 1)?;
            word.push(SUB);
            word.push_str(&subs.len().to_string());
            word.push(SUB);
            subs.push(inner);
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
            started = true;
            continue;
        }
        let amp_redirect = c == '&' && iter.peek() == Some(&'>');
        if matches!(c, '>' | '<') || amp_redirect {
            if redirect || matches!(iter.peek(), Some('(') | Some('<')) {
                return None;
            }
            if amp_redirect {
                iter.next();
            } else if started && !word.is_empty() && word.chars().all(|d| d.is_ascii_digit()) {
                // A file descriptor number such as the 2 in `2>`.
                word.clear();
                started = false;
            }
            flush!();
            if matches!(iter.peek(), Some('>') | Some('|')) {
                iter.next();
            }
            if !amp_redirect && iter.peek() == Some(&'&') {
                // `>&2` and `2>&1` duplicate a descriptor; no file word follows.
                iter.next();
                while iter.peek().is_some_and(|d| d.is_ascii_digit() || *d == '-') {
                    iter.next();
                }
            } else {
                redirect = true;
            }
            continue;
        }
        if matches!(c, '(' | ')' | '*' | '?' | '[' | ']') {
            return None;
        }
        if c == '#' && !started {
            while iter.peek().is_some_and(|&ch| ch != '\n') {
                iter.next();
            }
            continue;
        }
        if c.is_whitespace() || matches!(c, ';' | '|' | '&') {
            flush!();
            if matches!(c, '\n' | ';' | '|' | '&') {
                if redirect {
                    return None;
                }
                if !args.is_empty() {
                    commands.push(std::mem::take(&mut args));
                }
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() || (redirect && !started) {
        // Unbalanced quotes, or a redirection without its target.
        return None;
    }
    if started && !redirect {
        args.push(word);
    }
    if !args.is_empty() {
        commands.push(args);
    }
    Some(commands)
}

fn program(s: &str) -> &str {
    s.rsplit(['/', '\\'])
        .next()
        .unwrap_or(s)
        .trim_end_matches(".exe")
}
fn sub_index(s: &str) -> Option<usize> {
    s.strip_prefix(SUB)?.strip_suffix(SUB)?.parse().ok()
}
fn assignment(a: &str) -> Option<(&str, &str)> {
    let (k, v) = a.split_once('=')?;
    let mut chars = k.chars();
    (chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some((k, v))
}

/// Index of the first operand after leading options. `short` lists the short
/// options that take a value, `long` the long ones. `None` if a value is missing.
fn skip_options(args: &[String], short: &str, long: &[&str]) -> Option<usize> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            return Some(i + 1);
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        i += 1;
        if let Some(name) = a.strip_prefix("--") {
            if !name.contains('=') && long.contains(&name) {
                args.get(i)?;
                i += 1;
            }
            continue;
        }
        let flags: Vec<char> = a[1..].chars().collect();
        if flags
            .iter()
            .position(|c| short.contains(*c))
            .is_some_and(|p| p + 1 == flags.len())
        {
            args.get(i)?;
            i += 1;
        }
    }
    Some(i)
}

/// Strips `NAME=value` prefixes, shell keywords and command wrappers (`sudo`,
/// `env`, `timeout N`, ...) to reach the program that actually runs. Records
/// the assignments seen. `None` if a wrapper's options can't be followed.
fn strip_wrappers<'a>(
    mut argv: &'a [String],
    assigns: &mut Vec<(String, String)>,
) -> Option<&'a [String]> {
    loop {
        while let Some((k, v)) = argv.first().and_then(|a| assignment(a)) {
            assigns.push((k.into(), v.into()));
            argv = &argv[1..];
        }
        let Some(first) = argv.first() else {
            return Some(argv);
        };
        let rest = &argv[1..];
        let skip = match program(first) {
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" => 0,
            "sudo" => skip_options(
                rest,
                "ughpCUrtDRT",
                &[
                    "user",
                    "group",
                    "host",
                    "prompt",
                    "close-from",
                    "other-user",
                    "role",
                    "type",
                    "chdir",
                    "chroot",
                    "command-timeout",
                ],
            )?,
            "doas" => skip_options(rest, "uC", &[])?,
            "command"
                if rest
                    .first()
                    .is_some_and(|a| a.starts_with('-') && a.contains(['v', 'V'])) =>
            {
                // `command -v` looks a program up; it doesn't run it.
                return Some(&[]);
            }
            "command" | "builtin" | "nohup" | "time" | "setsid" => skip_options(rest, "", &[])?,
            "exec" => skip_options(rest, "a", &[])?,
            "nice" => skip_options(rest, "n", &["adjustment"])?,
            "stdbuf" => skip_options(rest, "ioe", &["input", "output", "error"])?,
            "timeout" => {
                let i = skip_options(rest, "sk", &["signal", "kill-after"])?;
                // The duration operand precedes the command.
                (i + 1).min(rest.len())
            }
            "env" => {
                let mut i = 0;
                while let Some(a) = rest.get(i) {
                    i += 1;
                    match a.as_str() {
                        "--" => break,
                        "-" | "-i" | "--ignore-environment" => {
                            assigns.push(("TABA_AGENT".into(), String::new()))
                        }
                        "-0" | "--null" | "-v" | "--debug" => {}
                        "-u" | "--unset" | "-C" | "--chdir" => {
                            let value = rest.get(i)?;
                            i += 1;
                            if a.contains('u') {
                                assigns.push((value.clone(), String::new()));
                            }
                        }
                        s if s.starts_with("--unset=") || s.starts_with("-u") => {
                            let name = s.trim_start_matches("--unset=").trim_start_matches("-u");
                            assigns.push((name.into(), String::new()));
                        }
                        s if s.starts_with("--chdir=") || s.starts_with("-C") => {}
                        s if s.starts_with('-') => return None,
                        _ => {
                            i -= 1;
                            break;
                        }
                    }
                }
                i
            }
            _ => return Some(argv),
        };
        argv = &rest[skip..];
    }
}

/// The command `xargs` runs, after its options.
fn xargs_command(args: &[String]) -> Option<&[String]> {
    let i = skip_options(
        args,
        "ILnPsdEa",
        &[
            "arg-file",
            "delimiter",
            "max-args",
            "max-procs",
            "max-chars",
            "process-slot-var",
        ],
    )?;
    Some(&args[i..])
}

/// Ports from one `lsof -i` address (`:P`, `tcp:P`, `4TCP@host:P,Q`, `:P-Q`).
/// `None` if it doesn't name numeric ports, e.g. `-i` alone or a service name.
fn lsof_address_ports(spec: &str) -> Option<Vec<u16>> {
    let (_, list) = spec.rsplit_once(':')?;
    let mut ports = Vec::new();
    for item in list.split(',') {
        if let Some((a, b)) = item.split_once('-') {
            let (a, b): (u16, u16) = (a.parse().ok()?, b.parse().ok()?);
            if b < a || b - a > 1024 {
                return None;
            }
            ports.extend(a..=b);
        } else {
            ports.push(item.parse().ok()?);
        }
    }
    Some(ports)
}

/// What an `lsof` command lists: whether it prints only PIDs (`-t`), the
/// ports it selects, and whether those ports are all it selects.
struct Lsof {
    terse: bool,
    ports: Vec<u16>,
    complete: bool,
}

fn lsof(args: &[String]) -> Lsof {
    let mut out = Lsof {
        terse: false,
        ports: Vec::new(),
        complete: true,
    };
    let mut and = false;
    let mut other = false;
    let looks_address = |a: &String| {
        !a.starts_with('-')
            && (a.contains(':')
                || a.starts_with('@')
                || a.to_ascii_lowercase().starts_with("tcp")
                || a.to_ascii_lowercase().starts_with("udp"))
    };
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            if a.starts_with('+') {
                out.complete = false;
            } else {
                // A file name: another selection.
                other = true;
            }
            continue;
        };
        if flags.starts_with('-') {
            out.complete = false;
            continue;
        }
        let chars: Vec<char> = flags.chars().collect();
        for (j, c) in chars.iter().enumerate() {
            let rest: String = chars[j + 1..].iter().collect();
            match c {
                't' => out.terse = true,
                'a' => and = true,
                'n' | 'P' | 'l' | 'w' | 'V' | 'b' | 'U' | 'X' | 'R' | 'M' => {}
                'i' => {
                    let spec = if !rest.is_empty() {
                        rest
                    } else if args.get(i).is_some_and(looks_address) {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        String::new()
                    };
                    match lsof_address_ports(&spec) {
                        Some(p) => out.ports.extend(p),
                        None => out.complete = false,
                    }
                    break;
                }
                's' => {
                    // `-sTCP:LISTEN` narrows the selection.
                    if rest.is_empty() && args.get(i).is_some_and(|n| n.contains(':')) {
                        i += 1;
                    }
                    break;
                }
                'p' | 'u' | 'c' | 'g' | 'd' | 'D' => {
                    other = true;
                    if rest.is_empty() && *c != 'g' {
                        i += 1;
                    }
                    break;
                }
                _ => {
                    out.complete = false;
                    break;
                }
            }
        }
    }
    if other && !and {
        out.complete = false;
    }
    if out.ports.len() > 4096 {
        out.ports.clear();
        out.complete = false;
    }
    out
}

fn is_signal(a: &str) -> bool {
    let Some(s) = a.strip_prefix('-') else {
        return false;
    };
    !s.is_empty()
        && (s.chars().all(|c| c.is_ascii_digit())
            || (s.starts_with(|c: char| c.is_ascii_uppercase())
                && s.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '+')))
}

/// PIDs a `pkill`, `pgrep` or `killall` pattern selects now. `None` for
/// options or patterns this doesn't follow.
fn pattern_targets(name: &str, args: &[String]) -> Option<Vec<i32>> {
    let signals = name != "pgrep";
    let mut full = false;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        i += 1;
        match a.as_str() {
            "-f" | "--full" if name != "killall" => full = true,
            "-s" | "--signal" if signals => {
                args.get(i)?;
                i += 1;
            }
            s if signals && (s.starts_with("--signal=") || is_signal(s)) => {}
            _ => return None,
        }
    }
    let [pat] = &args[i..] else {
        return None;
    };
    if pat.contains(SUB) {
        return None;
    }
    let re = if name == "killall" {
        None
    } else {
        Some(regex::Regex::new(pat).ok()?)
    };
    let mut targets = Vec::new();
    for id in proc::all_pids() {
        if id == std::process::id() as i32 {
            continue;
        }
        let Some(a) = proc::argv(id).filter(|a| !a.is_empty()) else {
            continue;
        };
        let value = if full {
            a.join(" ")
        } else {
            program(&a[0]).into()
        };
        if re
            .as_ref()
            .map_or_else(|| value == *pat, |r| r.is_match(&value))
        {
            targets.push(id);
        }
    }
    Some(targets)
}

/// Taba subcommands that don't act on services or leases.
const READ_ONLY_TABA: [&str; 9] = [
    "whoami",
    "ps",
    "logs",
    "help",
    "hook",
    "update",
    "--help",
    "-h",
    "--version",
];

#[derive(Default)]
struct Scan {
    targets: Vec<i32>,
    ports: Vec<u16>,
    ask: Option<String>,
    unchecked: bool,
}

impl Scan {
    /// Adds the PIDs a command prints (`lsof -t`, `pgrep`), as `kill`'s
    /// operands. Marks the scan unchecked when they can't be fully known.
    fn pid_source(&mut self, argv: &[String], more: bool) {
        let Some((name, args)) = argv.split_first() else {
            self.unchecked = true;
            return;
        };
        match program(name) {
            "lsof" => {
                let l = lsof(args);
                if !l.terse || l.ports.is_empty() || !l.complete || more {
                    self.unchecked = true;
                }
                if l.terse {
                    self.ports.extend(l.ports);
                }
            }
            "pgrep" => {
                match pattern_targets("pgrep", args) {
                    Some(t) => self.targets.extend(t),
                    None => self.unchecked = true,
                }
                if more {
                    self.unchecked = true;
                }
            }
            _ => self.unchecked = true,
        }
    }

    fn kill(&mut self, args: &[String], subs: &[(Commands, Vec<Vec<String>>)]) {
        let mut i = 0;
        if let Some(a) = args
            .first()
            .filter(|a| a.starts_with('-') && a.as_str() != "--")
        {
            i = 1;
            match a.as_str() {
                "-l" | "-L" | "--list" | "--table" => return,
                "-s" | "-n" => i = 2,
                _ => {}
            }
        }
        for a in args.iter().skip(i) {
            if a == "--" {
                continue;
            }
            if let Some(n) = sub_index(a) {
                let stripped = &subs[n].1;
                match stripped.first() {
                    Some(first) => self.pid_source(first, stripped.len() > 1),
                    None => self.unchecked = true,
                }
                continue;
            }
            match a.parse::<i32>() {
                Ok(p) if p > 0 => self.targets.push(p),
                Ok(p) if p < -1 => self.targets.extend(
                    proc::all_pids()
                        .into_iter()
                        .filter(|id| proc::pgid(*id) == p.checked_neg()),
                ),
                _ => self.unchecked = true,
            }
        }
    }

    fn fuser(&mut self, args: &[String]) {
        if !args.iter().any(|a| {
            a == "--kill"
                || a.strip_prefix('-').is_some_and(|f| {
                    !f.starts_with('-')
                        && f.contains('k')
                        && f.chars().all(|c| c.is_ascii_alphabetic())
                })
        }) {
            return;
        }
        let mut found = false;
        let mut i = 0;
        while let Some(a) = args.get(i) {
            i += 1;
            if a.starts_with('-') {
                if a.ends_with('n') || a == "--namespace" {
                    i += 1;
                }
                continue;
            }
            let local = a.split([',', '/']).next().unwrap_or(a);
            match local.parse() {
                Ok(p) => {
                    self.ports.push(p);
                    found = true;
                }
                Err(_) => self.unchecked = true,
            }
        }
        if !found {
            self.unchecked = true;
        }
    }

    /// `kill-port P...` as run by `npx` and friends or directly.
    fn kill_port(&mut self, args: &[String]) {
        let mut found = false;
        let mut i = 0;
        while let Some(a) = args.get(i) {
            i += 1;
            let list = match a.as_str() {
                "-p" | "--port" => {
                    i += 1;
                    args.get(i - 1).map(String::as_str).unwrap_or("")
                }
                s if s.starts_with("--port=") => &s[7..],
                s if s.starts_with('-') => {
                    if matches!(s, "-m" | "--method") {
                        i += 1;
                    }
                    continue;
                }
                s => s,
            };
            for p in list.split(',') {
                match p.trim().parse() {
                    Ok(p) => {
                        self.ports.push(p);
                        found = true;
                    }
                    Err(_) => self.unchecked = true,
                }
            }
        }
        if !found {
            self.unchecked = true;
        }
    }

    fn pattern(&mut self, name: &str, args: &[String]) {
        match pattern_targets(name, args) {
            Some(t) => self.targets.extend(t),
            None => self.unchecked = true,
        }
    }

    /// The process-stopping commands, by program name after wrappers.
    fn command(
        &mut self,
        argv: &[String],
        prev: Option<&[String]>,
        subs: &[(Commands, Vec<Vec<String>>)],
        identity: &Option<String>,
        caller: &scope::Caller,
    ) {
        let Some((name, args)) = argv.split_first() else {
            return;
        };
        if name.contains(SUB) {
            self.unchecked = true;
            return;
        }
        match program(name) {
            "taba" => self.taba(args, identity, caller),
            "kill" => self.kill(args, subs),
            "pkill" | "killall" => self.pattern(program(name), args),
            "fuser" => self.fuser(args),
            "kill-port" => self.kill_port(args),
            "npx" | "pnpx" | "bunx" => self.package_runner(args),
            "pnpm" | "yarn" if args.first().is_some_and(|a| a == "dlx" || a == "exec") => {
                self.package_runner(&args[1..])
            }
            "xargs" => {
                let Some(cmd) = xargs_command(args) else {
                    self.unchecked = true;
                    return;
                };
                let mut assigns = Vec::new();
                let Some(cmd) = strip_wrappers(cmd, &mut assigns) else {
                    self.unchecked = true;
                    return;
                };
                match cmd.split_first().map(|(n, a)| (program(n), a)) {
                    Some(("kill", rest)) => {
                        match prev {
                            Some(p) => self.pid_source(p, false),
                            None => self.unchecked = true,
                        }
                        self.kill(rest, subs);
                    }
                    Some((
                        "pkill" | "killall" | "fuser" | "kill-port" | "npx" | "pnpx" | "bunx"
                        | "taba" | "sh" | "bash" | "zsh" | "dash" | "eval" | "env" | "xargs",
                        _,
                    )) => self.unchecked = true,
                    _ => {}
                }
            }
            "eval" | "source" | "." | "sh" | "bash" | "zsh" | "dash" | "ksh" | "pwsh"
            | "powershell" | "cmd" => self.unchecked = true,
            _ => {}
        }
    }

    fn package_runner(&mut self, args: &[String]) {
        let Some(i) = skip_options(args, "pc", &["package", "call"]) else {
            self.unchecked = true;
            return;
        };
        if args[..i].iter().any(|a| a == "-c" || a == "--call") {
            self.unchecked = true;
            return;
        }
        if args
            .get(i)
            .is_some_and(|p| p == "kill-port" || p.starts_with("kill-port@"))
        {
            self.kill_port(&args[i + 1..]);
        }
    }

    fn taba(&mut self, args: &[String], identity: &Option<String>, caller: &scope::Caller) {
        let opts: Vec<&String> = args.iter().take_while(|a| a.as_str() != "--").collect();
        let clip = |s: &str| s.chars().take(100).collect::<String>();
        let mut reasons = Vec::new();
        if opts.iter().any(|a| a.as_str() == "--force") {
            let target = opts
                .iter()
                .take_while(|a| a.as_str() != "--force")
                .filter(|a| !a.starts_with('-'))
                .take(4)
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let why = opts
                .windows(2)
                .find(|p| p[0] == "--reason")
                .map_or("not supplied", |p| p[1].as_str());
            reasons.push(format!(
                "Confirm forced taba operation ({target}); reason: {}",
                why.chars().take(200).collect::<String>()
            ));
        }
        let read_only = opts
            .first()
            .is_none_or(|s| READ_ONLY_TABA.contains(&s.as_str()));
        let flag = opts
            .windows(2)
            .find(|p| p[0] == "--agent")
            .map(|p| p[1].clone())
            .or_else(|| {
                opts.iter()
                    .find_map(|a| a.strip_prefix("--agent=").map(String::from))
            })
            .filter(|a| *a != caller.agent);
        if let Some(other) = flag.as_ref().or(identity.as_ref()).filter(|_| !read_only) {
            let sub = opts.first().map_or("", |s| s.as_str());
            reasons.push(format!(
                "Confirm taba {} as agent {} (this session is {}); acting as another agent bypasses its lease checks",
                clip(sub),
                clip(other),
                caller.agent
            ));
        }
        if !reasons.is_empty() && self.ask.is_none() {
            self.ask = Some(reasons.join("; "));
        }
    }
}

/// The agent identity a command line sets for later taba calls, when it
/// differs from the hook's caller: `TABA_AGENT=` prefixes or assignments,
/// `export`, `env -u`/`-i`, or `unset`.
fn identity_override(stripped: &[Stripped], caller: &scope::Caller) -> Option<String> {
    let effective = |v: &str| {
        if v.is_empty() {
            "human".to_string()
        } else {
            v.to_string()
        }
    };
    let mut set = Vec::new();
    for (argv, assigns) in stripped {
        set.extend(
            assigns
                .iter()
                .filter(|(k, _)| k == "TABA_AGENT")
                .map(|(_, v)| effective(v)),
        );
        let Some((name, args)) = argv.as_deref().and_then(|a| a.split_first()) else {
            continue;
        };
        match program(name) {
            "export" | "declare" | "typeset" | "readonly" | "local" => set.extend(
                args.iter()
                    .filter_map(|a| assignment(a))
                    .filter(|(k, _)| *k == "TABA_AGENT")
                    .map(|(_, v)| effective(v)),
            ),
            "unset" if args.iter().any(|a| a == "TABA_AGENT") => set.push("human".into()),
            _ => {}
        }
    }
    set.into_iter().find(|v| *v != caller.agent)
}

fn audit(store: &Store, event: &str, caller: &scope::Caller) {
    if let Ok(_lock) = store.lock() {
        let _=store.append_event(&json!({"at":util::rfc3339_now(),"event":event,"agent":caller.agent,"scope":caller.scope}));
    }
}

/// Parses a command line into the process-stopping operations it requests.
/// `None` if the line can't be parsed.
fn scan(command: &str, caller: &scope::Caller) -> Option<Scan> {
    let mut subs = Vec::new();
    let top = words(command, &mut subs, 0)?;
    let strip = |c: &Commands| -> Vec<Stripped> {
        c.iter()
            .map(|argv| {
                let mut assigns = Vec::new();
                let s = strip_wrappers(argv, &mut assigns).map(<[String]>::to_vec);
                (s, assigns)
            })
            .collect()
    };
    // Each substitution: its commands and their stripped forms.
    let subs: Vec<(Commands, Vec<Vec<String>>)> = subs
        .into_iter()
        .map(|c| {
            let s = strip(&c)
                .into_iter()
                .map(|(s, _)| s.unwrap_or_default())
                .collect();
            (c, s)
        })
        .collect();
    // Top-level commands, then each substitution's, as separate pipelines.
    let mut groups = vec![strip(&top)];
    groups.extend(subs.iter().map(|(c, _)| strip(c)));
    let all: Vec<_> = groups.iter().flatten().cloned().collect();
    let identity = identity_override(&all, caller);
    let mut out = Scan::default();
    for group in &groups {
        for (i, (argv, _)) in group.iter().enumerate() {
            let Some(argv) = argv else {
                out.unchecked = true;
                continue;
            };
            let prev = i.checked_sub(1).and_then(|p| group[p].0.as_deref());
            out.command(argv, prev, &subs, &identity, caller);
        }
    }
    Some(out)
}

fn inspect(store: &Store, caller: &scope::Caller, command: &str, is_claude: bool) -> Result<Value> {
    let Some(Scan {
        mut targets,
        mut ports,
        ask,
        unchecked,
    }) = scan(command, caller)
    else {
        audit(store, "hook_unchecked", caller);
        return Ok(json!({}));
    };
    ports.sort_unstable();
    ports.dedup();
    for port in ports {
        targets.extend(proc::listeners(port).into_iter().map(|(p, _)| p));
    }
    targets.sort_unstable();
    targets.dedup();
    if !targets.is_empty() {
        let running: Vec<_> = store
            .list(None)?
            .into_iter()
            .filter(|s| s.is_running())
            .collect();
        let listeners = proc::all_listeners();
        for pid in targets {
            let deny = if let Some(svc) = running
                .iter()
                .find(|s| proc::in_group(pid, s.pid, s.pgid, s.start_time))
            {
                let same = caller.scope.as_deref() == Some(svc.scope.as_str());
                let owner = store.get_scope(&svc.scope)?;
                let holds = owner
                    .as_ref()
                    .is_none_or(|r| r.is_held_by(caller) && r.state(util::now_secs()).is_live());
                if !same || !holds || !crate::borrows::live(store, svc)?.is_empty() {
                    Some(format!(
                        "{} is owned by scope {} and is not yours to signal. Use taba stop {} to check ownership and borrows, start with --port auto, or ask the human.",
                        svc.id(),
                        svc.scope,
                        svc.id()
                    ))
                } else {
                    None
                }
            } else {
                let worktree = proc::cwd(pid).as_deref().and_then(scope::find_worktree);
                if listeners.iter().any(|s| s.pid == pid)
                    || worktree.is_some_and(|w| Some(w) != caller.worktree)
                {
                    Some(format!(
                        "Unmanaged process {pid} is a server or belongs to another worktree. Start your own service with --port auto, or ask the human."
                    ))
                } else {
                    None
                }
            };
            if let Some(reason) = deny {
                audit(store, "hook_denied", caller);
                return Ok(permission("deny", reason));
            }
        }
    }
    if unchecked {
        audit(store, "hook_unchecked", caller);
    }
    if let Some(reason) = ask {
        if is_claude {
            return Ok(permission("ask", reason));
        }
        audit(store, "hook_denied", caller);
        return Ok(permission(
            "deny",
            format!(
                "{reason}. Codex does not support hook confirmation; ask the human to run it separately."
            ),
        ));
    }
    // No decision: defer to the normal harness permissions, never elevate.
    Ok(json!({}))
}

fn permission(decision: &str, reason: String) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":decision,"permissionDecisionReason":reason}})
}

pub fn run(harness: Harness, event: Event) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    std::io::stdin().take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err(TabaError::usage("hook input exceeds 1 MiB"));
    }
    let input: Value =
        serde_json::from_slice(&bytes).map_err(|_| TabaError::usage("invalid hook JSON"))?;
    let claude = matches!(harness, Harness::ClaudeCode);
    let id = input["session_id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 100
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
        .ok_or_else(|| TabaError::usage("invalid hook session ID"))?;
    let agent = format!("{}:{id}", if claude { "claude-code" } else { "codex" });
    if matches!(event, Event::SessionStart) && claude {
        if let Some(path) = std::env::var_os("CLAUDE_ENV_FILE") {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(
                f,
                "export TABA_AGENT={}\nexport TABA_OUTPUT=json",
                util::posix_quote(&agent)
            )?;
        }
        return Ok(None);
    }
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let cwd = util::canonicalize(Path::new(&cwd))?;
    let caller = scope::resolve_at(cwd, None, Some(agent))?;
    match event {
        Event::PreToolUse => {
            let Some(command) = input["tool_input"]["command"].as_str() else {
                return Ok(None);
            };
            Ok(Some(inspect(&Store::open()?, &caller, command, claude)?))
        }
        Event::SessionEnd if claude => {
            crate::lease::cmd_lease_end(&caller, false)?;
            Ok(None)
        }
        _ => Err(TabaError::usage(
            "this lifecycle hook is only available for Claude Code",
        )),
    }
}

#[cfg(test)]
mod tests {
    //! Parser checks on synthetic command strings; nothing is executed.
    use super::*;

    fn w(s: &str) -> Option<Commands> {
        words(s, &mut Vec::new(), 0)
    }
    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }
    fn stripped(s: &str) -> Option<Vec<String>> {
        let argv = w(s)?.remove(0);
        strip_wrappers(&argv, &mut Vec::new()).map(<[String]>::to_vec)
    }

    #[test]
    fn words_split_pipelines_and_drop_redirections() {
        assert_eq!(
            w("a -t 2>/dev/null | b -r c >> out.log 2>&1; d &>x").unwrap(),
            vec![v(&["a", "-t"]), v(&["b", "-r", "c"]), v(&["d"])]
        );
        for s in [
            "a $X", "a <<EOF", "a <(b)", "a >", "a *", "(a)", "a $((1))", "a \0",
        ] {
            assert!(w(s).is_none(), "{s:?}");
        }
    }

    #[test]
    fn substitutions_are_parsed_not_evaluated() {
        let mut subs = Vec::new();
        let top = words("a \"$(b -x 'c)')\" `d e`", &mut subs, 0).unwrap();
        assert_eq!(top[0][0], "a");
        assert_eq!(sub_index(&top[0][1]), Some(0));
        assert_eq!(sub_index(&top[0][2]), Some(1));
        assert_eq!(
            subs,
            vec![vec![v(&["b", "-x", "c)"])], vec![v(&["d", "e"])]]
        );
        let deep = format!("a {}b{}", "$(".repeat(20), ")".repeat(20));
        assert!(w(&deep).is_none());
    }

    #[test]
    fn wrappers_and_assignments_are_stripped() {
        for input in [
            "sudo -u root -E prog 1",
            "doas prog 1",
            "command prog 1",
            "env -u X A=1 prog 1",
            "exec -a name prog 1",
            "nohup nice -n 5 prog 1",
            "timeout -s KILL 5 prog 1",
            "timeout 5 sudo prog 1",
            "A=1 B=2 prog 1",
            "then prog 1",
        ] {
            assert_eq!(stripped(input).unwrap(), v(&["prog", "1"]), "{input}");
        }
        assert_eq!(stripped("command -v prog").unwrap(), Vec::<String>::new());
        assert!(stripped("env -S 'prog 1'").is_none());
        assert!(stripped("sudo -u").is_none());
        let mut assigns = Vec::new();
        strip_wrappers(&v(&["env", "-i", "A=1", "prog"]), &mut assigns).unwrap();
        assert_eq!(
            assigns,
            vec![
                ("TABA_AGENT".into(), String::new()),
                ("A".into(), "1".into())
            ]
        );
    }

    #[test]
    fn xargs_options_are_skipped() {
        for s in [
            "xargs prog",
            "xargs -r prog",
            "xargs -n 1 -P4 prog",
            "xargs --max-args 1 -0 prog",
        ] {
            let argv = w(s).unwrap().remove(0);
            assert_eq!(xargs_command(&argv[1..]).unwrap()[0], "prog", "{s}");
        }
    }

    #[test]
    fn lsof_port_selections() {
        let l = |s: &str| lsof(&w(s).unwrap().remove(0)[1..]);
        for (input, ports) in [
            ("lsof -ti:8000", vec![8000]),
            ("lsof -t -i :8000", vec![8000]),
            ("lsof -ti tcp:8000", vec![8000]),
            ("lsof -t -iTCP:8000 -sTCP:LISTEN -nP", vec![8000]),
            ("lsof -ti:8000,4100", vec![8000, 4100]),
            ("lsof -t -i4TCP@127.0.0.1:8000-8002", vec![8000, 8001, 8002]),
            ("lsof -t -i :8000 -a -p 1", vec![8000]),
        ] {
            let got = l(input);
            assert!(got.terse && got.complete, "{input}");
            assert_eq!(got.ports, ports, "{input}");
        }
        for input in [
            "lsof -ti",
            "lsof -t -i :http",
            "lsof -t -i :8000 -p 1",
            "lsof -t file",
        ] {
            assert!(!l(input).complete, "{input}");
        }
        assert!(!l("lsof -i:8000").terse);
    }

    #[test]
    fn signals_are_recognised() {
        for s in ["-9", "-KILL", "-SIGTERM", "-HUP"] {
            assert!(is_signal(s), "{s}");
        }
        for s in ["-f", "-x", "9", "-"] {
            assert!(!is_signal(s), "{s}");
        }
    }

    #[test]
    fn identity_overrides_differ_from_the_caller() {
        let caller = scope::Caller {
            scope: None,
            source: None,
            worktree: None,
            branch: None,
            agent: "claude-code:a".into(),
            agent_pid: None,
            cwd: PathBuf::new(),
        };
        let check = |s: &str| {
            let all: Vec<_> = w(s)
                .unwrap()
                .iter()
                .map(|argv| {
                    let mut assigns = Vec::new();
                    let s = strip_wrappers(argv, &mut assigns).map(<[String]>::to_vec);
                    (s, assigns)
                })
                .collect();
            identity_override(&all, &caller)
        };
        assert_eq!(check("TABA_AGENT=claude-code:a prog"), None);
        assert_eq!(check("prog"), None);
        assert_eq!(check("TABA_AGENT=b prog").as_deref(), Some("b"));
        assert_eq!(check("export TABA_AGENT=b; prog").as_deref(), Some("b"));
        assert_eq!(check("unset TABA_AGENT; prog").as_deref(), Some("human"));
        assert_eq!(check("env -i prog").as_deref(), Some("human"));
    }
}
