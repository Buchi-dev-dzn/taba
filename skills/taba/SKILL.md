---
name: taba
description: Start, reuse, inspect and stop long-running local processes (dev servers, databases, file watchers, workers) with the `taba` CLI, grouped by task scope (by default the git worktree). Use whenever you need a process that outlives a single command, need a free port, need to check what is already running or on which port, read a background process's output, or clean up before handing work back.
---

# taba: long-running local processes, bundled per task

`taba` records which task (a **scope**) owns each process it starts, so that
you, other agents and the human can see what is running, and so that only
the owning task stops it. By default your scope is the git worktree you work
in. Everything below uses only these commands:

```sh
taba whoami [--scope S] [--agent A] [--json]
taba run [--scope S] --name N [--port P | --port auto | --port auto:START]
         [--wait[=SECS]] [--ready tcp|http:PATH|log:REGEX] [--ttl DURATION]
         [--service-ttl DURATION] [--json] -- CMD...
taba ps [--scope S] [--running] [--json]
taba ps --all [--json]
taba ps --watch --json [--scope S] [--running] [--interval DURATION]
taba logs S/N | N [--tail N] [-f] [--json]
taba stop S/N | N | --scope S [--timeout SECS] [--json]
taba rm S/N | N | --scope S [--json]
taba renew [--scope S] [--ttl DURATION] [--json]
taba lease end [--scope S] [--json]
taba keep [--scope S] --reason TEXT [--json]
taba gc [--dry-run] [--json]
taba borrow S/N [--ttl DURATION] [--json]
taba borrow --check BORROW_ID [--json]
taba borrow --mut S/N [--wait-lock SECS] [--json] -- CMD...
taba release BORROW_ID | S/N [--json]
taba move [S/N] --to AGENT_OR_SCOPE [--json]
taba move --take [--force --reason TEXT] [--json]
taba claim S/N [--cancel] [--json]
taba disown S/N | --scope S [--json]
taba hook claude-code|codex pre-tool-use|session-start|session-end
```

Every command takes `--json` (or `TABA_OUTPUT=json` in the environment) and
then prints exactly one JSON document with `"ok"`, for success and failure.
The one exception is `taba ps --watch --json`: it never ends on its own and
prints one `ps --json` document per line, first at once and then on every
change. Use it only from a program that reads the stream (it exits 0 on
Ctrl+C or when its stdout is closed), never in a one-shot tool call; there,
use `taba ps --json`.
`taba ui` is an interactive terminal UI for humans; agents should not run
it (it needs a terminal and waits for keys). Point the human to it instead.

## Identity, scope and leases

Set a distinct identity before the first mutating command. Hooks set it for
Claude Code. In Codex and Orca, export it for the session (for example,
`TABA_AGENT=codex:<session-id>` or `TABA_AGENT=orca:<session-id>`), and set
`TABA_AGENT_PID` to the long-lived agent process PID when that PID is
available. Without `TABA_AGENT`, every caller is `human`, so they share one
identity and human leases do not expire by TTL. These identifiers coordinate
cooperating agents; they are not credentials.

The first `whoami`, `run`, `renew`, or other mutating command establishes or
renews the scope lease. An agent lease defaults to four hours; `--ttl`,
`TABA_TTL`, then `$TABA_HOME/config.toml`'s `lease_ttl` override it. A live
lease held by another identity returns `E_SCOPE_HELD`; wait for the holder to
end or expire, or ask the coordinator. Do not change `--scope` or `--agent`
to impersonate a holder. `taba lease end` ends your lease without stopping
services; `taba keep --reason TEXT` preserves the scope's services beyond
expiry when the human needs them.

## Procedure

1. **First call: `taba whoami --json`.** It shows your `scope`, where it
   comes from (`scope_source`: `flag`, `env` or `git`) and the `services`
   that already exist in it. If `scope` is `null`, you are outside a git
   worktree: `cd` into the worktree you work in, or pass `--scope`.
   If a live lease belongs to another agent, wait for it to end or ask the
   coordinator. Do not use `--scope` or `--agent` to impersonate the holder.
   On `W_TOOK_OVER`, reuse the services you inherited.
2. **Reuse before you start.** If a service you need is listed, check it with
   `taba ps --scope <scope> --json` (rows are under `services`). If it has
   `"status": "running"`, use its `url` and don't start another. To see
   what holds a port, including processes taba didn't start, use
   `taba ps --all --json` (rows under `listeners`, with `managed`,
   `directory` and `git.branch`). It is read-only.
3. **Start with `--port auto --wait --json`** and put `{port}` where the app
   takes its port:

   ```sh
   taba run --name web --port auto --wait --json -- npm run dev -- --port {port} --strictPort
   taba run --name api --port auto --wait --json -- uvicorn app:app --port {port}
   taba run --name worker --json -- npm run worker          # no port
   ```

   Read `url` from the reply and use exactly that URL. Never invent a port,
   never use a port you did not get from taba, and make the app fail instead
   of moving to another port (`--strictPort` for Vite). taba also sets `PORT`
   in the app's environment. Re-running the same command is safe: it returns
   `"status": "already_running"` with the same URL.
4. **On an error, read `error.code` and do the first item of `error.next`.**
   Every reply is one JSON document with `"ok"`. Never answer
   `E_PORT_IN_USE`, `E_PORT_CLAIMED` or `E_NOT_OWNER` with `kill`, `pkill`,
   `lsof … | xargs kill` or `--force`, whether the holder is a taba service
   or a process taba didn't start. Start on another port with `--port auto`,
   or ask the human, who can stop it themselves.
5. **Stay inside your own scope.** Never stop or `rm` another scope's
   services. taba refuses it with `E_NOT_OWNER` (exit 3). Do not use raw
   stop commands or `--force` to circumvent that refusal. Optional hooks
   can reject unsafe shell actions. Claude Code asks the human before
   a forced tool call; Codex denies it because that hook cannot ask.
6. **Finish:** stop and clean up your scope, unless the human needs a server
   (see "Hand-off").

## Errors you will meet

| `error.code` | What happened | What to do |
| --- | --- | --- |
| `E_USAGE`, `E_INVALID_IDENT` | A flag, target, duration or identity is invalid. | Read `error.next` or `taba <command> --help`, correct it, then retry. |
| `E_NO_SCOPE` | No `--scope`, no `$TABA_SCOPE`, not in a git worktree. | `cd` into your worktree or pass its intended `--scope`. |
| `E_SCOPE_HELD` | Another agent has a live lease on the scope. | Wait for its lease to end or ask the coordinator; do not impersonate it. |
| `E_LEASE_LIVE` | `move --take` found a live holder. | Wait for the normal hand-off, or ask the coordinator. |
| `E_NOT_OWNER` | The service belongs to another scope. | Use its URL read-only, borrow it, or ask the coordinator. |
| `E_BORROWED` | The owner tried to stop or replace a service with live borrowers. | Leave it running, coordinate its hand-off with `disown`, or wait for borrowers to release. |
| `E_LOCKED` | Another scope has an exclusive maintenance borrow. | Retry later or use the documented `--wait-lock` when appropriate. |
| `E_DANGLING` | A borrow no longer refers to its recorded live instance. | Borrow the service again; do not trust the stale URL. |
| `E_NO_CLAIMANT` | `disown` found no eligible claimant. | Keep the service, or arrange for a successor to `claim` it first. |
| `E_NOT_FOUND` | The requested service, scope, or borrow is absent. | Run `taba ps --json` and use a current target or borrow ID. |
| `E_CONFLICT` | A name or destination conflicts with existing state. | Reuse the matching service, choose another name, or coordinate a transfer. |
| `E_STILL_RUNNING` | `rm` was asked to remove a running service. | Stop only your own service first, then retry `rm`. |
| `E_PORT_IN_USE`, `E_PORT_CLAIMED`, `E_NO_FREE_PORT` | The requested or automatic port cannot be used. | Use the `error.next` command or another `--port auto` range; never kill the holder. |
| `E_START_FAILED` | The process exited before it was ready. | Read `error.details.log_tail` or `taba logs <name> --tail 100 --json`, fix, re-run. |
| `E_NOT_READY` | `--wait` timed out; the process still runs. | Inspect logs; stop it only if it is your service and genuinely stuck. |
| `E_BORROW_COMMAND` | The command run under `borrow --mut` failed. | Read its output and `exit_code`, fix the maintenance command, then borrow again if needed. |
| `E_CONFIG` | `$TABA_HOME/config.toml`, `TABA_TTL`, or `TABA_LOCK_TIMEOUT` is invalid or unreadable. | Use `error.details` to correct the named key or environment value; do not discard state. |
| `E_STATE` | State is unreadable, damaged, unavailable, or lacks space. | Read `error.details.path` and `error.next`; ask the human before any repair that removes records. |
| `E_BUSY` | Another change is in progress, or the global state lock timed out. | Wait, inspect with `taba ps --json`, then retry. |
| `E_SIGNAL_FAILED` | taba could not signal a verified service. | Inspect the result and ask the human; do not use raw kill commands. |
| `E_FORCE_NEEDS_REASON` | A protected action needs an audited reason. | Do not bypass the guard; ask the human or coordinator for direction. |
| `E_UPDATE_FAILED` | An update check or verification failed. | Keep using the current binary and ask the human if an update is required. |

## Choosing a scope

- Inside a git worktree, the worktree folder is the scope (for example
  `login-fix-3f9a`), whatever branch is checked out. Agents in other
  worktrees have other scopes. A later agent in the same worktree shares
  your services.
- If `$TABA_SCOPE` is set, it wins over the worktree. Orchestrators use it to
  name scopes after tasks. `--scope` wins over both.
- Service names describe the role: `web`, `api`, `db`, `worker`.
- `taba stop` with no target needs an explicit `--scope`.

## Logs

```sh
taba logs web --tail 100 --json       # recent output (stdout + stderr) in your scope
taba logs login-fix-3f9a/web --tail 20 -f  # show recent lines, then follow until exit
```

Use `--tail 100 --json` for one-shot calls; `-f` blocks until the service
exits and is incompatible with `--json`. The `log` field contains up to 16 KiB
of source data, possibly starting mid-line. `truncated` is true if earlier
data was omitted, and `bytes_total` is the original log size in bytes. The
file is available at `log_path`, also listed by `taba ps --json`. Logs of any
scope are readable.

## Borrowing another scope's service

Use `taba borrow S/N --json` for the URL and borrow ID. Check that ID with
`taba borrow --check ID --json` before a long test run, and release it when
finished with `taba release ID --json`. A borrow ends when its TTL or the
borrower's lease ends. Treat `E_DANGLING` as a stale reference: borrow again
and use the replacement reply rather than the old URL. For exclusive
maintenance use `taba borrow --mut S/N -- COMMAND`; the command receives
`TABA_BORROWED_URL` and its lock ends when the command exits. `E_LOCKED`
means another exclusive borrower is active. A live borrow prevents an owner
from stopping or replacing the instance (`E_BORROWED`); coordinate a normal
release or hand-off rather than bypassing that protection.

## Stopping and cleaning up

```sh
taba stop web --json                 # one service of your scope
taba stop --scope <scope> --json     # everything in your scope
taba rm --scope <scope>              # delete records + logs of stopped services
```

taba only signals process groups it started and can still verify. It sends
SIGTERM, then SIGKILL after `--timeout` seconds (default 5); on Windows,
Ctrl+Break, then it terminates the service's job. Stopping never
deletes logs or records: stopped services stay in `taba ps` (status
`exited`). `rm` refuses running services, so stop first, and skip `rm` if the
human may still want the logs.

On Linux, keep `$TABA_HOME/btime/` with the service records. It holds the
boot-time values taba uses to verify running processes after clock changes.

## Hand-off

A successor in the same scope can take over after your lease ends. Use
`taba claim S/N` if you need a service after another scope's lease ends;
check for `W_HANDED_OVER` rather than assuming the service transferred. If you
know the next agent, give it the scope with `taba move --to AGENT`. Use
`taba move S/N --to SCOPE` when transferring just one service. Neither
command restarts the service. `W_TOOK_OVER` means your expired predecessor's
scope lease was adopted; reuse its services after inspecting them.

When you hand work back, or may be cut off (usage limits), report each
service you started, for example:

- `login-fix-3f9a/web`, http://localhost:4100, **left running** (needed for
  review), or **stopped**
- how to stop and clean it up:
  `taba stop --scope login-fix-3f9a && taba rm --scope login-fix-3f9a`

By default, stop what you no longer need and clean up:

```sh
taba stop --scope <scope> && taba rm --scope <scope>
```

Leave a service running only when the human will use it, and say so. Use
`taba keep --reason TEXT` when it must survive lease expiry. `taba lease end`
marks your leases expired without stopping anything immediately.

## Updating taba

`taba update --check --json` is read-only and reports whether a newer release
exists. Run `taba update` only when the human asks: it can replace the `taba`
binary itself (official standalone Linux and macOS installs; elsewhere it only
reports how to upgrade). It never restarts or stops services.
