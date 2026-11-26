//! taba: start, list, inspect and stop local processes, grouped by work scope.

mod borrows;
mod claims;
mod config;
mod error;
mod gc;
mod handoff;
mod hooks;
mod lease;
mod listing;
mod ports;
mod proc;
mod project;
mod ready;
mod runform;
mod scope;
mod state;
mod ui;
mod update;
mod util;
mod watch;

use clap::{Parser, Subcommand};
use error::{Next, Output, Result, TabaError, Warning, emit};
use ports::{PortSpec, RunRequest};
use ready::{Outcome, ReadySpec};
use scope::{Caller, ScopeSource, check_ident};
use serde_json::{Value, json};
use state::{OpLock, Service, Store};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;
use util::{local_timestamp, log_tail, tilde};

#[derive(Parser)]
#[command(
    name = "taba",
    version,
    about = "Start, list and stop local processes, bundled by work scope"
)]
struct Cli {
    /// Without a command, taba lists services (as `taba ps`)
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a command detached, owned by SCOPE/NAME.
    ///
    /// taba run [taba flags] <command...>: taba's flags come first;
    /// everything from the first word that is not a taba flag belongs to the
    /// command, its own options and `--` included. In
    /// `taba run npm run dev -- --host`, npm gets `run dev -- --host`.
    ///
    /// Without --name, the name comes from the command (`npm run dev` →
    /// dev), and run waits up to 30 s for the first port the service
    /// listens on and prints its URL.
    Run {
        /// Work scope that owns the service (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Service name, unique within the scope. Without it, the name comes
        /// from the command: `npm|pnpm|yarn|bun run X` and `pnpm X` give X,
        /// `flutter run -d D` gives flutter-D, anything else the program name
        #[arg(long)]
        name: Option<String>,
        /// Port: a number, `auto` (a free port in 4100-4999) or `auto:START`.
        /// taba replaces `{port}` in the command and sets PORT
        #[arg(long, value_parser = ports::parse_port_spec, value_name = "PORT|auto|auto:START")]
        port: Option<PortSpec>,
        /// Block until the service is ready (default 30 seconds), then report
        #[arg(long, value_name = "SECS", num_args = 0..=1, require_equals = true,
              default_missing_value = "30")]
        wait: Option<f64>,
        /// Ready check for --wait: tcp (default with a port), http:PATH or log:REGEX
        #[arg(long, value_parser = ready::parse_ready_spec, requires = "wait",
              value_name = "tcp|http:PATH|log:REGEX")]
        ready: Option<ReadySpec>,
        /// Lease TTL for the caller's scope: a duration such as 4h, or none
        /// (default: $TABA_TTL, then lease_ttl in config.toml, then 4h for agents)
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        /// Stop this service once the duration has passed (renewed by an
        /// already_running re-run), independent of the lease
        #[arg(long, value_parser = config::parse_service_ttl_flag)]
        service_ttl: Option<u64>,
        /// Set an environment variable for the service (repeatable). Values
        /// are never stored in taba state or printed.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Write this service to taba.toml after it starts.
        #[arg(long)]
        save: bool,
        /// With --save, replace a different saved definition.
        #[arg(long, requires = "save")]
        force: bool,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
        /// The command: everything from the first word that is not a taba
        /// flag, passed on unchanged (`--` before it is optional)
        #[arg(required = true, trailing_var_arg = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Start the services declared in the nearest taba.toml (all, or NAME...
    /// in that order) and wait until each is ready; running ones are reused
    Up {
        /// Services to start, in this order (default: all, in file order)
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Work scope that owns the services (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Lease TTL for the caller's scope: a duration such as 4h, or none
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Stop the caller's services declared in the nearest taba.toml (all, or
    /// NAME...)
    Down {
        /// Services to stop (default: all declared in taba.toml)
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// The caller's scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Seconds to wait after SIGTERM (Windows: Ctrl+Break) before SIGKILL
        /// (Windows: TerminateJobObject)
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Show the caller's scope, where it came from, and its services
    Whoami {
        /// Scope to report (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// List services
    Ps {
        /// Only show this scope
        #[arg(long, conflicts_with = "all")]
        scope: Option<String>,
        /// Only show running services (exited ones are listed by default)
        #[arg(long, conflicts_with = "all")]
        running: bool,
        /// List every listening TCP port of this user, managed by taba or not
        /// (read-only)
        #[arg(long)]
        all: bool,
        /// Print JSON instead of a table (paths are absolute; also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
        /// Keep running and print a full `--json` snapshot, one per line
        /// (NDJSON), first at once and then whenever it changes (needs --json)
        #[arg(long, conflicts_with = "all")]
        watch: bool,
        /// How often --watch re-reads state: SECS, or a number with ms or s
        /// (default 1s, minimum 250ms)
        #[arg(long, requires = "watch", value_parser = watch::parse_interval,
              value_name = "DURATION")]
        interval: Option<Duration>,
    },
    /// Browse every scope's services in a terminal UI; stop, remove,
    /// follow logs and filter with the keyboard
    Ui,
    /// Print a service's log
    Logs {
        /// SCOPE/NAME, or NAME with $TABA_SCOPE or the git worktree's scope
        target: String,
        /// Keep printing new output until the service exits
        #[arg(short, long, conflicts_with = "json")]
        follow: bool,
        /// Show only the last N existing lines (0 starts at the end with --follow)
        #[arg(long, value_name = "N")]
        tail: Option<u64>,
        /// Print JSON with up to 16 KiB of log data (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Stop a service (SCOPE/NAME) or every service in a scope (--scope)
    Stop {
        /// SCOPE/NAME, or NAME in the caller's scope
        target: Option<String>,
        /// The caller's scope; without a target, stop all its services
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Seconds to wait after SIGTERM (Windows: Ctrl+Break) before SIGKILL
        /// (Windows: TerminateJobObject)
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
        /// Stop a service of another scope (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why --force is needed (recorded in events.jsonl)
        #[arg(long, requires = "force")]
        reason: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Remove stopped services' records and logs (SCOPE/NAME, or --scope)
    Rm {
        /// SCOPE/NAME, or NAME in the caller's scope
        target: Option<String>,
        /// The caller's scope; without a target, clean up all its stopped services
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Remove a service of another scope (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why --force is needed (recorded in events.jsonl)
        #[arg(long, requires = "force")]
        reason: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Renew the current scope's lease
    Renew {
        /// The caller's scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// New lease TTL: a duration such as 4h, or none
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Manage leases
    Lease {
        #[command(subcommand)]
        command: LeaseCmd,
    },
    /// Keep the scope's services after its lease ends
    Keep {
        /// The caller's scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Remove the keep mark again
        #[arg(long)]
        off: bool,
        /// Set the mark without the lease check (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why the scope is kept (required for agents; shown in ps and recorded)
        #[arg(long)]
        reason: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Collect services past their lease or TTL
    Gc {
        /// Show what would be handed over or dropped; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Record a shared or exclusive borrow of another scope's service
    Borrow {
        /// SCOPE/NAME to borrow, or with --check a borrow ID
        target: String,
        /// The borrowing scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Check that a borrow still points at a running instance
        #[arg(long, conflicts_with_all = ["exclusive", "ttl", "wait_lock", "command"])]
        check: bool,
        /// Take the exclusive borrow (&mut), which allows stop and re-run of
        /// that service
        #[arg(long = "mut")]
        exclusive: bool,
        /// How long the borrow lasts (default: the borrowing scope's lease)
        #[arg(long, value_parser = config::parse_ttl_flag)]
        ttl: Option<config::Ttl>,
        /// Seconds to wait for another exclusive borrow to end
        #[arg(long, default_value_t = 0.0)]
        wait_lock: f64,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
        /// Command to run while holding an exclusive borrow, after `--`;
        /// the borrow ends when it exits
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// End a borrow by ID or service
    Release {
        /// Borrow ID or the borrowed SCOPE/NAME
        target: String,
        /// The borrowing scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Transfer a scope lease or move a service without restarting it
    Move {
        /// SCOPE/NAME to move one service; without it, move the scope lease
        target: Option<String>,
        /// The caller's scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Receiving agent (scope lease) or receiving scope (one service)
        #[arg(long, conflicts_with = "take", required_unless_present = "take")]
        to: Option<String>,
        /// Take the scope lease; its holder's lease must have ended, or --force
        #[arg(long, conflicts_with = "target")]
        take: bool,
        /// Override the lease check (needs --reason; recorded)
        #[arg(long)]
        force: bool,
        /// Why --force is needed (recorded in events.jsonl)
        #[arg(long, requires = "force")]
        reason: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Queue for ownership of another scope's service
    Claim {
        /// SCOPE/NAME of another scope's service
        target: String,
        /// The claiming scope (default: $TABA_SCOPE, else the git worktree)
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Leave the claim queue instead of joining it
        #[arg(long)]
        cancel: bool,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Hand a service to its first live claimant
    Disown {
        /// SCOPE/NAME, or NAME in the caller's scope; without it, use --scope
        target: Option<String>,
        /// The caller's scope; without a target, disown all its services
        #[arg(long)]
        scope: Option<String>,
        /// Agent acting (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Check for or install a newer stable release from official GitHub Releases
    Update {
        /// Only report whether a newer stable release exists; change nothing
        #[arg(long)]
        check: bool,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
    /// Inspect agent hook input on stdin
    Hook {
        /// The agent harness whose hook input is read
        harness: hooks::Harness,
        /// The hook event being handled
        event: hooks::Event,
    },
    /// Internal (Windows): send Ctrl+Break to console process group PGID,
    /// attaching to the console of process ATTACH
    #[cfg(windows)]
    #[command(name = "__ctrl-break", hide = true)]
    CtrlBreak { attach: u32, pgid: u32 },
}

#[derive(Subcommand)]
enum LeaseCmd {
    /// End leases without immediately stopping services
    End {
        /// End only the agent's lease of this scope (default: all its leases)
        #[arg(long, conflicts_with = "agent")]
        scope: Option<String>,
        /// Agent whose leases end (default: $TABA_AGENT, else "human")
        #[arg(long)]
        agent: Option<String>,
        /// Print one JSON document (also: TABA_OUTPUT=json)
        #[arg(long)]
        json: bool,
    },
}

fn mutating_cmd(
    f: impl FnOnce(&Store, &config::Config) -> Result<Output>,
) -> Result<Option<Output>> {
    let store = Store::open()?;
    let cfg = config::Config::load(store.root())?;
    f(&store, &cfg).map(Some)
}

fn env_json() -> bool {
    std::env::var("TABA_OUTPUT").is_ok_and(|v| v == "json")
}

/// Whether JSON output was asked for, judged from the raw arguments (used
/// when clap itself rejects them).
fn json_requested_raw() -> bool {
    env_json()
        || std::env::args()
            .skip(1)
            .take_while(|a| a != "--")
            .any(|a| a == "--json")
}

/// The subcommand path a raw argument list names, such as `taba lease end`;
/// `taba` when it names none.
fn command_path(args: &[String]) -> String {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let mut path = vec!["taba".to_string()];
    for a in args.iter().take_while(|a| a.as_str() != "--") {
        if a.starts_with('-') {
            continue;
        }
        let Some(sub) = cmd.find_subcommand(a).filter(|s| !s.is_hide_set()).cloned() else {
            break;
        };
        path.push(sub.get_name().to_string());
        cmd = sub;
        if !cmd.has_subcommands() {
            break;
        }
    }
    path.join(" ")
}

/// A clap parse error as a complete `E_USAGE`: the whole message (not just
/// its first line), the offending arguments and usage in `details`, and the
/// help of the actual subcommand in `next`.
fn usage_error(e: &clap::Error, args: &[String]) -> TabaError {
    use clap::error::{ContextKind, ContextValue};
    let rendered = e.render().to_string();
    let message = rendered
        .lines()
        .take_while(|l| !l.starts_with("Usage:"))
        .map(|l| l.trim().trim_start_matches("error:").trim())
        .filter(|l| !l.is_empty() && !l.starts_with("For more information"))
        .fold(String::new(), |acc, l| {
            match (acc.is_empty(), l.starts_with("tip:")) {
                (true, _) => l.to_string(),
                (false, true) => format!("{acc}; {l}"),
                (false, false) => format!("{acc} {l}"),
            }
        });
    let message = if message.is_empty() {
        "invalid arguments".to_string()
    } else {
        message
    };
    let strings = |kind| match e.get(kind) {
        Some(ContextValue::String(s)) => vec![s.clone()],
        Some(ContextValue::Strings(v)) => v.clone(),
        _ => Vec::new(),
    };
    let mut details = serde_json::Map::new();
    let invalid = strings(ContextKind::InvalidArg);
    if !invalid.is_empty() {
        details.insert("arguments".into(), json!(invalid));
    }
    if let Some(value) = strings(ContextKind::InvalidValue)
        .first()
        .filter(|v| !v.is_empty())
    {
        details.insert("value".into(), json!(value));
    }
    if let Some(sub) = strings(ContextKind::InvalidSubcommand).first() {
        details.insert("subcommand".into(), json!(sub));
    }
    if let Some(usage) = rendered.lines().find_map(|l| l.strip_prefix("Usage:")) {
        // The binary may be `taba.exe` or a path; name it as the docs do.
        let rest = usage.trim().split_once(' ').map_or("", |(_, r)| r);
        details.insert("usage".into(), json!(format!("taba {rest}").trim_end()));
    }
    let help = format!("{} --help", command_path(args));
    let mut err = TabaError::usage(message).next(Next::run("see the usage", help));
    if !details.is_empty() {
        err = err.details(Value::Object(details));
    }
    err
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    let (args, shape) = match runform::prepare(raw.clone()) {
        Ok(prepared) => prepared,
        Err(e) => return ExitCode::from(emit(json_requested_raw(), Err(e))),
    };
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            let informational = matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            if informational || !json_requested_raw() {
                e.exit();
            }
            let args: Vec<String> = std::env::args().skip(1).collect();
            return ExitCode::from(emit(true, Err(usage_error(&e, &args))));
        }
    };

    let Some(cmd) = cli.cmd else {
        // `taba` alone lists services, as `taba ps` does.
        let json = env_json();
        return ExitCode::from(emit(json, cmd_ps(None, false)));
    };
    let (json_mode, res): (bool, Result<Option<Output>>) = match cmd {
        #[cfg(windows)]
        Cmd::CtrlBreak { attach, pgid } => {
            return ExitCode::from(proc::ctrl_break_helper(attach, pgid));
        }
        Cmd::Run {
            scope,
            agent,
            name,
            port,
            wait,
            ready,
            ttl,
            service_ttl,
            env,
            save,
            force,
            json,
            command,
        } => {
            let json = json || env_json();
            let interactive = !json
                && std::env::var_os("TABA_AGENT").is_none_or(|a| a.is_empty())
                && io::stdin().is_terminal()
                && io::stderr().is_terminal();
            (
                json,
                project::parse_env_args(&env)
                    .and_then(|env| {
                        cmd_run(RunArgs {
                            scope,
                            agent,
                            name,
                            port,
                            wait,
                            ready,
                            ttl,
                            service_ttl,
                            command,
                            cwd: None,
                            env,
                            save,
                            force,
                            shape,
                            raw,
                            interactive,
                        })
                    })
                    .map(Some),
            )
        }
        Cmd::Up {
            names,
            scope,
            agent,
            ttl,
            json,
        } => (
            json || env_json(),
            cmd_up(names, scope, agent, ttl).map(Some),
        ),
        Cmd::Down {
            names,
            scope,
            agent,
            timeout,
            json,
        } => (
            json || env_json(),
            cmd_down(names, scope, agent, timeout).map(Some),
        ),
        Cmd::Renew {
            scope,
            agent,
            ttl,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|_, cfg| lease::cmd_renew(&scope::resolve(scope, agent)?, cfg, ttl)),
        ),
        Cmd::Lease {
            command: LeaseCmd::End { scope, agent, json },
        } => {
            let bulk = scope.is_some();
            (
                json || env_json(),
                scope::resolve(scope, agent)
                    .and_then(|c| lease::cmd_lease_end(&c, bulk))
                    .map(Some),
            )
        }
        Cmd::Keep {
            scope,
            agent,
            off,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|_, cfg| {
                lease::cmd_keep(&scope::resolve(scope, agent)?, cfg, off, reason, force)
            }),
        ),
        Cmd::Gc { dry_run, json } => (
            json || env_json(),
            mutating_cmd(|store, cfg| gc::collect(store, cfg, dry_run, usize::MAX, None)),
        ),
        Cmd::Borrow {
            target,
            scope,
            agent,
            check,
            exclusive,
            ttl,
            wait_lock,
            json,
            command,
        } => {
            let json = json || env_json();
            (
                json,
                mutating_cmd(|store, cfg| {
                    borrows::cmd(
                        store,
                        cfg,
                        &scope::resolve(scope, agent)?,
                        borrows::Request {
                            target,
                            check,
                            exclusive,
                            ttl,
                            wait: wait_lock,
                            command,
                            json,
                        },
                    )
                }),
            )
        }
        Cmd::Release {
            target,
            scope,
            agent,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, _| {
                borrows::release(store, &scope::resolve(scope, agent)?, &target)
            }),
        ),
        Cmd::Update { check, json } => {
            let json = json || env_json();
            (json, update::command(check, json).map(Some))
        }
        Cmd::Hook { harness, event } => {
            return match hooks::run(harness, event) {
                Ok(value) => {
                    if let Some(value) = value {
                        error::print_json(&value);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("taba hook: {}", e.message);
                    ExitCode::from(1)
                }
            };
        }
        Cmd::Move {
            target,
            scope,
            agent,
            to,
            take,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, cfg| {
                handoff::execute(
                    store,
                    cfg,
                    &scope::resolve(scope, agent)?,
                    handoff::Request {
                        target,
                        to,
                        take,
                        force,
                        reason,
                    },
                )
            }),
        ),
        Cmd::Claim {
            target,
            scope,
            agent,
            cancel,
            json,
        } => (
            json || env_json(),
            mutating_cmd(|store, cfg| {
                claims::claim(store, cfg, &scope::resolve(scope, agent)?, &target, cancel)
            }),
        ),
        Cmd::Disown {
            target,
            scope,
            agent,
            json,
        } => {
            let bulk = scope.is_some();
            (
                json || env_json(),
                mutating_cmd(|store, cfg| {
                    claims::disown(store, cfg, &scope::resolve(scope, agent)?, target, bulk)
                }),
            )
        }
        Cmd::Ui => (env_json(), ui::command()),
        Cmd::Whoami { scope, agent, json } => {
            (json || env_json(), cmd_whoami(scope, agent).map(Some))
        }
        Cmd::Ps {
            scope,
            running,
            all,
            json,
            watch,
            interval,
        } => {
            let json = json || env_json();
            if watch {
                return ExitCode::from(watch::run(json, scope, running, interval));
            }
            let res = if all {
                listing::cmd_ps_all()
            } else {
                cmd_ps(scope, running)
            };
            (json, res.map(Some))
        }
        Cmd::Logs {
            target,
            follow,
            tail,
            json,
        } => {
            // `-f` streams raw text; TABA_OUTPUT=json does not apply to it.
            let json = json || (env_json() && !follow);
            (json, cmd_logs(&target, follow, tail, json))
        }
        Cmd::Stop {
            target,
            scope,
            agent,
            timeout,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            cmd_stop(target, scope, agent, timeout, force, reason).map(Some),
        ),
        Cmd::Rm {
            target,
            scope,
            agent,
            force,
            reason,
            json,
        } => (
            json || env_json(),
            cmd_rm(target, scope, agent, force, reason).map(Some),
        ),
    };
    let code = match res {
        Ok(None) => 0,
        Ok(Some(out)) => emit(json_mode, Ok(out)),
        Err(e) => emit(json_mode, Err(e)),
    };
    ExitCode::from(code)
}

/// Parses `SCOPE/NAME`, or `NAME` in the caller's scope.
fn resolve_target(target: &str, caller: &Caller) -> Result<(String, String)> {
    let (scope, name) = match target.split_once('/') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => (caller.require_scope()?, target.to_string()),
    };
    check_ident("scope", &scope)?;
    check_ident("name", &name)?;
    Ok((scope, name))
}

fn not_found(scope: &str, name: &str) -> TabaError {
    TabaError::new("E_NOT_FOUND", format!("no such service: {scope}/{name}"))
        .details(json!({ "service": format!("{scope}/{name}") }))
        .next(Next::run("list services", "taba ps --json"))
}

/// `--force` needs a non-empty `--reason`.
fn check_force(force: bool, reason: Option<String>) -> Result<Option<String>> {
    if !force {
        return Ok(None);
    }
    match reason.map(|r| r.trim().to_string()) {
        Some(r) if !r.is_empty() => Ok(Some(r)),
        _ => Err(TabaError::new(
            "E_FORCE_NEEDS_REASON",
            "--force needs --reason TEXT; the reason is recorded in events.jsonl",
        )),
    }
}

fn busy(svc_id: &str, op: &OpLock) -> TabaError {
    TabaError::new(
        "E_BUSY",
        format!(
            "{svc_id} is busy: another taba `{}` (pid {}, agent {}) is in progress; \
             retry when it has finished",
            op.op, op.pid, op.agent
        ),
    )
    .details(json!({
        "service": svc_id, "op": op.op, "pid": op.pid,
        "agent": op.agent, "started_at": op.started_at,
    }))
    .next(Next::run(
        "wait for the other operation, then check the service",
        format!("taba ps --scope {}", svc_id.split('/').next().unwrap_or("")),
    ))
}

/// The caller may mutate `svc` only if it is in the caller's own scope.
fn not_owner(caller: &Caller, svc: &Service, verb: &str) -> TabaError {
    let you = caller
        .scope
        .as_deref()
        .map_or("you have no scope".to_string(), |s| {
            format!("you are in {s}")
        });
    let mut err = TabaError::new(
        "E_NOT_OWNER",
        format!(
            "{} belongs to scope {} ({}), and {you}; taba does not {verb} another scope's \
             service. A human can override this with: taba {verb} {} --force --reason TEXT",
            svc.id(),
            svc.scope,
            tilde(&svc.cwd),
            svc.id()
        ),
    )
    .details(json!({
        "service": svc.id(),
        "owner_scope": svc.scope,
        "directory": svc.cwd,
        "caller_scope": caller.scope,
        "agent": caller.agent,
        "url": svc.url(),
    }));
    if let Some(url) = svc.url() {
        err = err.next(Next::just(&format!(
            "use the server read-only at {url}; do not stop it"
        )));
    }
    err.next(Next::just(&format!(
        "ask whoever works in {} to {verb} it",
        tilde(&svc.cwd)
    )))
    .next(Next::just("ask the human"))
}

/// Records a `--force` override in events.jsonl. Call under the global lock.
fn record_force(
    store: &Store,
    event: &str,
    caller: &Caller,
    svc: &Service,
    reason: &str,
    overrode: &[&str],
) -> Result<()> {
    store.append_event(&json!({
        "at": util::rfc3339_now(),
        "event": event,
        "agent": caller.agent,
        "caller_scope": caller.scope,
        "cwd": caller.cwd,
        "target": svc.id(),
        "reason": reason,
        "instance": svc.instance(),
        "overrode": overrode,
    }))?;
    Ok(())
}

/// Short multi-line summary printed by `run`, e.g.
///
/// ```text
/// started login-fix/web → http://localhost:5173
///   command: npm run dev -- --port 5173
///   dir  ~/dev/app-login
///   logs taba logs login-fix/web
/// ```
fn describe(verb: &str, svc: &Service) -> String {
    let head = match svc.url() {
        Some(url) => format!("{verb} {} → {url}", svc.id()),
        None => format!("{verb} {}", svc.id()),
    };
    format!(
        "{head}\n  command: {}\n  dir  {}\n  logs taba logs {}",
        runform::display_command(svc.template()),
        tilde(&svc.cwd),
        svc.id()
    )
}

// ---------------------------------------------------------------- run

struct RunArgs {
    scope: Option<String>,
    agent: Option<String>,
    /// `None` for the short form: see [`runform`].
    name: Option<String>,
    port: Option<PortSpec>,
    wait: Option<f64>,
    ready: Option<ReadySpec>,
    ttl: Option<config::Ttl>,
    service_ttl: Option<u64>,
    /// The command as clap collected it; [`runform::command_spec`] turns it
    /// into the argv to start.
    command: Vec<String>,
    /// Working directory (default: the caller's current directory).
    cwd: Option<std::path::PathBuf>,
    /// Extra environment variables for the command.
    env: Vec<(String, String)>,
    save: bool,
    force: bool,
    shape: runform::Shape,
    /// The original arguments, program name first (for corrections).
    raw: Vec<String>,
    /// A person at a terminal: a correctable command is offered to run.
    interactive: bool,
}

fn cmd_run(mut a: RunArgs) -> Result<Output> {
    let command = std::mem::take(&mut a.command);
    let spec = match runform::command_spec(a.shape, command.clone(), &a.raw) {
        Ok(spec) => spec,
        Err(e) => {
            let fixed = e
                .details
                .as_ref()
                .and_then(|d| d["corrected"].as_str())
                .map(String::from);
            match fixed {
                Some(fixed) if a.interactive && confirm_corrected(&fixed) => {
                    let shape = runform::Shape {
                        separator: true,
                        ..a.shape
                    };
                    runform::command_spec(shape, command, &a.raw)?
                }
                _ => return Err(e),
            }
        }
    };
    a.command = spec.template;
    let form = runform::resolve(a.name.clone(), a.wait, a.ready.is_some(), &spec.words)?;
    let plan = if a.save {
        let caller = scope::resolve(a.scope.clone(), a.agent.clone())?;
        let cwd = a.cwd.clone().unwrap_or_else(|| caller.cwd.clone());
        Some(project::plan_save(
            project::save_path(&caller.cwd, caller.worktree.as_deref()),
            project::SaveRequest {
                name: form.name.clone(),
                command: a.command.clone(),
                port: a.port.clone(),
                ready: a.ready.clone(),
                wait: a.wait,
                env: a.env.clone(),
                cwd,
                service_ttl: a.service_ttl,
            },
            a.force,
        )?)
    } else {
        None
    };
    let mut out = run_service(a, form)?;
    if let Some(plan) = plan {
        let plan = project::write_save(&plan)?;
        let shown = project::render(
            &plan.def,
            plan.path.parent().unwrap_or(std::path::Path::new(".")),
            true,
        );
        out.body.insert("saved".into(), json!({"path": plan.path, "name": plan.def.name, "result": plan.result, "definition": shown}));
        if !plan.def.env.is_empty() && plan.result != "unchanged" {
            out.warnings.push(Warning { code: "W_ENV_SAVED", message: "environment values were saved to taba.toml in plain text; do not commit secrets".into() });
        }
    }
    Ok(out)
}

/// Offers the corrected command line on the terminal; Enter or `y` runs it.
fn confirm_corrected(fixed: &str) -> bool {
    eprint!("taba: with taba options, put -- before the command:\n  {fixed}\nRun it? [Y/n] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

/// How a short-form run reports itself (see [`runform`]).
struct Short {
    derived: runform::Derived,
    /// How to refer to the service in the printed hints: its name, or
    /// SCOPE/NAME when the scope came from `--scope`.
    label: String,
    /// It waited for a listening port rather than a ready check.
    port_wait: bool,
}

/// Starts (or reuses) service `form.name` of the caller's scope. `form`
/// carries the short form's name and wait (see [`runform`]); the explicit
/// form passes `a`'s flags through as they are.
fn run_service(a: RunArgs, form: runform::RunForm) -> Result<Output> {
    let name = form.name;
    let caller = scope::resolve(a.scope.clone(), a.agent.clone())?;
    let scope = caller.require_scope()?;
    check_ident("name", &name)?;
    let dir = a.cwd.clone().unwrap_or_else(|| caller.cwd.clone());
    let mut env_keys: Vec<String> = a.env.iter().map(|(k, _)| k.clone()).collect();
    env_keys.sort();
    if let Some(dupe) = env_keys.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(TabaError::usage(format!(
            "--env {} is given twice",
            dupe[0]
        )));
    }
    let short = form.derived.map(|derived| Short {
        derived,
        label: if caller.source == Some(ScopeSource::Flag) {
            format!("{scope}/{name}")
        } else {
            name.clone()
        },
        port_wait: form.wait_for_port,
    });
    let short = short.as_ref();
    let wait = match form.wait {
        None => None,
        Some(s) if s.is_finite() && s > 0.0 => Some(Duration::from_secs_f64(s)),
        Some(_) => {
            return Err(TabaError::usage(
                "--wait takes a positive number of seconds",
            ));
        }
    };
    if let Some(r) = &a.ready
        && r.needs_port()
        && a.port.is_none()
    {
        return Err(TabaError::usage(format!(
            "--ready {} needs --port (use log:REGEX for a service without a port)",
            r.name()
        )));
    }
    // The ready check: --ready, else tcp when there is a port. Without
    // either, --wait keeps v0.1's "still alive after 200 ms" check. The
    // short form instead waits for any listening port.
    let check = wait.filter(|_| !form.wait_for_port).and_then(|_| {
        a.ready
            .clone()
            .or_else(|| a.port.as_ref().map(|_| ReadySpec::Tcp))
    });
    let template = a.command.clone();
    let port_request = a.port.as_ref().map(PortSpec::request);
    let scope_flag = (caller.source == Some(ScopeSource::Flag)).then_some(scope.as_str());
    let req = RunRequest {
        scope_flag,
        name: &name,
        template: &template,
    };

    let store = Store::open()?;
    let cfg = config::Config::load(store.root())?;
    cfg.resolve_ttl(a.ttl, caller.is_human())?;
    gc::collect(&store, &cfg, false, 5, Some(&scope))?;
    let lock = store.lock()?;
    store.mark_version()?;
    let env_hash = store.env_fingerprint(&a.env)?;
    let existing = store.get(&scope, &name)?;
    if let Some(short) = short
        && let Some(ex) = &existing
        && let Some(err) = runform::derived_name_clash(ex, &short.derived, &template, &dir)
    {
        return Err(err);
    }
    let mut delegation = None;
    if a.scope.is_some()
        && a.ttl.is_none()
        && let Some(ex) = &existing
        && !ex.is_running()
    {
        let borrower = scope::resolve(None, a.agent.clone())?;
        if borrower.scope.as_deref() != Some(scope.as_str()) {
            delegation = borrows::live(&store, ex)?
                .into_iter()
                .find(|b| b.exclusive && b.owned(&borrower));
        }
    }
    let touched = if delegation.is_some() {
        lease::Touched::default()
    } else {
        lease::touch(&store, &cfg, &caller, &scope, lease::Access::Create, a.ttl)?
    };
    let id = format!("{scope}/{name}");
    if let Some(op) = store.live_op(&scope, &name)? {
        return Err(busy(&id, &op));
    }
    store.clear_stale_op(&scope, &name)?;

    if let Some(ex) = &existing
        && !ex.is_running()
        && delegation.is_none()
    {
        borrows::authorise(&store, ex, &caller, false, false)?;
    }
    if let Some(ex) = &existing
        && ex.is_running()
    {
        if ex.template() == template.as_slice()
            && ex.cwd == dir
            && ex.port_request() == port_request
            && ex.env_keys == env_keys
            && ex.env_hash == env_hash
        {
            let mut ex = ex.clone();
            if let Some(ttl) = a.service_ttl.or(ex.service_ttl_secs) {
                ex.service_ttl_secs = Some(ttl);
                ex.service_expires_at = Some(util::rfc3339(util::now_secs().saturating_add(ttl)));
                store.put(&ex)?;
            }
            drop(lock);
            let mut out = finish_existing(&store, ex, check, wait, short)?;
            out.warnings.extend(touched.warnings);
            return Ok(out);
        }
        return Err(TabaError::new(
            "E_CONFLICT",
            format!(
                "{} is already running with a different command, directory, port or environment; stop it first",
                ex.id(),
            ),
        )
        .details(json!({
            "service": ex.id(), "command": ex.template(), "directory": ex.cwd,
            "port": ex.port, "port_request": ex.port_request(), "instance": ex.instance(),
            "env": ex.env_keys,
        }))
        .next(Next::run(
            "stop it, then run again",
            format!("taba stop {}", ex.id()),
        ))
        .next(Next::just("or use another --name")));
    }
    // An exited record is replaced.
    let replacing = existing.is_some();

    let port = match &a.port {
        None => None,
        Some(PortSpec::Fixed(p)) => {
            ports::check_port_free(&store, *p, &scope, &req)?;
            Some(*p)
        }
        Some(PortSpec::Auto { first, last, .. }) => {
            Some(ports::pick_auto(&store, *first, *last, &scope, &name)?)
        }
    };
    let mut warnings = touched.warnings;
    if matches!(a.port, Some(PortSpec::Auto { .. }))
        && !template.iter().any(|x| x.contains("{port}"))
    {
        warnings.push(Warning {
            code: "W_PORT_NOT_IN_COMMAND",
            message: format!(
                "the command has no {{port}}; taba only set PORT={}, which the app may ignore",
                port.unwrap_or(0)
            ),
        });
    }
    let command: Vec<String> = match port {
        Some(p) => template
            .iter()
            .map(|x| x.replace("{port}", &p.to_string()))
            .collect(),
        None => template.clone(),
    };

    let log_path = store.log_path(&scope, &name);
    state::create_private_dir_all(log_path.parent().expect("log has parent"))?;
    let mut log = state::private_file()
        .append(true)
        .open(&log_path)
        .map_err(|e| TabaError::state(format!("cannot open log {}: {e}", log_path.display())))?;
    writeln!(
        log,
        "--- taba: {} starting {:?} in {} ---",
        local_timestamp(),
        command,
        dir.display()
    )?;
    let log_offset = log.metadata()?.len();
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(proc::resolve_program(&command[0]));
    #[cfg(windows)]
    if let Some(line) = runform::shell_line(&command) {
        // cmd.exe parses its own command line: pass the line as typed.
        use std::os::windows::process::CommandExt;
        cmd.arg("/C").raw_arg(line);
    } else {
        cmd.args(&command[1..]);
    }
    #[cfg(not(windows))]
    cmd.args(&command[1..]);
    cmd.current_dir(&dir)
        // Caller identity and output preferences belong to the invoking
        // agent, not nested taba calls made by the service. Explicit --env
        // (or project env) remains a deliberate override.
        .env_remove("TABA_AGENT")
        .env_remove("TABA_AGENT_PID")
        .env_remove("TABA_OUTPUT")
        .envs(a.env.iter().map(|(k, v)| (k, v)))
        .env("TABA_SCOPE", &scope)
        .env("TABA_SERVICE", &name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(p) = port {
        cmd.env("PORT", p.to_string());
    }
    let start_failed = |message: String| {
        TabaError::new("E_START_FAILED", message)
            .details(json!({
                "service": id, "log_path": log_path, "log_tail": log_tail(&log_path, 20),
            }))
            .next(Next::run("read the log", format!("taba logs {id}")))
    };
    // Detached: its own session / process group (Unix) or console and job
    // (Windows), so closing the launching terminal does not stop it.
    let spawned = proc::spawn_detached(cmd).map_err(|e| match e {
        proc::SpawnError::Start(e) => start_failed(format!("cannot start {:?}: {e}", command[0])),
        proc::SpawnError::Vanished => {
            start_failed(format!("{id} exited immediately; see taba logs {id}"))
        }
    })?;
    let pid = spawned.pid;
    let mut svc = Service {
        scope: scope.clone(),
        name: name.clone(),
        command,
        cwd: dir.clone(),
        port,
        pid,
        pgid: spawned.pgid,
        start_time: spawned.start_time,
        log_path: log_path.clone(),
        command_template: Some(template),
        port_request,
        instance: Some(format!("{pid}-{}", spawned.start_time)),
        started_by: Some(caller.agent.clone()),
        ready: None,
        service_ttl_secs: a.service_ttl,
        service_expires_at: a
            .service_ttl
            .map(|t| util::rfc3339(util::now_secs().saturating_add(t))),
        moves: Vec::new(),
        stop_requested_at: None,
        env_keys,
        env_hash,
        discovered_ports: Vec::new(),
    };
    store.put(&svc)?;
    if let Some(borrow) = &delegation {
        borrows::rebind(&store, borrow, &svc)?;
    }
    let op = if replacing { "replace" } else { "start" };
    store.put_op(&scope, &name, &OpLock::mine(op, &caller.agent))?;
    // The port is reserved by the record now; wait without the global lock.
    drop(lock);

    let outcome = match (&check, wait) {
        (_, Some(w)) if short.is_some_and(|s| s.port_wait) => ready::wait_for_port(&mut svc, w),
        (Some(c), Some(w)) => ready::wait(&svc, c, w, log_offset),
        _ => {
            thread::sleep(Duration::from_millis(200));
            if svc.is_running() {
                Outcome::Ready { ms: 200 }
            } else {
                Outcome::Exited
            }
        }
    };

    if matches!(outcome, Outcome::Ready { .. }) && svc.discovered_ports.is_empty() {
        svc.discovered_ports = ready::discover_ports(&svc);
    }

    {
        let _lock = store.lock()?;
        if let Outcome::Ready { .. } = &outcome {
            let check_name = match &check {
                Some(c) => Some(c.name()),
                None if short.is_some_and(|s| s.port_wait) => Some("port"),
                None => None,
            };
            if let Some(check) = check_name {
                svc.ready = Some(state::Ready {
                    check: check.to_string(),
                    ready_at: util::now_micros(),
                });
            }
        }
        if svc.ready.is_some() || !svc.discovered_ports.is_empty() {
            save_if_current(&store, &svc)?;
        }
        store.release_op(&scope, &name)?;
    }
    run_result(
        svc,
        "started",
        check.as_ref(),
        wait,
        outcome,
        warnings,
        short,
    )
}

/// Writes `svc` back, but only if the record still describes this
/// instance. Call under the global lock.
fn save_if_current(store: &Store, svc: &Service) -> Result<()> {
    if store
        .get(&svc.scope, &svc.name)?
        .is_some_and(|cur| cur.instance() == svc.instance())
    {
        store.put(svc)?;
    }
    Ok(())
}

/// `run` found the same service already running: report it, after the
/// ready check when --wait was given.
fn finish_existing(
    store: &Store,
    mut svc: Service,
    check: Option<ReadySpec>,
    wait: Option<Duration>,
    short: Option<&Short>,
) -> Result<Output> {
    let outcome = match (&check, wait) {
        (_, Some(w)) if short.is_some_and(|s| s.port_wait) => ready::wait_for_port(&mut svc, w),
        (Some(c), Some(w)) => ready::wait(&svc, c, w, 0),
        _ => Outcome::Ready { ms: 0 },
    };
    if matches!(outcome, Outcome::Ready { .. }) {
        let ports = ready::discover_ports(&svc);
        if !ports.is_empty() && ports != svc.discovered_ports {
            svc.discovered_ports = ports;
            let _lock = store.lock()?;
            save_if_current(store, &svc)?;
        }
    }
    run_result(
        svc,
        "already_running",
        check.as_ref(),
        wait,
        outcome,
        Vec::new(),
        short,
    )
}

/// The text a short-form `run` prints:
///
/// ```text
/// started dev → http://localhost:5173
///   command: npm run dev
///   name "dev" (from "npm run dev"; set another with --name)
///   logs: taba logs dev · stop: taba stop dev
/// ```
fn short_text(svc: &Service, started: bool, short: &Short) -> String {
    let label = &short.label;
    let mut head = if started {
        format!("started {label}")
    } else {
        format!("already running: {label}")
    };
    match svc.url() {
        Some(url) => {
            head.push_str(&format!(" → {url}"));
            let shown = svc.port.or_else(|| svc.discovered_ports.first().copied());
            let others: Vec<String> = svc
                .discovered_ports
                .iter()
                .filter(|p| Some(**p) != shown)
                .map(u16::to_string)
                .collect();
            if !others.is_empty() {
                head.push_str(&format!(" (also listening on {})", others.join(", ")));
            }
        }
        None if short.port_wait => {
            head.push_str(&format!(", running, no port yet (see: taba logs {label})"));
        }
        None => {}
    }
    format!(
        "{head}\n  command: {}\n  name {:?} (from {:?}; set another with --name)\n  logs: taba logs {label} · stop: taba stop {label}",
        runform::display_command(svc.template()),
        svc.name,
        short.derived.source_text(),
    )
}

fn run_result(
    svc: Service,
    status: &str,
    check: Option<&ReadySpec>,
    wait: Option<Duration>,
    outcome: Outcome,
    warnings: Vec<Warning>,
    short: Option<&Short>,
) -> Result<Output> {
    let id = svc.id();
    let port_wait = short.is_some_and(|s| s.port_wait);
    // The short form's port wait ends without an error when no port showed
    // up in time: the service runs, it has no port yet.
    let (outcome, port_found) = match outcome {
        Outcome::TimedOut { ms } if port_wait => (Outcome::Ready { ms }, false),
        o => (o, port_wait),
    };
    match outcome {
        Outcome::Ready { ms } => {
            let ready = match check {
                Some(c) => Some(json!({ "check": c.name(), "ms": ms })),
                None if port_found => Some(json!({ "check": "port", "ms": ms })),
                None => None,
            };
            let mut body = json!({
                "status": status,
                "service": id,
                "scope": svc.scope,
                "name": svc.name,
                "url": svc.url(),
                "port": svc.port,
                "instance": svc.instance(),
                "ready": ready,
                "pid": svc.pid,
                "directory": svc.cwd,
                "log_path": svc.log_path,
                "discovered_ports": svc.discovered_ports,
            });
            if !svc.env_keys.is_empty() {
                body["env"] = json!(svc.env_keys);
            }
            if let Some(short) = short {
                let mut out = Output::new(body, short_text(&svc, status == "started", short));
                out.warnings = warnings;
                return Ok(out);
            }
            let verb = if status == "started" {
                "started"
            } else {
                "already running"
            };
            let mut text = describe(verb, &svc);
            if !svc.env_keys.is_empty() {
                text.push_str(&format!(
                    "\n  env  {} (values not shown)",
                    svc.env_keys.join(", ")
                ));
            }
            if let Some(c) = check {
                text.push_str(&format!("\n  ready {} check passed in {ms} ms", c.name()));
            }
            let mut out = Output::new(body, text);
            out.warnings = warnings;
            Ok(out)
        }
        Outcome::Exited => Err(TabaError::new(
            "E_START_FAILED",
            format!("{id} exited before it was ready; see taba logs {id}"),
        )
        .details(json!({
            "service": id, "pid": svc.pid, "instance": svc.instance(),
            "log_path": svc.log_path, "log_tail": log_tail(&svc.log_path, 20),
        }))
        .next(Next::run("read the log", format!("taba logs {id}")))),
        Outcome::TimedOut { ms } => {
            let secs = wait.map_or(0.0, |w| w.as_secs_f64());
            let check_name = check.map_or("alive", ReadySpec::name);
            Err(TabaError::new(
                "E_NOT_READY",
                format!(
                    "{id} is running but its {check_name} check did not pass within {secs}s; \
                     it was left running (pid {})",
                    svc.pid
                ),
            )
            .details(json!({
                "service": id, "status": "running", "pid": svc.pid, "instance": svc.instance(),
                "port": svc.port, "url": svc.url(), "check": check_name, "waited_ms": ms,
                "log_path": svc.log_path, "log_tail": log_tail(&svc.log_path, 20),
            }))
            .next(Next::run("read the log", format!("taba logs {id}")))
            .next(Next::run(
                "stop it if it is stuck",
                format!("taba stop {id}"),
            )))
        }
    }
}

// ---------------------------------------------------------------- up / down

/// `taba up`: `run --wait` for each selected service of the nearest
/// `taba.toml`, in order. The first failure ends the command; services
/// already started stay up and are reported under `results`.
fn cmd_up(
    names: Vec<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    ttl: Option<config::Ttl>,
) -> Result<Output> {
    let caller = scope::resolve(scope_flag.clone(), agent.clone())?;
    let scope = caller.require_scope()?;
    let project = project::load_from(&caller.cwd)?;
    let selected = project.select(&names)?;
    let mut results: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut warnings = Vec::new();
    let mut failure = None;
    for (i, def) in selected.iter().enumerate() {
        // taba.toml gives an explicit name and an argv, so the `run`
        // command-line rules (see [`runform`]) do not apply.
        let args = RunArgs {
            scope: scope_flag.clone(),
            agent: agent.clone(),
            name: Some(def.name.clone()),
            port: def.port.clone(),
            wait: Some(def.wait),
            ready: def.ready.clone(),
            ttl,
            service_ttl: def.service_ttl,
            command: def.command.clone(),
            cwd: Some(def.cwd.clone()),
            env: def.env.clone(),
            save: false,
            force: false,
            shape: runform::Shape::default(),
            raw: Vec::new(),
            interactive: false,
        };
        let form = runform::resolve(
            args.name.clone(),
            args.wait,
            args.ready.is_some(),
            &args.command,
        )?;
        let res = run_service(args, form);
        match res {
            Ok(out) => {
                results.push(Value::Object(out.body));
                lines.push(out.text);
                warnings.extend(out.warnings);
            }
            Err(e) => {
                let skipped: Vec<&str> =
                    selected[i + 1..].iter().map(|d| d.name.as_str()).collect();
                results.push(json!({
                    "service": format!("{scope}/{}", def.name),
                    "status": "failed",
                    "error": e.to_json(),
                }));
                let mut message = format!("taba up stopped at {scope}/{}: {}", def.name, e.message);
                if !skipped.is_empty() {
                    message.push_str(&format!("; not started: {}", skipped.join(", ")));
                }
                let mut f = TabaError::new(e.code, message).details(json!({
                    "service": format!("{scope}/{}", def.name),
                    "not_started": skipped,
                    "cause": e.to_json(),
                }));
                for n in e.next {
                    f = f.next(n);
                }
                failure = Some(f);
                break;
            }
        }
    }
    let body = json!({
        "project": project.path,
        "scope": scope,
        "results": results,
    });
    let mut out = Output::new(body, lines.join("\n"));
    out.warnings = warnings;
    out.failure = failure;
    Ok(out)
}

/// `taba down`: stops the selected services of the nearest `taba.toml` in
/// the caller's scope, with the rules of `taba stop`. A service with no
/// record is already down.
fn cmd_down(
    names: Vec<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    timeout: f64,
) -> Result<Output> {
    if !timeout.is_finite() || timeout < 0.0 {
        return Err(TabaError::usage(
            "--timeout must be a non-negative number of seconds",
        ));
    }
    let timeout = Duration::from_secs_f64(timeout);
    let caller = scope::resolve(scope_flag, agent)?;
    let scope = caller.require_scope()?;
    let project = project::load_from(&caller.cwd)?;
    let selected = project.select(&names)?;
    let store = Store::open()?;
    let mut targets = Vec::new();
    let mut absent = Vec::new();
    for def in &selected {
        if store.get(&scope, &def.name)?.is_some() {
            targets.push((scope.clone(), def.name.clone()));
        } else {
            absent.push(format!("{scope}/{}", def.name));
        }
    }
    let mut out = if targets.is_empty() {
        Output::new(json!({ "results": [] }), String::new())
    } else {
        stop_services(&store, &caller, &targets, false, timeout, None)?
    };
    let mut lines: Vec<String> = Some(out.text.clone())
        .filter(|t| !t.is_empty())
        .into_iter()
        .collect();
    if let Some(Value::Array(results)) = out.body.get_mut("results") {
        for id in &absent {
            results.push(json!({ "service": id, "result": "absent" }));
            lines.push(format!("{id}: not running (no record)"));
        }
    }
    out.body.insert("project".into(), json!(project.path));
    out.body.insert("scope".into(), json!(scope));
    out.text = lines.join("\n");
    Ok(out)
}

// ---------------------------------------------------------------- whoami

fn cmd_whoami(scope: Option<String>, agent: Option<String>) -> Result<Output> {
    let caller = scope::resolve(scope, agent)?;
    let store = Store::open()?;
    let cfg = config::Config::load(store.root())?;
    let prospective = lease::prospective_lease(&cfg, &caller)?;
    gc::collect(&store, &cfg, false, 5, caller.scope.as_deref())?;
    let touched = if let Some(s) = &caller.scope {
        let _lock = store.lock()?;
        lease::touch(&store, &cfg, &caller, s, lease::Access::Observe, None)?
    } else {
        lease::Touched::default()
    };
    let rec = touched.record.as_ref();
    let services: Vec<String> = match &caller.scope {
        Some(s) => store.list(Some(s))?.iter().map(Service::id).collect(),
        None => Vec::new(),
    };
    let body = json!({
        "scope": caller.scope,
        "scope_source": caller.source.map(ScopeSource::as_str),
        "worktree": caller.worktree,
        "branch": caller.branch,
        "agent": caller.agent,
        "human": caller.is_human(),
        "directory": caller.cwd,
        "services": services,
        "holder": rec.map(|r| &r.holder.agent),
        "holder_pid": rec.and_then(|r| r.holder.pid),
        "you_hold_scope": touched.you_hold,
        "lease": caller.scope.as_ref().map(|_| rec.map_or(prospective, |r| r.lease_json(util::now_secs()))),
        "takeover": touched.takeover,
        "next": if rec.is_some() && !touched.you_hold { vec![Next::just("wait for the live lease to end, or ask the coordinator or human").to_json()] } else { Vec::new() },
    });
    let mut text = match (&caller.scope, caller.source) {
        (Some(s), Some(src)) => format!("scope     {s} (from {})", src.as_str()),
        _ => "scope     none (pass --scope, set TABA_SCOPE, or cd into a git worktree)".into(),
    };
    if let Some(w) = &caller.worktree {
        text.push_str(&format!("\nworktree  {}", tilde(w)));
    }
    if let Some(b) = &caller.branch {
        text.push_str(&format!("\nbranch    {b}"));
    }
    text.push_str(&format!("\nagent     {}", caller.agent));
    if caller.scope.is_some() {
        let list = if services.is_empty() {
            "none".to_string()
        } else {
            services.join(", ")
        };
        text.push_str(&format!("\nservices  {list}"));
    }
    if let Some(r) = rec {
        text.push_str(&format!(
            "\nholder    {} (TTL {})",
            r.holder.agent,
            r.ttl().display()
        ));
    }
    let mut out = Output::new(body, text);
    out.warnings = touched.warnings;
    Ok(out)
}

// ---------------------------------------------------------------- ps

fn status_of(store: &Store, s: &Service) -> &'static str {
    match store.live_op(&s.scope, &s.name) {
        Ok(Some(op)) if op.op == "stop" => "stopping",
        Ok(Some(_)) => "starting",
        _ if s.is_running() => "running",
        _ => "exited",
    }
}

/// What `ps` lists: the services (sorted by scope and name), their statuses
/// and their `--json` rows. Building it only reads state.
pub(crate) struct Snapshot {
    pub services: Vec<Service>,
    pub statuses: Vec<&'static str>,
    pub rows: Vec<Value>,
    /// Per service: the scopes holding a live borrow of it (for `ui`).
    pub borrowers: Vec<Vec<String>>,
}

/// Reads the `ps` rows of every service, or of one scope, optionally only
/// the running ones. Shared by `ps`, `ps --watch` and `ui`; it never changes
/// state.
pub(crate) fn ps_snapshot(
    store: &Store,
    scope: Option<&str>,
    running_only: bool,
) -> Result<Snapshot> {
    let mut services = store.list(scope)?;
    if running_only {
        services.retain(|s| s.is_running());
    }
    let statuses: Vec<&'static str> = services.iter().map(|s| status_of(store, s)).collect();
    // One clock reading makes a snapshot internally consistent at an expiry
    // boundary. It also means a watch changes once when that boundary passes,
    // rather than producing tick-by-tick clock churn.
    let now = util::now_secs();

    let mut rows = Vec::new();
    let mut borrowers = Vec::new();
    for (s, status) in services.iter().zip(&statuses) {
        let rec = store.get_scope(&s.scope)?;
        let queue = store.get_claims(&s.scope, &s.name)?;
        let borrows = borrows::live(store, s)?;
        let exclusive = borrows.iter().find(|b| b.exclusive);
        let service_expired = s
            .service_expires_at
            .as_deref()
            .and_then(util::parse_rfc3339)
            .is_some_and(|expires| now >= expires);
        // A service TTL is a direct collection candidate once it expires,
        // unless a live borrow protects this instance. An expired scope lease
        // is deliberately *not* a pending drop: claims and borrowers can take
        // ownership instead, so reporting it as certain cleanup would lie.
        let pending_gc = if service_expired && borrows.is_empty() {
            Some("service_ttl_expired")
        } else {
            None
        };
        rows.push(json!({
            "service": s.id(),
            "scope": s.scope,
            "name": s.name,
            "status": status,
            "url": s.url(),
            "port": s.port,
            "discovered_ports": s.discovered_ports,
            "directory": s.cwd,
            "command": s.command,
            "pid": s.pid,
            "pgid": s.pgid,
            "start_time": s.start_time,
            "log_path": s.log_path,
            "instance": s.instance(),
            "holder": rec.as_ref().map(|r| &r.holder.agent),
            "lease": rec.as_ref().map(|r| r.lease_json(now)),
            "service_ttl": s.service_ttl_secs.map(config::format_duration),
            "service_expires_at": s.service_expires_at,
            "pending_gc": pending_gc,
            "keep": rec.as_ref().is_some_and(|r| r.keep),
            "keep_reason": rec.as_ref().and_then(|r| r.keep_reason.as_deref()),
            "claims": claims::rows(store, &queue)?,
            "borrows": { "shared": borrows.iter().filter(|b| !b.exclusive).count(), "exclusive": usize::from(exclusive.is_some()) },
            "locked_by": exclusive.map(|b| &b.scope),
        }));
        borrowers.push(borrows.iter().map(|b| b.scope.clone()).collect());
    }
    Ok(Snapshot {
        services,
        statuses,
        rows,
        borrowers,
    })
}

fn cmd_ps(scope: Option<String>, running_only: bool) -> Result<Output> {
    if let Some(s) = &scope {
        check_ident("scope", s)?;
    }
    let store = Store::open()?;
    let caller = scope::resolve(None, None)?;
    let touched = peek_scope(&store, &caller)?;
    let Snapshot {
        services,
        statuses,
        rows,
        ..
    } = ps_snapshot(&store, scope.as_deref(), running_only)?;

    let mut table = vec![[
        "SERVICE".to_string(),
        "STATUS".to_string(),
        "URL".to_string(),
        "DIRECTORY".to_string(),
        "OWNER".to_string(),
        "NEXT".to_string(),
    ]];
    for ((s, status), row) in services.iter().zip(&statuses).zip(&rows) {
        let status = match (
            row["pending_gc"].as_str(),
            row["lease"]["state"].as_str() == Some("expired"),
        ) {
            (Some(reason), _) => format!("{status} ({reason})"),
            (None, true) => format!("{status} (lease expired)"),
            (None, false) => status.to_string(),
        };
        table.push([
            s.id(),
            status,
            s.url().unwrap_or_else(|| "-".into()),
            tilde(&s.cwd),
            row["holder"].as_str().map_or_else(
                || "-".into(),
                |a| {
                    if row["lease"]["state"] == "expired" {
                        format!("{a} (expired)")
                    } else {
                        a.into()
                    }
                },
            ),
            row["claims"]
                .as_array()
                .and_then(|q| q.iter().find(|e| e["alive"] == true))
                .and_then(|e| e["scope"].as_str())
                .unwrap_or("-")
                .into(),
        ]);
    }
    let mut body = json!({ "services": rows });
    if touched.takeover_available {
        body["takeover"] = json!("available");
    }
    let mut out = Output::new(body, util::render_table(&table));
    out.warnings = touched.warnings;
    Ok(out)
}

fn peek_scope(store: &Store, caller: &Caller) -> Result<lease::Touched> {
    let Some(scope) = &caller.scope else {
        return Ok(lease::Touched::default());
    };
    let cfg = config::Config::load(store.root())?;
    // `ps` and `logs` read without waiting (design §2 rule 2): the lease
    // renewal is skipped while another process holds the state lock.
    let Some(_lock) = store.try_lock()? else {
        return Ok(lease::Touched::default());
    };
    lease::touch(store, &cfg, caller, scope, lease::Access::Peek, None)
}

// ---------------------------------------------------------------- logs

const LOG_JSON_MAX_BYTES: u64 = 16 * 1024;

/// Find the start of the last `lines` lines by scanning backwards in blocks.
/// `min` limits the scan to the byte window that can be returned.
fn log_tail_start(file: &mut File, min: u64, end: u64, mut lines: u64) -> io::Result<u64> {
    if lines == 0 {
        return Ok(end);
    }
    let mut offset = end;
    let mut block = [0; 8192];
    while offset > min {
        let len = (offset - min).min(block.len() as u64) as usize;
        offset -= len as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut block[..len])?;
        for (index, &byte) in block[..len].iter().enumerate().rev() {
            let next = offset + index as u64 + 1;
            // A final newline terminates the last line.
            if byte == b'\n' && next != end {
                lines -= 1;
                if lines == 0 {
                    return Ok(next);
                }
            }
        }
    }
    Ok(min)
}

fn cmd_logs(target: &str, follow: bool, tail: Option<u64>, json: bool) -> Result<Option<Output>> {
    let caller = scope::resolve(None, None)?;
    let (scope, name) = resolve_target(target, &caller)?;
    let store = Store::open()?;
    let svc = store
        .get(&scope, &name)?
        .ok_or_else(|| not_found(&scope, &name))?;
    let peek = peek_scope(&store, &caller)?;
    let mut file = File::open(&svc.log_path)
        .map_err(|e| TabaError::state(format!("cannot open {}: {e}", svc.log_path.display())))?;
    let end = file.metadata()?.len();
    let min = if json {
        end.saturating_sub(LOG_JSON_MAX_BYTES)
    } else {
        0
    };
    let start = match tail {
        Some(lines) => log_tail_start(&mut file, min, end, lines)?,
        None => min,
    };
    file.seek(SeekFrom::Start(start))?;
    if json {
        let mut buf = Vec::new();
        file.take(end - start).read_to_end(&mut buf)?;
        let body = json!({
            "service": svc.id(),
            "log_path": svc.log_path,
            "log": String::from_utf8_lossy(&buf),
            "truncated": start > 0,
            "bytes_total": end,
        });
        let mut out = Output::new(body, String::new());
        out.warnings = peek.warnings;
        return Ok(Some(out));
    }
    let mut stdout = io::stdout().lock();
    if tail.is_some() {
        io::copy(&mut (&mut file).take(end - start), &mut stdout)?;
    } else {
        io::copy(&mut file, &mut stdout)?;
    }
    stdout.flush()?;
    if !follow {
        return Ok(None);
    }
    let mut pos = file.stream_position()?;
    let mut buf = Vec::new();
    loop {
        let running = svc.is_running();
        file.seek(SeekFrom::Start(pos))?;
        buf.clear();
        file.read_to_end(&mut buf)?;
        if !buf.is_empty() {
            pos += buf.len() as u64;
            if stdout.write_all(&buf).and_then(|_| stdout.flush()).is_err() {
                return Ok(None); // reader went away (e.g. `| head`)
            }
        } else if !running {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------- stop

/// The services a `stop` or `rm` acts on: `SCOPE/NAME`, `NAME` in the
/// caller's scope, or with no target every service of the explicit `--scope`.
fn targets(
    store: &Store,
    target: Option<String>,
    scope_flag_given: bool,
    caller: &Caller,
    verb: &str,
) -> Result<(Vec<(String, String)>, bool)> {
    match target {
        Some(t) => Ok((vec![resolve_target(&t, caller)?], true)),
        None => {
            let Some(s) = caller.scope.clone().filter(|_| scope_flag_given) else {
                return Err(TabaError::usage(format!(
                    "nothing to {verb}: give SCOPE/NAME or --scope SCOPE"
                )));
            };
            let list = store
                .list(Some(&s))?
                .into_iter()
                .map(|x| (x.scope, x.name))
                .collect();
            Ok((list, false))
        }
    }
}

struct Stopped {
    svc: Service,
    result: &'static str,
    signal: Option<&'static str>,
    forced: bool,
    /// The recorded pid is alive but is another process now (pid reuse).
    unverifiable: bool,
}

fn cmd_stop(
    target: Option<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    timeout: f64,
    force: bool,
    reason: Option<String>,
) -> Result<Output> {
    if !timeout.is_finite() || timeout < 0.0 {
        return Err(TabaError::usage(
            "--timeout must be a non-negative number of seconds",
        ));
    }
    let timeout = Duration::from_secs_f64(timeout);
    let reason = check_force(force, reason)?;
    let scope_flag_given = scope_flag.is_some();
    let caller = scope::resolve(scope_flag, agent)?;
    let store = Store::open()?;
    let (targets, single) = targets(&store, target, scope_flag_given, &caller, "stop")?;
    stop_services(
        &store,
        &caller,
        &targets,
        single,
        timeout,
        reason.as_deref(),
    )
}

/// Stops `targets` as `caller`: the body of `taba stop`, also used by
/// `taba ui`. With `single`, the first refusal is returned as the error;
/// otherwise each service is reported under `results`.
pub(crate) fn stop_services(
    store: &Store,
    caller: &Caller,
    targets: &[(String, String)],
    single: bool,
    timeout: Duration,
    reason: Option<&str>,
) -> Result<Output> {
    stop_services_with(
        &proc::System,
        store,
        caller,
        targets,
        single,
        timeout,
        reason,
    )
}

/// `stop_services` with the process operations of `ops`.
fn stop_services_with(
    ops: &impl proc::Ops,
    store: &Store,
    caller: &Caller,
    targets: &[(String, String)],
    single: bool,
    timeout: Duration,
    reason: Option<&str>,
) -> Result<Output> {
    let mut done = Vec::new();
    let mut refused: Vec<(String, TabaError)> = Vec::new();
    for (s, n) in targets {
        match stop_one(ops, store, caller, s, n, timeout, reason) {
            Ok(d) => done.push(d),
            Err(e) if single => return Err(e),
            Err(e) => refused.push((format!("{s}/{n}"), e)),
        }
    }

    let mut results: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for d in &done {
        let mut r = json!({
            "service": d.svc.id(),
            "result": d.result,
            "instance": d.svc.instance(),
            "directory": d.svc.cwd,
        });
        if let Some(sig) = d.signal {
            r["signal"] = json!(sig);
            r["platform_action"] = json!(match sig {
                "KILL" => proc::KILL_ACTION,
                _ => proc::GRACEFUL_ACTION,
            });
        }
        if d.forced {
            r["forced"] = json!(true);
        }
        if d.unverifiable {
            r["unverifiable"] = json!(true);
        }
        results.push(r);
        let what = match d.result {
            "stopped" => "stopped".to_string(),
            "killed" => format!("stopped ({} after timeout)", proc::KILL_SIGNAL),
            _ if d.unverifiable => format!(
                "not running (exited; pid {} is now another process, not signalled)",
                d.svc.pid
            ),
            _ => "not running (exited)".to_string(),
        };
        let forced = if d.forced { " (forced)" } else { "" };
        lines.push(format!(
            "{}: {what}{forced}  {}",
            d.svc.id(),
            tilde(&d.svc.cwd)
        ));
    }
    for (id, e) in &refused {
        let result = if e.code == "E_SIGNAL_FAILED" {
            "failed"
        } else {
            "refused"
        };
        results.push(json!({ "service": id, "result": result, "error": e.to_json() }));
    }
    if targets.is_empty() {
        lines.push(format!(
            "no services in scope {}",
            caller.scope.clone().unwrap_or_default()
        ));
    }
    let mut out = Output::new(json!({ "results": results }), lines.join("\n"));
    if !refused.is_empty() {
        // A refusal (exit 3) outranks a signal failure (exit 1).
        let first = refused
            .iter()
            .map(|(_, e)| e)
            .max_by_key(|e| e.exit_code() == 3)
            .expect("non-empty");
        let detail: Vec<String> = refused
            .iter()
            .map(|(id, e)| format!("{id}: {}", e.message))
            .collect();
        out.failure = Some(TabaError::new(
            first.code,
            format!(
                "{} of {} service(s) were not stopped:\n  {}",
                refused.len(),
                targets.len(),
                detail.join("\n  ")
            ),
        ));
    }
    Ok(out)
}

/// Stops one service under the operation lock: checks and lock-taking happen
/// under the global lock, signalling and waiting without it.
fn stop_one(
    ops: &impl proc::Ops,
    store: &Store,
    caller: &Caller,
    scope: &str,
    name: &str,
    timeout: Duration,
    force_reason: Option<&str>,
) -> Result<Stopped> {
    let (svc, preserve_exclusive) = {
        let _lock = store.lock()?;
        let svc = store
            .get(scope, name)?
            .ok_or_else(|| not_found(scope, name))?;
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        if owner && force_reason.is_none() {
            let cfg = config::Config::load(store.root())?;
            lease::touch(store, &cfg, caller, scope, lease::Access::Mutate, None)?;
        }
        let exclusive = !owner
            && borrows::live(store, &svc)?
                .iter()
                .any(|b| b.exclusive && b.owned(caller));
        if !owner && !exclusive && force_reason.is_none() {
            return Err(not_owner(caller, &svc, "stop"));
        }
        let preserve_exclusive =
            borrows::authorise(store, &svc, caller, force_reason.is_some(), true)?
                && force_reason.is_none();
        if let Some(op) = store.live_op(scope, name)? {
            return Err(busy(&svc.id(), &op));
        }
        store.clear_stale_op(scope, name)?;
        if !ops.is_running(&svc) {
            let unverifiable = matches!(proc::info(svc.pid), Some(p) if !p.zombie && p.start_time != svc.start_time);
            return Ok(Stopped {
                svc,
                result: "not_running",
                signal: None,
                forced: false,
                unverifiable,
            });
        }
        store.put_op(scope, name, &OpLock::mine("stop", &caller.agent))?;
        if let Some(reason) = force_reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            record_force(store, "force_stop", caller, &svc, reason, overrode)?;
        }
        (svc, preserve_exclusive)
    };

    // Re-verify identity immediately before signalling (as v0.1 does).
    let res = if ops.is_running(&svc) {
        ops.stop_group(&svc, timeout).map(Some)
    } else {
        Ok(None)
    };
    {
        let _lock = store.lock()?;
        store.release_op(scope, name)?;
        if res.is_ok() && !ops.is_running(&svc) {
            borrows::break_all(store, &svc, preserve_exclusive)?;
            claims::discard(store, &svc)?;
        }
    }
    let forced = force_reason.is_some();
    match res {
        Ok(Some(proc::StopResult::Terminated)) => Ok(Stopped {
            svc,
            result: "stopped",
            signal: Some("TERM"),
            forced,
            unverifiable: false,
        }),
        Ok(Some(proc::StopResult::Killed)) => Ok(Stopped {
            svc,
            result: "killed",
            signal: Some("KILL"),
            forced,
            unverifiable: false,
        }),
        Ok(None) => Ok(Stopped {
            svc,
            result: "not_running",
            signal: None,
            forced: false,
            unverifiable: false,
        }),
        Err(e) => Err(TabaError::new(
            "E_SIGNAL_FAILED",
            format!(
                "{}: cannot signal process group {}: {e}",
                svc.id(),
                svc.pgid
            ),
        )
        .details(json!({ "service": svc.id(), "pgid": svc.pgid }))),
    }
}

// ---------------------------------------------------------------- rm

fn cmd_rm(
    target: Option<String>,
    scope_flag: Option<String>,
    agent: Option<String>,
    force: bool,
    reason: Option<String>,
) -> Result<Output> {
    let reason = check_force(force, reason)?;
    let scope_flag_given = scope_flag.is_some();
    let caller = scope::resolve(scope_flag, agent)?;
    let store = Store::open()?;
    let forced_scope = caller
        .scope
        .clone()
        .filter(|_| reason.is_some() && target.is_none() && scope_flag_given);
    let mut out = rm_services(&store, &caller, reason.clone(), |store| {
        let (mut list, single) = targets(store, target, scope_flag_given, &caller, "remove")?;
        if let Some(scope) = &forced_scope {
            // A forced scope-wide rm also removes the scope's unreadable records.
            list.extend(
                store
                    .unreadable_records(scope)?
                    .into_iter()
                    .map(|n| (scope.clone(), n)),
            );
        }
        Ok((list, single))
    })?;
    if let (Some(scope), Some(reason)) = (forced_scope, reason) {
        // An unreadable scope record blocks every command for the scope;
        // only an explicit forced rm removes it, never taba on its own.
        let _lock = store.lock()?;
        if let Err(e) = store.get_scope(&scope)
            && state::unreadable(&e).is_some()
        {
            store.append_event(&json!({
                "at": util::rfc3339_now(),
                "event": "force_rm_scope_record",
                "agent": caller.agent,
                "caller_scope": caller.scope,
                "cwd": caller.cwd,
                "target": scope,
                "reason": reason,
                "unreadable": true,
            }))?;
            store.remove_scope(&scope)?;
            out.body
                .insert("scope_record".into(), json!("removed_unreadable"));
            out.text.push_str(&format!(
                "
scope {scope}: removed unreadable scope record"
            ));
        }
    }
    Ok(out)
}

/// Removes the services `select` picks (under the global lock) as `caller`:
/// the body of `taba rm`, also used by `taba ui`. `select` returns the
/// targets and whether this is a single-target call, whose refusal is
/// returned as the error.
pub(crate) fn rm_services(
    store: &Store,
    caller: &Caller,
    reason: Option<String>,
    select: impl FnOnce(&Store) -> Result<(Vec<(String, String)>, bool)>,
) -> Result<Output> {
    let _lock = store.lock()?;
    let (targets, single) = select(store)?;

    let mut lines = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut refusals: Vec<TabaError> = Vec::new();
    for (s, n) in &targets {
        let svc = match store.get(s, n) {
            Ok(svc) => svc.ok_or_else(|| not_found(s, n))?,
            Err(e) if state::unreadable(&e).is_some() && reason.is_some() => {
                // An unreadable record: nothing in it can be verified, so only
                // a forced rm removes it, and only when no operation is live.
                let id = format!("{s}/{n}");
                if let Some(op) = store.live_op(s, n)? {
                    return Err(busy(&id, &op));
                }
                store.append_event(&json!({
                    "at": util::rfc3339_now(),
                    "event": "force_rm",
                    "agent": caller.agent,
                    "caller_scope": caller.scope,
                    "cwd": caller.cwd,
                    "target": id,
                    "reason": reason,
                    "unreadable": true,
                }))?;
                store.remove(s, n)?;
                lines.push(format!(
                    "{id}: removed unreadable record and its log (a process it started is no longer tracked)"
                ));
                results.push(json!({ "service": id, "result": "removed", "unreadable": true }));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let owner = caller.scope.as_deref() == Some(svc.scope.as_str());
        let lease_refusal = if owner && reason.is_none() {
            let cfg = config::Config::load(store.root())?;
            lease::touch(store, &cfg, caller, s, lease::Access::Mutate, None).err()
        } else {
            None
        };
        let refusal = if let Some(e) = lease_refusal {
            Some(e)
        } else if !owner && reason.is_none() {
            Some(not_owner(caller, &svc, "rm"))
        } else if let Some(op) = store.live_op(s, n)? {
            Some(busy(&svc.id(), &op))
        } else if svc.is_running() {
            Some(
                TabaError::new(
                    "E_STILL_RUNNING",
                    format!(
                        "{}: still running; stop it first: taba stop {}",
                        svc.id(),
                        svc.id()
                    ),
                )
                .details(json!({ "service": svc.id() }))
                .next(Next::run(
                    "stop it first",
                    format!("taba stop {}", svc.id()),
                )),
            )
        } else {
            borrows::authorise(store, &svc, caller, reason.is_some(), false).err()
        };
        if let Some(e) = refusal {
            if single {
                return Err(e);
            }
            results.push(json!({
                "service": svc.id(), "result": "refused", "error": e.to_json(),
            }));
            refusals.push(e);
            continue;
        }
        if let Some(r) = &reason {
            let overrode: &[&str] = if owner { &[] } else { &["E_NOT_OWNER"] };
            record_force(store, "force_rm", caller, &svc, r, overrode)?;
        }
        borrows::break_all(store, &svc, false)?;
        store.remove(&svc.scope, &svc.name)?;
        lines.push(format!("{}: removed (record and log)", svc.id()));
        results.push(json!({ "service": svc.id(), "result": "removed" }));
    }
    if targets.is_empty() {
        lines.push(format!(
            "no services in scope {}",
            caller.scope.clone().unwrap_or_default()
        ));
    }
    let mut out = Output::new(json!({ "results": results }), lines.join("\n"));
    if let Some(first) = refusals.first() {
        let detail: Vec<String> = refusals.iter().map(|e| e.message.clone()).collect();
        out.failure = Some(TabaError::new(
            first.code,
            format!(
                "{}\n{} service(s) were not removed",
                detail.join("\n"),
                refusals.len()
            ),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::CommandFactory;

    fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
        for a in cmd.get_arguments() {
            if a.is_hide_set() || matches!(a.get_id().as_str(), "help" | "version") {
                continue;
            }
            if a.get_help().is_none_or(|h| h.to_string().trim().is_empty()) {
                missing.push(format!("{path} {}", a.get_id()));
            }
        }
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() {
                continue;
            }
            walk(sub, &format!("{path} {}", sub.get_name()), missing);
        }
    }

    #[test]
    fn every_flag_has_help() {
        let mut missing = Vec::new();
        walk(&Cli::command(), "taba", &mut missing);
        assert!(missing.is_empty(), "arguments without help: {missing:?}");
    }

    /// `runform::prepare` reads `run`'s raw arguments itself; its flag
    /// lists must match the clap definition.
    #[test]
    fn runform_knows_every_run_flag() {
        let cli = Cli::command();
        let run = cli.find_subcommand("run").unwrap();
        let mut values = Vec::new();
        let mut switches = vec!["--help".to_string(), "-h".to_string()];
        for a in run.get_arguments() {
            let Some(long) = a.get_long() else { continue };
            let takes_value = a.get_action().takes_values();
            let equals_only = a.is_require_equals_set();
            if takes_value && !equals_only {
                values.push(format!("--{long}"));
            } else if long != "help" {
                switches.push(format!("--{long}"));
            }
        }
        values.sort();
        switches.sort();
        let mut known_values: Vec<String> = runform::RUN_VALUE_FLAGS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut known_switches: Vec<String> = runform::RUN_SWITCHES
            .iter()
            .map(|s| s.to_string())
            .collect();
        known_values.sort();
        known_switches.sort();
        assert_eq!(values, known_values);
        assert_eq!(switches, known_switches);
    }
}

/// `taba stop` with a fake process layer: signal failures are deterministic
/// and no real process is signalled.
#[cfg(test)]
mod stop_tests {
    use super::*;
    use crate::proc::fake::FakeOps;
    use std::io::ErrorKind;
    use std::path::PathBuf;

    /// A throwaway store; removed on drop.
    struct Tmp(Store, PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    fn store(test: &str) -> Tmp {
        let dir =
            std::env::temp_dir().join(format!("taba-stop-unit-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Tmp(Store::at(dir.clone()), dir)
    }

    /// A service record whose pid and start time match no real process.
    fn service(store: &Store, scope: &str, name: &str) {
        store
            .put(&Service {
                scope: scope.into(),
                name: name.into(),
                command: vec!["dummy".into()],
                cwd: PathBuf::from("."),
                port: None,
                pid: i32::MAX,
                pgid: i32::MAX,
                start_time: 1,
                log_path: store.log_path(scope, name),
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

    fn human_in(scope: &str) -> Caller {
        Caller {
            scope: Some(scope.into()),
            source: None,
            worktree: None,
            branch: None,
            agent: "human".into(),
            agent_pid: None,
            cwd: PathBuf::from("."),
        }
    }

    fn targets(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(s, n)| (s.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn a_failed_signal_is_e_signal_failed_and_keeps_the_service() {
        let t = store("single");
        service(&t.0, "s1", "web");
        let ops = FakeOps::at(0);
        ops.fail.set(Some(ErrorKind::PermissionDenied));
        let err = stop_services_with(
            &ops,
            &t.0,
            &human_in("s1"),
            &targets(&[("s1", "web")]),
            true,
            Duration::from_secs(2),
            None,
        )
        .err()
        .expect("the stop fails");
        assert_eq!(ops.calls(), ["stop web 2000"]);
        assert_eq!(err.code, "E_SIGNAL_FAILED");
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.message.contains("s1/web: cannot signal process group")
                && err.message.contains("fake signal failure"),
            "{}",
            err.message
        );
        let j = err.to_json();
        assert_eq!(j["details"]["service"], "s1/web");
        assert_eq!(j["details"]["pgid"], i32::MAX);
        // The record stays, and the operation lock is released.
        assert!(t.0.get("s1", "web").unwrap().is_some());
        assert!(t.0.live_op("s1", "web").unwrap().is_none());
    }

    #[test]
    fn a_failed_signal_among_several_is_reported_as_failed() {
        let t = store("multi");
        service(&t.0, "s1", "bad");
        service(&t.0, "s1", "good");
        let ops = FakeOps::at(0);
        ops.fail.set(Some(ErrorKind::PermissionDenied));
        *ops.fail_name.borrow_mut() = Some("bad".into());
        let out = stop_services_with(
            &ops,
            &t.0,
            &human_in("s1"),
            &targets(&[("s1", "bad"), ("s1", "good")]),
            false,
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert_eq!(ops.calls(), ["stop bad 1000", "stop good 1000"]);
        let results = out.body["results"].as_array().unwrap();
        assert_eq!(results[0]["service"], "s1/good");
        assert_eq!(results[0]["result"], "stopped");
        assert_eq!(results[0]["signal"], "TERM");
        assert_eq!(results[1]["service"], "s1/bad");
        assert_eq!(results[1]["result"], "failed");
        assert_eq!(results[1]["error"]["code"], "E_SIGNAL_FAILED");
        let failure = out.failure.expect("a failure");
        assert_eq!(failure.code, "E_SIGNAL_FAILED");
        assert_eq!(failure.exit_code(), 1);
        assert!(
            failure.message.starts_with("1 of 2 service(s)"),
            "{}",
            failure.message
        );
        assert!(t.0.live_op("s1", "bad").unwrap().is_none());
    }

    #[test]
    fn a_refusal_outranks_a_signal_failure() {
        let t = store("outrank");
        service(&t.0, "s1", "bad");
        service(&t.0, "s2", "theirs");
        let ops = FakeOps::at(0);
        ops.fail.set(Some(ErrorKind::PermissionDenied));
        let out = stop_services_with(
            &ops,
            &t.0,
            &human_in("s1"),
            &targets(&[("s1", "bad"), ("s2", "theirs")]),
            false,
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        // Another scope's service is refused before any signal.
        assert_eq!(ops.calls(), ["stop bad 1000"]);
        let results = out.body["results"].as_array().unwrap();
        assert_eq!(results[0]["result"], "failed");
        assert_eq!(results[1]["result"], "refused");
        assert_eq!(results[1]["error"]["code"], "E_NOT_OWNER");
        let failure = out.failure.expect("a failure");
        assert_eq!(failure.code, "E_NOT_OWNER");
        assert_eq!(failure.exit_code(), 3);
        assert!(t.0.get("s2", "theirs").unwrap().is_some());
    }

    #[test]
    fn a_service_that_exited_is_not_signalled() {
        let t = store("exited");
        service(&t.0, "s1", "web");
        let ops = FakeOps::at(0);
        ops.running.set(false);
        ops.fail.set(Some(ErrorKind::PermissionDenied));
        let out = stop_services_with(
            &ops,
            &t.0,
            &human_in("s1"),
            &targets(&[("s1", "web")]),
            true,
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert!(ops.calls().is_empty());
        assert_eq!(out.body["results"][0]["result"], "not_running");
        assert!(out.failure.is_none());
    }
}
