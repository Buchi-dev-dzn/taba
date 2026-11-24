# taba design v0.2: traffic rules for agents sharing localhost

Status: design only, not implemented. Date: 2026-11-24. Supersedes
[plan.md](plan.md) (v0.1). Describes the code on `feat/taba-prototype` as of
commit `ee809eb` wherever it says "today" or "v0.1".

**One sentence.** taba is the traffic rules for several AI coding agents
(Claude Code, Codex, Orca workers in separate worktrees, sub-agents) that
share one Mac's `localhost`. Agents are the primary users. Humans watch and
decide.

The rules are borrowed from Rust's ownership model. Every way agents break
each other's servers looks like a memory-safety bug class, and Rust already
has a small set of rules that rules those classes out. taba applies the same
rules to dev servers, ports and shared databases. It can only enforce them on
actions that go through taba (or through the optional agent hook). Anything
else is advice.

Contents:

1. [Problem and scenarios](#1-problem-and-scenarios)
2. [Rust bug classes and the rules](#2-rust-bug-classes-and-the-rules)
3. [Concepts and data model](#3-concepts-and-data-model)
4. [CLI v0.2](#4-cli-v02)
5. [Agent integration](#5-agent-integration)
6. [Out of scope](#6-out-of-scope)
7. [Overlap with agent harnesses](#7-overlap-with-agent-harnesses)
8. [Prototype plan](#8-prototype-plan)
9. [Open decisions](#9-open-decisions)

---

## 1. Problem and scenarios

v0.1 solved "which task does `:5173` belong to?" for a human reading
`taba ps`. With several agents working at once, the problem changes. Agents
don't ask what a process is: they act. They start a second copy, kill
whatever holds the port, restart a server another agent is testing against,
or end their session and leave everything running. Each agent sees only its
own conversation, so nothing tells it that it is about to break someone
else's work.

### What exists today (v0.1, `src/`)

- `run`, `ps`, `logs`, `stop` and `rm`, grouped by **scope**. A scope comes from
  `--scope` or `$TABA_SCOPE` and is an error if neither is set
  (`src/main.rs`, `resolve_scope`).
- A JSON record per service with `scope, name, command, cwd, port, pid, pgid,
  start_time, log_path` (`src/state.rs`, `Service`).
- `run` is idempotent: the same scope, name, command, directory and port
  returns the running service. A record that has exited is replaced.
- `run --port P` checks the port first. It reports either a taba service that
  claims or holds the port, or the pid and command line of an unmanaged
  holder, and then starts nothing (`check_port_free`). It does not configure
  the app or wait for it: it sleeps 200 ms and checks the process is alive.
- `stop` signals a process group only if its identity still matches: leader
  pid plus start time, or surviving members that all started after the
  recorded time (`src/proc.rs`, `group_is_ours`). It sends SIGTERM, then
  SIGKILL after `--timeout`.
- `stop` and `rm` accept **any** scope. Nothing checks who is asking.
- The state has one global `flock` (`$TABA_HOME/lock`), and records are
  written with write-to-temp-then-rename. `run` holds the lock across the
  spawn and the 200 ms wait. `stop` releases it **before** signalling, so two
  concurrent `stop`s of the same service both signal it. That is harmless
  today, because ESRCH is ignored, but it is the race v0.2 has to close.
  `ps` and `logs` read without the lock.
- Errors are one line of text on stderr, and every error exits 1 (clap usage
  errors exit 2).

### Scenarios

Each scenario names the Rust bug class it resembles (section 2) and how v0.2
handles it.

**S1. A worker hits its usage limit and another agent continues in the same
worktree.** Claude worker A in `wt/login-fix` has `login-fix/web` on `:5173`
and `login-fix/db` on `:5433`. It hits its usage limit mid-task. The
coordinator dispatches Codex worker B into the same worktree. B doesn't know
those servers exist, so it starts new ones on other ports or fails on the
ports. Later A comes back and stops "its" servers while B is testing against
them. *Bug class: use-after-free and double free. v0.2: same worktree, same
derived scope; B sees the scope's services with `taba whoami` / `ps`, and
takes them over with `taba move --take`. A's later `stop` is refused because
A no longer holds the scope.*

**S2. Two workers start servers in parallel worktrees.** Workers in
`wt/login-fix` and `wt/billing` both run `npm run dev` on the default
`:5173`. The second fails, or on Vite without `--strictPort` silently moves to
`:5174`, and its agent then tests against the other worktree's server on
`:5173`. *Bug class: writing someone else's memory / use-after-free. v0.2:
`--port auto` hands out a free port under the state lock, and `--wait`
returns the URL that is actually ready.*

**S3. Several worktrees share one database.** One Postgres on `:5432` serves
three worktrees. Worker C runs `db:reset` and restarts Postgres with a new
config while workers D and E are running integration tests against it. *Bug
class: data race. v0.2: the database lives in its own scope (for example
`shared`). Readers take shared borrows, and the reset takes an exclusive
borrow (`borrow --mut -- CMD`) that refuses while another exclusive borrow is
held and tells the readers afterwards that the instance changed.*

**S4. An agent "fixes" a port conflict by killing the holder.** `run` fails
with `port 5173 is in use by taba service billing/web`. The agent runs
`lsof -ti:5173 | xargs kill -9` and continues. Billing's worker loses its
server mid-test. *Bug class: writing someone else's memory. v0.2: taba
refuses cross-scope `stop` (`E_NOT_OWNER`), and the optional PreToolUse hook
blocks the raw `kill` before it runs, with an explanation and the next step
(`--port auto`).*

**S5. An agent finishes without cleaning up.** A worker reports
`worker_done` and its session ends. Three servers and a watcher keep running
for days and hold ports that later workers trip over. *Bug class: leak. v0.2:
each scope has a lease. When the holder ends (SessionEnd hook, dead agent
pid, or TTL expiry), `taba gc` stops and removes what the scope owns, unless
a human pinned it.*

**S6. Sub-agents of one session restart the same server.** Two sub-agents in
one worktree both decide the API needs a restart. Both run `taba stop` then
`taba run`. *Bug class: data race (within one owner). v0.2: `stop` and a
replacing `run` take a per-service operation lock. The second one gets
`E_BUSY` and is told to wait for the instance the first one started.*

---

## 2. Rust bug classes and the rules

### Agent failures, by bug class

| Rust bug class | Agent failure | Rule that prevents it | Where taba enforces it |
| --- | --- | --- | --- |
| **Leak** (owned value never dropped) | A server keeps running after the agent or task that started it has ended (S5). | **Drop**: when the owner ends, what it owns is stopped. Owner end is detected by a lease (TTL, agent pid, SessionEnd hook), and `gc` drops expired owners. | `gc`, run lazily by `run` and explicitly. |
| **Double free** (two frees of one allocation) | Two agents stop or restart the same server: the second stop hits a replacement the first one started, or both start a replacement (S1, S6). | **Ownership**: only the owner scope (or an exclusive borrower) may stop or replace a service. **Operation lock**: one mutation per service at a time. | `stop`, `rm`, replacing `run` refuse non-owners (`E_NOT_OWNER`) and concurrent mutations (`E_BUSY`). |
| **Use-after-free** (dangling pointer) | An agent keeps calling `http://localhost:5173` after that server was stopped, or after a different server took the port (S1, S2). | **Lifetimes**: a borrow is tied to one service *instance*, and cannot outlive it. Stopping a borrowed service warns or refuses. Checking a borrow whose instance is gone returns `E_DANGLING`. | `borrow` records the instance; `stop` reports live borrows; `borrow --check` / `release` detect stale ones. |
| **Data race** (unsynchronised writes to shared memory) | Two agents restart, reconfigure, migrate or reset the same server or shared database at once (S3, S6). | **Exclusive borrow (`&mut`)**: at most one mutator at a time, through a lock with an expiry. **Shared borrow (`&`)**: many readers at once. | `borrow --mut`, the per-service operation lock. |
| **Writing someone else's memory** (out-of-bounds or wild write) | `kill`, `pkill` or `lsof -ti:PORT \| xargs kill` on a process another task owns (S4). | **Borrow checker**: taba refuses cross-scope mutations, and the hook refuses raw kills of processes owned by another scope. **`unsafe`**: a human can override with `--force`, and every override is recorded. | taba CLI; PreToolUse hook (Claude Code, Codex). |

### The rules

1. **Ownership.** Every service has exactly one owner: a **scope**. A scope is
   bound to a worktree (derived from the git worktree root) or named
   explicitly. One **agent** holds the scope at a time, under a lease. The
   scope outlives individual agents and terminals; the agent is
   replaceable. This is v0.1's rule ("the owner is the work item, not the
   terminal") plus an explicit holder.

2. **Shared borrow (`&`).** Any agent in any scope may *use* a service: read
   its URL, read its logs, send it traffic. `ps` and `logs` stay free and
   lock-free. Taking a recorded shared borrow (`taba borrow S/N`) is optional.
   It makes the dependency visible to the owner and returns the instance the
   borrower is using. Many shared borrows can coexist.

3. **Exclusive borrow (`&mut`).** Restarting, reconfiguring, resetting or
   migrating needs exclusive access. The owner gets it implicitly for the
   length of one `stop` or replacing `run` (the per-service operation lock).
   Anyone else, or the owner for a longer operation, takes
   `taba borrow --mut`. Only one exclusive borrow per service at a time.
   **Deviation from Rust:** an exclusive borrow does *not* wait for shared
   borrows to end, because stale readers would then block writers forever.
   Readers are notified instead: their borrow becomes `E_DANGLING` once the
   instance changes.

4. **Move.** A scope can be handed from one agent to another with
   `taba move`. After a move, the old agent is no longer the holder, and its
   mutations in that scope are refused like any other non-owner's. A move is
   free when the old lease has expired. Taking a live lease needs `--force`
   (a human, or the coordinator acting for one).

5. **Drop.** When the holder's lease ends, the scope is dropped: `gc` stops
   its services and removes their records. Logs are kept for a grace period
   (decision 2). A human can mark a scope `--keep` to opt out, which is
   "leaking on purpose" (`std::mem::forget`) and is shown in `ps`.

6. **Lifetimes.** A borrow cannot outlive the instance it borrows. Every
   borrow records the instance id (`pid` + `start_time`, which v0.1 already
   stores). When the owner stops a service that other scopes have borrowed,
   `stop` lists the borrowers and refuses unless `--break-borrows` is given
   (decision 3). After any stop or restart, those borrows check as
   `E_DANGLING` with the new URL, if there is one.

7. **Borrow checker.** taba checks every mutation against rules 1 to 6 and
   refuses with a structured error that says why and what to do next. The
   optional agent hook extends the check to raw `kill`, `pkill`, `killall`,
   `fuser -k` and `lsof … | xargs kill` before the agent's shell runs them.

8. **`unsafe`.** `--force --reason TEXT` overrides ownership checks. It is
   meant for humans. Every use is appended to `events.jsonl` with the
   caller's agent, scope, directory and reason, and shows in `ps` as a
   warning on the affected scope. The hook denies `--force` from agent tool
   calls, so in practice only a human terminal can use it. This is not
   foolproof (section "Limits").

### Limits

taba is a borrow checker without a compiler. It sees only what passes through
it:

- **Enforced:** mutations made through taba (`run`, `stop`, `rm`, `move`,
  `borrow`, `gc`), and, when the hook is installed, kill commands typed into
  the agent's shell tool that the hook can parse.
- **Not enforced:** a process started without taba (`npm run dev &`, Claude
  Code's own background tasks, a human's terminal). Raw `kill` from a shell
  without the hook, from a script, from `eval`, or with a pid taken from a
  variable. Traffic sent to a URL (`curl` against a dangling URL succeeds if
  something else now listens there). Database-level changes (`db:reset`)
  made without taking `borrow --mut` first. A human or agent that edits
  `$TABA_HOME` directly.
- **Unverified identity:** `TABA_AGENT` is a claim, not a credential. Any
  process running as the same Unix user can claim any agent name. taba
  protects against *accidents* between cooperating agents, not against an
  adversary. The `--force` audit trail exists so that accidents can be traced
  afterwards.
- **Time of check vs time of use:** the hook checks a pid before the shell
  runs; a pid can change between the check and the kill. taba's own stop
  re-verifies identity immediately before signalling (as v0.1 does).

---

## 3. Concepts and data model

### Owner identity: the scope

A scope is resolved in this order. The first match wins, and `taba whoami`
prints the result and its source:

| Order | Source | Example scope | Notes |
| --- | --- | --- | --- |
| 1 | `--scope S` | `login-fix` | Explicit, as in v0.1. |
| 2 | `$TABA_SCOPE` | `login-fix` | As in v0.1. Orchestrators set it per worker. |
| 3 | git worktree of the current directory | `login-fix-3f9a` | Derived, new in v0.2. |
| 4 | none | — | `run`, `borrow`, `move` fail with `E_NO_SCOPE`. Read-only commands still work. |

**Derived scope.** taba finds the worktree root with
`git rev-parse --show-toplevel` (or by walking up to the nearest `.git`
file or directory, so it does not depend on `git` being on `PATH`). The
scope name is the slug of the root's directory name plus the first 4 hex
digits of a hash of the canonical root path: `<dirname>-<hash4>`. It is
truncated so it fits the existing identifier rule (`validate_ident`: at most
64 characters of `[A-Za-z0-9._-]`, not starting with `.`). The branch is **not**
part of the name. It is recorded on the scope and shown in `ps`, and taba
warns (`W_BRANCH_CHANGED`) when the worktree's branch differs from the
recorded one. Reason: an agent that switches branches inside a worktree is
still the same task with the same ports, and a branch-derived name would
silently orphan its services (decision 1).

Two worktrees of the same repository always get different scopes, because
their root paths differ. Successive agents in one worktree get the *same*
scope. That is what makes S1 work.

### Agent identity

`TABA_AGENT` names the agent session that is acting, for example
`claude-code:5c1e…`, `codex:019a…` or `orca:a82b7606-…`. It can also be passed
as `--agent` on any command. Optional `TABA_AGENT_PID` is the pid of the
agent's long-lived process (the Claude Code or Codex process, not the
short-lived shell). taba stores that pid together with its start time, as it
already does for services, and treats the agent as gone as soon as that
process is gone.

If `TABA_AGENT` is unset, the caller is `human` (decision 2). In v0.1 every
caller is effectively `human`, so this keeps v0.1 behaviour for people who
never set it.

### Records

v0.2 keeps the v0.1 layout and adds files next to it. Every new field is
optional when reading, so v0.1 records load unchanged (a record without an
owner agent is treated as held by `human`).

```text
$TABA_HOME (default ~/.taba)
├── services/<scope>/<name>.json      # v0.1 record + new fields below
├── logs/<scope>/<name>.log           # unchanged
├── scopes/<scope>.json               # new: the owner, its holder and lease
├── borrows/<scope>/<name>/<id>.json  # new: one file per live borrow
├── ops/<scope>/<name>.json           # new: in-flight mutation (operation lock)
├── events.jsonl                      # new: append-only audit log
├── version                           # new: state schema version ("2")
└── lock                              # unchanged: global flock
```

**Service record** (v0.1 fields first):

```json
{
  "scope": "login-fix-3f9a",
  "name": "web",
  "command": ["npm", "run", "dev", "--", "--port", "4113", "--strictPort"],
  "cwd": "/Users/me/wt/login-fix",
  "port": 4113,
  "pid": 41230,
  "pgid": 41230,
  "start_time": 1795502011043765,
  "log_path": "/Users/me/.taba/logs/login-fix-3f9a/web.log",

  "command_template": ["npm", "run", "dev", "--", "--port", "{port}", "--strictPort"],
  "port_request": "auto",
  "instance": "41230-1795502011043765",
  "started_by": "claude-code:5c1e9d",
  "ttl_secs": null,
  "ready": { "check": "tcp", "ready_at": 1795502013200000 }
}
```

- `instance` is `pid-start_time`. It is what borrows point at. v0.1 already
  has both values; v0.2 only names their combination.
- `command_template` is what idempotence compares when `--port auto` was
  used, because the substituted port differs between runs.
- `ttl_secs` is an optional per-service limit (`run --ttl`), independent of
  the scope lease.

**Scope record:**

```json
{
  "scope": "login-fix-3f9a",
  "source": "git",
  "worktree": "/Users/me/wt/login-fix",
  "branch": "feat/login-fix",
  "holder": {
    "agent": "claude-code:5c1e9d",
    "pid": 38811,
    "pid_start_time": 1795501000000000
  },
  "lease": { "ttl_secs": 14400, "renewed_at": "2026-11-24T10:02:11Z", "expires_at": "2026-11-24T14:02:11Z" },
  "keep": false,
  "created_at": "2026-11-24T09:58:40Z",
  "moves": [
    { "at": "2026-11-24T09:58:40Z", "from": null, "to": "claude-code:5c1e9d", "how": "created" }
  ]
}
```

A scope record is created by the first `run` in that scope. It is removed by
`gc` (or by `rm --scope`) once the scope has no services and no borrows.

**Borrow record:**

```json
{
  "id": "b-7k2q",
  "kind": "shared",
  "service": "shared/db",
  "instance": "40110-1795500000000000",
  "url": "http://localhost:5432",
  "borrower": { "agent": "codex:019a3e", "scope": "billing-91c0" },
  "taken_at": "2026-11-24T10:05:00Z",
  "expires_at": "2026-11-24T14:05:00Z",
  "holder_pid": null
}
```

`kind` is `shared` or `exclusive`. An exclusive borrow taken with
`-- CMD` records taba's own pid and start time as `holder_pid`, and the
borrow ends when that process ends.

**Event** (one JSON object per line in `events.jsonl`):

```json
{"at":"2026-11-24T11:40:02Z","event":"force_stop","agent":"human","caller_scope":null,"cwd":"/Users/me","target":"billing-91c0/web","reason":"stuck worker","instance":"41888-1795503300000000"}
```

Events: `created, moved, force_*, gc_drop, borrow_broken, lease_ended,
hook_denied` (when the hook reports back), `keep_set`. taba never rewrites
or truncates the file. Rotation is out of scope, like log rotation in v0.1.

### Leases: TTL renewed on use (recommended) vs heartbeat

| | Heartbeat | TTL renewed on use (**recommended**) |
| --- | --- | --- |
| How | The holder writes "alive" every N seconds. | The lease has an expiry. Any taba command by the holder in that scope extends it. `taba renew` extends it explicitly. |
| Needs | A resident process or a loop in every agent. | Nothing resident. |
| Fits taba's "no daemon" property | no | yes |
| Crash detection | fast (N seconds) | slow by TTL alone, but fast with `TABA_AGENT_PID` (dead pid = expired lease) and with the SessionEnd hook |
| Failure mode | A busy agent that misses beats loses its servers. | A long-silent agent can lose its lease. Mitigated by a generous default TTL (4 h, decision 2) and by `gc` checking the holder pid first. |

So a lease is **expired** when any of these is true:

1. The holder's `TABA_AGENT_PID` was recorded and that process (pid with its
   start time) no longer exists.
2. The holder ran `taba lease end` (the SessionEnd hook does this).
3. `now > expires_at`.

Renewal: `run`, `stop`, `rm`, `borrow`, `renew` and `whoami` by the holder
renew the lease. `ps` and `logs` renew too, but only when the lease is more
than 60 s old, so read-heavy agents don't write on every call. A `human`
holder with no TTL never expires.

### Borrows and locks

Two kinds of lock, with different lifetimes:

1. **Global state lock** (`$TABA_HOME/lock`, `flock(LOCK_EX)`, exists in
   v0.1). It is held only for short read-modify-write steps on the state, and
   never while waiting on a process. v0.1's `run` holds it across the spawn and
   the 200 ms wait. With `--wait` lasting up to minutes, v0.2 must not.
2. **Per-service operation lock** (`ops/<scope>/<name>.json`). A record with
   the operation (`stop`, `replace`, `exclusive_borrow`), taba's own pid and
   start time, and an expiry. It is created under the global lock (it is
   refused if one exists and its holder is still alive), and the slow part
   (signal and wait, spawn and wait for ready, the `-- CMD` of an exclusive
   borrow) runs without the global lock. The record is deleted under the
   global lock when the operation ends. If taba itself crashes, the record's
   pid is dead and the next caller takes over. This replaces v0.1's
   "release the lock, then signal" in `stop`.

An exclusive borrow taken *without* `-- CMD` (`taba borrow --mut S/N`) also
creates the operation lock, with the borrower agent as holder and a TTL
(default 15 min), and ends with `release` or expiry.

Shared borrows take no lock. They are records that the owner's `stop` and
`gc` read.

### Hand-off (move)

```text
          run (first)          move --to B / move --take          lease end / TTL / pid gone
 (none) ─────────────▶ held(A) ─────────────────────────▶ held(B) ───────────────────────────▶ expired
                          │                                                                    │
                          └──────────── lease end / TTL / pid gone ──────────▶ expired ◀──────┘
                                                                                  │ move --take (free)
                                                                                  │ or gc (Drop)
```

- **Give:** the holder runs `taba move --to AGENT`. This is used when a worker
  knows it is about to stop. A usage-limit warning is the usual trigger.
- **Take:** the next agent runs `taba move --take`. This succeeds without
  force if the lease has expired. Otherwise it fails with `E_LEASE_LIVE`,
  naming the holder, when it last renewed, and when the lease expires.
  `--force --reason` overrides (human or coordinator).
- A move transfers the scope, with all its services and their leases, and
  keeps every running instance. Borrows by other scopes stay valid, because
  the instance did not change.
- The coordinator case: an orchestrator that dispatches worker B into worker
  A's worktree runs `taba move --scope S --to B --force --reason "A hit usage
  limit"` before starting B. Then B never sees a refusal.

### Concurrency safety of taba's own state

- Every mutation runs read-check-write under the global `flock`, as today.
  Records are written to `*.tmp` and renamed (as today), so lock-free readers
  (`ps`, `logs`) see either the old or the new record, never a partial one.
- Slow work happens outside the global lock, under the per-service operation
  lock (above). Two agents can then `run --wait` different services at the
  same time without queueing behind each other.
- `--port auto` picks and records the port under the global lock: it writes
  the record (status `starting`, port reserved) before releasing the lock.
  Two concurrent `--port auto` runs never pick the same port.
- `events.jsonl` is appended under the global lock with a single `write` of
  one line.
- Crash safety: every lock-like record (operation lock, exclusive borrow,
  starting service) carries a pid with its start time, so a crashed taba or
  agent can be detected and its record discarded. This uses the same identity
  check as `group_is_ours`.
- `version` holds the schema version. A v0.1 binary reading v0.2 state ignores
  the unknown files and fields (serde ignores unknown fields by default, and
  v0.1 never lists `scopes/`, `borrows/` or `ops/`). It would still stop
  across scopes, so mixing versions loses v0.2 protection. That is
  acceptable for a prototype.

---

## 4. CLI v0.2

### Common conventions

- **Output.** Human text by default. `--json` (or `TABA_OUTPUT=json`) prints
  one JSON document on stdout for success *and* failure, so agents never have
  to parse stderr. The skill tells agents to set `TABA_OUTPUT=json`.
- **Identity flags.** Every command accepts `--scope` and `--agent`. They
  override `$TABA_SCOPE` and `$TABA_AGENT`.
- **Success envelope:** `{"ok": true, ...command fields..., "warnings": [...]}`
- **Error envelope:**

  ```json
  {
    "ok": false,
    "error": {
      "code": "E_PORT_IN_USE",
      "message": "port 5173 is in use by taba service billing-91c0/web (/Users/me/wt/billing)",
      "details": { "port": 5173, "holder": { "kind": "taba", "service": "billing-91c0/web", "scope": "billing-91c0", "agent": "codex:019a3e", "pid": 41888 } },
      "next": [
        { "do": "start on a free port instead", "command": "taba run --name web --port auto --wait -- npm run dev -- --port {port} --strictPort" },
        { "do": "use the existing server read-only", "command": "taba borrow billing-91c0/web" }
      ]
    }
  }
  ```

  `next` lists actions that are allowed for this caller, in order of
  preference. It never suggests `kill` or `--force`. When only a human can
  resolve the error, the list ends with `{"do": "ask the human", ...}`.

- **Exit codes.** `0` ok. `1` runtime or state error. `2` usage (clap already
  uses 2). `3` refused by the ownership rules or by the state of the service
  (`E_NOT_OWNER`, `E_SCOPE_HELD`, `E_LEASE_LIVE`, `E_LOCKED`, `E_BUSY`,
  `E_BORROWED`, `E_CONFLICT`, `E_STILL_RUNNING`). `4` the
  service could not be started or got no port (`E_PORT_IN_USE`,
  `E_NO_FREE_PORT`, `E_START_FAILED`, `E_NOT_READY`). `5` not found
  (`E_NOT_FOUND`, `E_DANGLING`).

### Error codes

| Code | Exit | Meaning | Typical `next` |
| --- | --- | --- | --- |
| `E_USAGE` | 2 | Bad flags or arguments. | `taba <cmd> --help` |
| `E_NO_SCOPE` | 2 | No scope from `--scope`, `$TABA_SCOPE` or a git worktree. | pass `--scope`, or `cd` into the worktree |
| `E_INVALID_IDENT` | 2 | Scope or name fails `validate_ident`. | — |
| `E_NOT_FOUND` | 5 | No such service, scope or borrow. | `taba ps --json` |
| `E_CONFLICT` | 3 | `run` with the same name but a different command, directory or port while the old one runs (v0.1's "already running with a different command"). | `taba stop S/N` then re-run, or use another `--name` |
| `E_PORT_IN_USE` | 4 | Port held by a taba service or an unmanaged process (details say which). | `--port auto`; `taba borrow` if it is the server you wanted |
| `E_PORT_CLAIMED` | 4 | Port reserved by another taba service that is starting or running. | `--port auto` |
| `E_NO_FREE_PORT` | 4 | `--port auto` found nothing in its range. | `--port auto:<other start>`; `taba gc` |
| `E_START_FAILED` | 4 | The process exited before it was ready. `details.log_tail` has the last 20 log lines. | `taba logs S/N` |
| `E_NOT_READY` | 4 | `--wait` timed out. The process is still running (and is reported). | `taba logs S/N`; `taba stop S/N` |
| `E_NOT_OWNER` | 3 | Mutation of a service in a scope the caller does not hold. | `taba borrow S/N`; ask the holder; ask the human |
| `E_SCOPE_HELD` | 3 | `run` in a scope that another agent holds under a live lease. | `taba move --take` after expiry; ask the human |
| `E_LEASE_LIVE` | 3 | `move --take` while the lease is live. | wait until `expires_at`; ask the human |
| `E_LOCKED` | 3 | Another agent holds an exclusive borrow. | retry after `expires_at`, or `--wait-lock SECS` |
| `E_BUSY` | 3 | Another `stop` or replacing `run` is in flight. | retry; `taba ps S/N` |
| `E_BORROWED` | 3 | `stop` or `rm` of a service with live borrows from other scopes. | tell the borrowers; `--break-borrows` |
| `E_DANGLING` | 5 | Borrow's instance is gone (stopped, replaced, or the port now belongs to something else). | `taba borrow S/N` again (new URL in details) |
| `E_STILL_RUNNING` | 3 | `rm` of a running service (v0.1 behaviour). | `taba stop S/N` |
| `E_UNVERIFIABLE` | 1 | The recorded pid is alive but is no longer the process taba started (start time differs). v0.1 reports this case as `exited` and never signals it; v0.2 keeps the no-signal rule and names the case. | `taba ps --all` |
| `E_SIGNAL_FAILED` | 1 | `kill` on the process group failed. | — |
| `E_FORCE_NEEDS_REASON` | 2 | `--force` without `--reason`. | — |
| `E_STATE` | 1 | State directory unreadable, or the schema is too new. | — |

Warnings (`warnings[]`, never fatal): `W_BRANCH_CHANGED`,
`W_LEASE_RENEWED`, `W_BORROW_BROKEN` (a borrower's instance was replaced),
`W_FORCED` (a `--force` override happened in this scope), `W_UNMANAGED_PORT`
(an unmanaged process listens on a port in this worktree).

### `taba whoami` (new)

```sh
taba whoami [--json]
```

Prints the resolved scope, its source, the agent and the lease. Agents call it
first. It renews the lease if the caller is the holder.

```json
{
  "ok": true,
  "scope": "login-fix-3f9a",
  "scope_source": "git",
  "worktree": "/Users/me/wt/login-fix",
  "branch": "feat/login-fix",
  "agent": "codex:019a3e",
  "holder": "claude-code:5c1e9d",
  "you_hold_scope": false,
  "lease": { "state": "live", "expires_at": "2026-11-24T14:02:11Z" },
  "services": ["login-fix-3f9a/web", "login-fix-3f9a/db"],
  "next": [{ "do": "take over this scope when the lease expires, or ask the human", "command": "taba move --take" }]
}
```

Errors: `E_STATE`. Without a scope it still succeeds, with `"scope": null`.

### `taba run` (extended)

```sh
taba run [--scope S] --name N
         [--port P | --port auto | --port auto:START]
         [--wait[=SECS]] [--ready tcp | http:PATH | log:REGEX]
         [--ttl DURATION] [--json] -- CMD...
```

Unchanged from v0.1: detach in a new session and process group, append
stdout and stderr to the log, set `TABA_SCOPE` and `TABA_SERVICE` in the
child, idempotent re-run, port check before start.

New:

- **`--port auto`** picks the lowest free port in `4100–4999` (decided here,
  not open: it avoids the common defaults 3000, 5173, 8000 and 8080 that
  humans and unmanaged tools use). `--port auto:5173` tries 5173 first, then
  the next free ports. "Free" means: not reserved by a starting or running
  taba service, and not in use by the v0.1 check (`port_in_use`, loopback
  connect plus wildcard bind). taba passes the port to the app in two ways:
  it replaces every `{port}` in `CMD`, and sets `PORT` in the child's
  environment. If `CMD` contains no `{port}`, taba warns that the app may
  ignore `PORT`. `--wait` then catches an app that listens elsewhere.
- **`--wait[=SECS]`** (default 30 s) blocks until the service is ready, then
  prints the result. The ready check is `--ready`, default `tcp` (a connect to
  the port succeeds). `http:/health` waits for any HTTP response below 500.
  `log:REGEX` waits for a log line, for servers without a port. Without a
  port and without `--ready`, `--wait` keeps v0.1's check that the process
  is still alive after 200 ms. If the process exits first, the result is
  `E_START_FAILED` with the end of the log. If the timeout passes, the result
  is `E_NOT_READY` and the process keeps running (decided: stopping it would
  lose the evidence the agent needs; the lease still bounds its life).
- **`--ttl DURATION`** (`30m`, `2h`) stops the service at the next `gc` after
  that long without a renewal, even if the scope's lease is live. Use it for
  one-off previews.
- **Ownership:** `run` in a scope held by another agent under a live lease
  fails with `E_SCOPE_HELD`. The first `run` in a scope creates the scope
  record with the caller as holder. A replacing `run` (exited record, or
  `E_CONFLICT` resolved by stop) takes the operation lock.
- **Idempotence with `--port auto`:** a re-run compares `command_template`
  and `port_request` instead of the substituted command, so the same
  `--port auto` invocation returns the running instance and its port.

Success, `--wait --json`:

```json
{
  "ok": true,
  "status": "started",
  "service": "login-fix-3f9a/web",
  "scope": "login-fix-3f9a",
  "name": "web",
  "url": "http://localhost:4113",
  "port": 4113,
  "instance": "41230-1795502011043765",
  "ready": { "check": "tcp", "ms": 1840 },
  "pid": 41230,
  "directory": "/Users/me/wt/login-fix",
  "log_path": "/Users/me/.taba/logs/login-fix-3f9a/web.log",
  "warnings": []
}
```

`status` is `started` or `already_running` (v0.1's "already running", now
machine-readable). For `already_running`, `--wait` still runs the ready check
before it returns.

Errors: `E_NO_SCOPE, E_INVALID_IDENT, E_CONFLICT, E_SCOPE_HELD, E_BUSY,
E_PORT_IN_USE, E_PORT_CLAIMED, E_NO_FREE_PORT, E_START_FAILED, E_NOT_READY,
E_STATE`.

### `taba ps` (extended) and `taba ps --all` (new)

```sh
taba ps [--scope S] [--running] [--json]
taba ps --all [--json]
```

`ps` keeps every v0.1 JSON field (`service, scope, name, status, url, port,
directory, command, pid, pgid, start_time, log_path`) and adds `instance`,
`holder`, `lease` (`live` / `expired` / `none`), `keep`, `borrows` (count by
kind) and `locked_by`. `status` gains `starting` and `stopping`, which
appear while an operation lock is held. The text table gains an `OWNER`
column (`holder` with a mark when the lease has expired).

`ps --all` is **read-only** and answers "what is listening on this Mac, and
whose is it?". It lists every TCP socket in LISTEN state owned by the current
user (via `lsof -nP -iTCP -sTCP:LISTEN`, as v0.1 already uses `lsof` for one
port), whether or not taba started it. For each it shows the port, bound
address, pid, command line (`proc::argv`, exists), working directory (macOS
`proc_pidinfo(PROC_PIDVNODEPATHINFO)`), the git worktree root and branch of
that directory (read from `.git`, no writes), and whether taba manages it
(matched by process group to a verified record). It never signals or writes
anything. It does not renew leases.

```json
{
  "ok": true,
  "listeners": [
    {
      "port": 4113, "address": "127.0.0.1", "pid": 41230, "pgid": 41230,
      "command": "node /Users/me/wt/login-fix/node_modules/.bin/vite --port 4113 --strictPort",
      "directory": "/Users/me/wt/login-fix",
      "git": { "worktree": "/Users/me/wt/login-fix", "branch": "feat/login-fix" },
      "managed": true,
      "service": "login-fix-3f9a/web", "scope": "login-fix-3f9a", "holder": "claude-code:5c1e9d", "lease": "live"
    },
    {
      "port": 5173, "address": "*", "pid": 39001, "pgid": 38990,
      "command": "node /Users/me/wt/billing/node_modules/.bin/vite",
      "directory": "/Users/me/wt/billing",
      "git": { "worktree": "/Users/me/wt/billing", "branch": "feat/billing" },
      "managed": false,
      "service": null, "scope": null, "holder": null, "lease": null
    }
  ]
}
```

Text form: `PORT  MANAGED  SERVICE/PID  BRANCH  DIRECTORY  COMMAND`.
Unknown fields are `null` (for example, a directory taba isn't allowed to
read).

Errors: `E_STATE`. `lsof` missing or failing gives an empty list with a
warning, not an error.

### `taba logs` (unchanged, plus JSON errors)

```sh
taba logs [-f] S/N        # or N with $TABA_SCOPE / derived scope
```

Output is the raw log, as in v0.1. With `--json`, errors use the envelope.
Logs are readable by any scope (shared borrow without a record).

Errors: `E_NOT_FOUND, E_INVALID_IDENT, E_NO_SCOPE` (bare `NAME` without a
scope), `E_STATE`.

### `taba stop` (extended)

```sh
taba stop S/N | N | --scope S
          [--timeout SECS] [--break-borrows]
          [--force --reason TEXT] [--json]
```

v0.1 rules stay: verify identity before signalling, SIGTERM then SIGKILL
after `--timeout`, never signal unmanaged processes, never delete files.
`stop` with no target still needs an explicit `--scope`, even when the scope
could be derived.

New:

- **Cross-scope refusal.** The caller must hold the service's scope (or hold
  an exclusive borrow on that service). Otherwise: `E_NOT_OWNER`. A `human`
  caller with no resolvable scope is decision 4.
- **Expired lease.** If the target scope's lease has expired, any caller in
  any scope may stop it without `--force`. That is Drop, done early, and it
  is recorded as `gc_drop`.
- **Lifetimes.** If other scopes hold live borrows: `E_BORROWED` with the
  borrowers, unless `--break-borrows` is given. Then the borrows are marked
  broken, and the borrowers get `W_BORROW_BROKEN` on their next taba call.
- **Operation lock:** concurrent stops or replaces of one service give
  `E_BUSY` instead of double signalling.
- **`--force --reason`** skips the ownership and borrow checks, never the
  identity check. It is recorded.

```json
{
  "ok": true,
  "results": [
    { "service": "login-fix-3f9a/web", "result": "stopped", "signal": "TERM", "instance": "41230-1795502011043765", "broken_borrows": [] },
    { "service": "login-fix-3f9a/db", "result": "not_running" }
  ]
}
```

`result` is `stopped`, `killed` (SIGKILL after timeout), `not_running` or
`refused`. Per-service refusals appear in `results` with their error object.
The command exits 3 if any service was refused.

Errors: `E_NOT_FOUND, E_NO_SCOPE, E_NOT_OWNER, E_BORROWED, E_BUSY, E_LOCKED,
E_UNVERIFIABLE, E_SIGNAL_FAILED, E_FORCE_NEEDS_REASON`.

### `taba rm` (extended)

Same syntax as v0.1 plus `--force --reason`. Same ownership rule as `stop`.
`E_STILL_RUNNING` stays as v0.1's refusal. `rm --scope S` also deletes the
scope record when nothing is left.

Errors: `E_NOT_FOUND, E_NOT_OWNER, E_STILL_RUNNING, E_BUSY, E_STATE`.

### `taba borrow` and `taba release` (new)

```sh
taba borrow S/N [--ttl DURATION] [--json]                 # shared (&)
taba borrow --mut S/N [--ttl DURATION] [--wait-lock SECS] [--json]   # exclusive (&mut), until release
taba borrow --mut S/N [--wait-lock SECS] -- CMD...         # exclusive for the length of CMD
taba borrow --check BORROW_ID [--json]
taba release BORROW_ID | S/N [--json]
```

A shared borrow returns the URL and instance, and registers the borrower so
that the owner sees it. The default TTL is the borrower's scope lease. An
exclusive borrow takes the per-service lock. Holding it lets the borrower
`stop` and re-`run` that one service even though the borrower does not own
its scope: it is the sanctioned way for worker C to restart the shared
database in S3. The `-- CMD` form runs `CMD` in the foreground while holding
the lock, sets `TABA_BORROWED_URL` in its environment, and releases the lock
when `CMD` exits, also on Ctrl-C.

```json
{
  "ok": true,
  "borrow": "b-7k2q",
  "kind": "shared",
  "service": "shared/db",
  "url": "http://localhost:5432",
  "port": 5432,
  "instance": "40110-1795500000000000",
  "expires_at": "2026-11-24T14:05:00Z",
  "other_borrows": { "shared": 2, "exclusive": null }
}
```

`--check` returns the same shape if the instance is still running, otherwise
`E_DANGLING` with `details.current` (the new instance and URL, if the service
was restarted).

Errors: `E_NOT_FOUND, E_LOCKED` (exclusive held elsewhere, with holder and
expiry), `E_BUSY, E_DANGLING, E_NO_SCOPE` (a borrower needs a scope).

### `taba move` (new)

```sh
taba move [--scope S] --to AGENT [--json]                 # give: caller must hold S
taba move [--scope S] --take [--force --reason TEXT] [--json]   # take: lease must be expired, or force
```

```json
{
  "ok": true,
  "scope": "login-fix-3f9a",
  "from": "claude-code:5c1e9d",
  "to": "codex:019a3e",
  "how": "take_expired",
  "services": [
    { "service": "login-fix-3f9a/web", "status": "running", "url": "http://localhost:4113" },
    { "service": "login-fix-3f9a/db", "status": "running", "url": "http://localhost:5433" }
  ],
  "lease": { "expires_at": "2026-11-24T18:10:00Z" }
}
```

`how` is `give`, `take_expired` or `take_forced`. Errors: `E_NO_SCOPE,
E_NOT_FOUND, E_NOT_OWNER` (give by a non-holder), `E_LEASE_LIVE`,
`E_FORCE_NEEDS_REASON`.

### `taba renew`, `taba lease end`, `taba keep` (new)

```sh
taba renew [--scope S] [--ttl DURATION] [--json]
taba lease end [--scope S | --agent A] [--json]     # used by the SessionEnd hook
taba keep [--scope S] [--off] [--force --reason TEXT] [--json]
```

`renew` returns the new lease. `lease end` marks every lease held by the
agent as expired right away, and does nothing else (it must fit the
SessionEnd hook's 1.5 s budget, section 5). `keep` pins a scope so `gc` never
drops it. Agents may keep their own scope only with `--reason`, which is
recorded and shown in `ps`, so a human can see why something was left
running.

Errors: `E_NO_SCOPE, E_NOT_OWNER, E_FORCE_NEEDS_REASON`.

### `taba gc` (new)

```sh
taba gc [--dry-run] [--json]
```

For every scope whose lease has expired and which is not `keep`: stop its
services (with the identity check and `--break-borrows`), delete their records,
end their borrows, and remove the scope record. Also: drop services past
their own `--ttl`, delete borrow and operation records whose holder process
is gone, and delete logs of dropped services after the grace period
(decision 2). Every drop is an event.

`run` performs a **bounded lazy gc**: scopes expired more than 10 minutes ago
are dropped before port selection, and at most 5 services per call. That
releases ports leaked by crashed agents without any daemon. No launchd job
in v0.2.

```json
{
  "ok": true,
  "dry_run": false,
  "dropped": [
    { "scope": "old-task-77aa", "reason": "holder_pid_gone", "services": ["old-task-77aa/web"], "broken_borrows": ["b-1x9m"] }
  ],
  "kept": [{ "scope": "demo-0c21", "reason": "keep: human review tomorrow" }]
}
```

Errors: `E_STATE`. Per-service failures are reported in `dropped[].errors`.

### `taba hook` (new, section 5)

```sh
taba hook claude-code pre-tool-use     # reads hook JSON on stdin
taba hook claude-code session-start
taba hook claude-code session-end
taba hook codex pre-tool-use
```

---

## 5. Agent integration

### Identity per harness

| Who | `TABA_AGENT` | `TABA_AGENT_PID` | Scope |
| --- | --- | --- | --- |
| Claude Code | `claude-code:<session_id>`, written by a SessionStart hook to `CLAUDE_ENV_FILE` so that every later Bash call sees it | the hook's parent pid, if that is the Claude Code process (to verify in M6) | derived from the git worktree |
| Codex CLI | `codex:<session_id>`. How to export it per session is **unknown** (no env-file mechanism confirmed). Fallback: the skill tells the agent to pass `--agent codex:<id>` | unknown | derived |
| Orca worker | `orca:<worker orca_session_id>`. Proposed: Orca sets `TABA_AGENT` (and `TABA_SCOPE` if it wants task-named scopes) in the environment of the worker terminal it spawns. Whether Orca can do this today is **unknown**. Fallback: the coordinator's dispatch text includes the `export`, or the agent passes `--agent` | worker process pid if Orca exports it | derived (one worktree per worker), or `TABA_SCOPE=<task id>` |
| Sub-agents (Claude `Agent`, Codex sub-tasks) | inherit the parent's `TABA_AGENT`. A sub-agent is a borrow inside the parent's ownership, not a new owner | inherited | inherited |
| Human terminal | unset, so `human` | — | explicit, env or derived |

Explicit `--scope` or `TABA_SCOPE` beats derivation everywhere. This is how a
coordinator puts two workers into one scope on purpose, or names scopes after
tasks.

### Skill changes (`skills/taba/SKILL.md`)

Rewrite the skill around a short procedure, keep the v0.1 rules, and add:

1. **First call:** `TABA_OUTPUT=json taba whoami`. If `you_hold_scope` is
   false and services exist, follow `next` (take over after expiry, or ask).
   Don't start duplicates.
2. **Start:** `taba run --name web --port auto --wait -- <cmd with {port}>`,
   and read `url` from the JSON. Never invent a port. Never pass the port
   only to taba.
3. **Errors:** read `error.code` and do the first item of `error.next`. Never
   respond to `E_PORT_IN_USE`, `E_NOT_OWNER` or `E_LOCKED` with `kill`,
   `pkill`, `lsof | xargs kill` or `--force`.
4. **Using someone else's server:** `taba borrow S/N` and use the returned
   URL. Before a long test run against it, `taba borrow --check ID`.
5. **Restarting or resetting a shared server or DB:**
   `taba borrow --mut S/N -- <migration or reset command>`.
6. **Hand-off and usage limits:** if you are about to stop working, run
   `taba move --to <next agent>` if you know it. In any case report each
   service's name, URL and status. If you take over a worktree, run
   `taba move --take`.
7. **Finish:** `taba stop --scope <scope>` and `taba rm --scope <scope>`,
   unless the human needs a server. In that case use
   `taba keep --reason "<why>"` and say so in the hand-off.
8. **Remove** the v0.1 advice to choose scope names by hand, except for
   shared infrastructure (`shared`).

The AGENTS.md / CLAUDE.md snippet in the README gets the same changes when
the README is updated (not in this task).

### Claude Code hooks

Configured in `.claude/settings.json` (project) or `~/.claude/settings.json`.
Facts about the hook interface are from the official hooks reference (see
section 7, sources).

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "taba hook claude-code session-start" }] }],
    "PreToolUse":   [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "taba hook claude-code pre-tool-use" }] }],
    "SessionEnd":   [{ "hooks": [{ "type": "command", "command": "taba hook claude-code session-end" }] }]
  }
}
```

**SessionStart.** Reads `session_id` and `cwd` from stdin. It appends
`export TABA_AGENT=claude-code:<session_id>` (and `TABA_AGENT_PID`, if
verified) to the file named by `CLAUDE_ENV_FILE`, and `TABA_OUTPUT=json`. It
prints nothing that blocks.

**PreToolUse, matcher `Bash`.** It inspects only `tool_input.command`. It
never rewrites the command (no `updatedInput`), and it is fast: no network,
one `lsof` at most.

1. Tokenise the command with shell quoting rules and split on `;`, `&&`,
   `||` and `|`. Find these patterns:
   - `kill [-SIG] PID…` and `kill -- -PGID`
   - `kill $(lsof -t -i:PORT …)` and `lsof -ti:PORT | xargs kill`
   - `fuser -k PORT/tcp` and `npx kill-port PORT`
   - `pkill [-f] PATTERN` and `killall NAME`
   - `taba … --force` and `taba move … --take --force`
2. Resolve the targets **read-only**: literal pids directly; ports through
   `lsof` (taba runs the `lsof` itself, it does not run the agent's
   pipeline); `pkill` and `killall` patterns by matching `proc::argv` of the
   user's processes, as `pgrep -f` would.
3. Map each target pid to a taba service, by process group and verified
   identity, and from there to its owner scope. Find the caller's scope from
   the hook's `cwd` (same derivation as the CLI) and its agent from
   `session_id`.
4. Decide:

   | Target | Decision |
   | --- | --- |
   | taba service in the caller's own scope | **allow** (the skill already says to prefer `taba stop`) |
   | taba service in another scope, lease live | **deny** |
   | taba service in another scope, lease expired | **deny**, with `next`: `taba stop S/N` (which is allowed for expired leases) |
   | unmanaged process listening on a TCP port, or whose cwd is in another git worktree | **ask** (decision 5): the human sees the explanation and decides |
   | any other unmanaged process | **allow** |
   | `taba … --force` | **deny**: `--force` is for humans |
   | command not parseable (variables, `eval`, scripts, `sh -c "$X"`) | **allow**, and record `hook_unchecked` in `events.jsonl` |

5. Explain. A deny uses `hookSpecificOutput.permissionDecision: "deny"` and
   a `permissionDecisionReason`, which Claude sees. The reason states the
   fact, the rule and the next step, for example:

   ```text
   taba: blocked `lsof -ti:5173 | xargs kill -9`.
   Port 5173 is billing-91c0/web, owned by another task (holder codex:019a3e,
   worktree ~/wt/billing, lease live until 14:02). Killing it would break that
   task's server.
   Instead: taba run --name web --port auto --wait -- npm run dev -- --port {port} --strictPort
   If you really need that server stopped, ask the human.
   ```

6. **Fail open.** If the hook itself fails (taba missing, state unreadable,
   timeout), the tool call proceeds, because Claude Code's default for a
   failed PreToolUse hook is not to block. A team that wants fail-closed can
   set `"onFailure": "block"` (documented for Claude Code v2.1.295+).

**SessionEnd.** Runs `taba lease end --agent claude-code:<session_id>` and
nothing else. The hooks reference gives SessionEnd hooks a shared 1.5 s
budget, too short to stop servers safely. Actual cleanup happens in the next
`gc` (lazy, in any agent's `run`, or explicit). The *Stop* hook is not used:
it fires at the end of every turn, not at the end of the task.

### Codex CLI hooks

Codex documents a `PreToolUse` event with matcher `Bash`, `tool_input.command`
in stdin, and blocking by exit code 2 or
`hookSpecificOutput.permissionDecision: "deny"`. That is the same shape as
Claude Code, so `taba hook codex pre-tool-use` shares the implementation and
differs only in identity (`codex:<session_id>`). Codex requires non-managed
hooks to be trusted through `/hooks` before they run, and its own docs call
tool hooks "a useful guardrail, not a complete enforcement boundary". A
SessionStart env file and SessionEnd behaviour are **not confirmed** for
Codex, so its agent identity falls back to `--agent` (above).

Gemini CLI documents an equivalent `BeforeTool` hook. Its docs say Gemini CLI
was replaced by Antigravity CLI on 2026-06-18, so it is not a v0.2 target.

---

## 6. Out of scope

- **Isolation and containers.** No namespaces, no network isolation, no
  per-task filesystem. Two worktrees still share one `localhost` and one
  user. Use Docker for that ([benchmarks.md](benchmarks.md)).
- **Restarts and supervision.** No restart on crash, no health monitoring
  after `--wait`, no dependency ordering. `--wait` checks readiness once, at
  start. Use process-compose, dekit or pm2 for supervision
  ([comparison.md](comparison.md)).
- **A daemon.** Leases, gc and locks are designed to work without any
  resident process. There is no launchd agent in v0.2.
- **Linux (for now).** The design is portable (flock, process groups, pid
  plus start time), but `src/proc.rs` uses macOS APIs (`proc_pidinfo`,
  `proc_listallpids`, `KERN_PROCARGS2`), and `ps --all` needs
  `PROC_PIDVNODEPATHINFO`. Linux would need `/proc` versions of these.
- **Security between users or against a malicious agent.** Identity is
  claimed, not proven (section 2, "Limits").
- **Proxies and stable hostnames** (`web.login-fix.localhost`). These would
  remove port conflicts at the root, but they are a separate tool.
- **Database-level locking.** `borrow --mut` is an advisory lock that agents
  take before touching a shared DB. taba does not understand schemas,
  connections or transactions.
- **Log rotation, Windows, a TUI.** Unchanged from v0.1.

---

## 7. Overlap with agent harnesses

Checked 2026-11-24 against the official documentation pages listed below.
Facts are marked **[docs]** when the official page says so. **[summary]** means
the fact came from a long page that was read through an automated summary
rather than verbatim, so it should be re-checked before taba depends on it.
**[unknown]** means it was not found. Nothing here was tested by running the
harnesses.

### Claude Code

- Bash commands can run in the background: when asked, via `Ctrl+B`, or when
  a command hits its timeout. The tool "immediately returns a background task
  ID". "Output is written to a file and Claude can retrieve it using the Read
  tool." [docs: interactive mode]
- `/tasks` lists running background shells and subagents, and can stop them.
  [docs: interactive mode]
- "Background tasks are automatically cleaned up when Claude Code exits",
  including processes that detach under `setsid` or `timeout` on macOS and
  Linux. Output over 5 GB kills the task. `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`
  turns the feature off. [docs: interactive mode]
- The Bash tool's input has `command`, `description`, `timeout` and
  `run_in_background`. PreToolUse hooks can match `Bash` and deny with exit 2
  or `permissionDecision: "deny"` plus a reason shown to Claude. Precedence
  between hooks is `deny > defer > ask > allow`. [docs: hooks]
  `session_id` and `cwd` are in the hook input, and `CLAUDE_ENV_FILE` is
  available to SessionStart hooks. [summary: hooks]
- SessionEnd hooks "share a 1.5-second budget". [docs: hooks]
- Whether one Claude Code session can see or stop another session's
  background tasks: [unknown], not described in the docs read.

### Codex CLI (OpenAI)

- `features.unified_exec` is "Use resumable command execution (stable;
  enabled on all platforms)". `background_terminal_max_timeout` (default
  300000 ms) is the maximum poll window for background terminals.
  [summary: config reference]
- A `/ps`-style listing of background terminals: [unknown]. It is not in the
  CLI pages read. A third-party blog (2026-03-28) describes `/ps` behind
  `/experimental`, but that is not official.
- What happens to background processes when a Codex session ends: [unknown].
- `PreToolUse` hooks exist, match `Bash`, and can block (exit 2 or
  `permissionDecision: "deny"`). Non-managed hooks must be trusted via
  `/hooks`. [summary: hooks]

### Gemini CLI

- `run_shell_command` has an optional `is_background` parameter and returns
  "Background PIDs". [docs: shell tool, last updated 2026-05-01]
- `BeforeTool` hooks can deny a tool call with exit 2 or
  `decision: "deny"`. [docs: hooks reference, last updated 2026-04-10]
- Both pages say Gemini CLI was replaced by Antigravity CLI on 2026-06-18.
  Antigravity CLI: [unknown], not checked.

### Others

- Orca: whether it tracks processes started inside worker terminals, or sets
  per-worker environment variables: [unknown], not checked.
- Cursor, Windsurf, Aider and other harnesses: not checked.
- Local process managers (pm2, process-compose, dekit and others) are
  compared in [comparison.md](comparison.md). process-compose ships an MCP
  server, and dekit describes itself as "a full CLI for humans and agents".
  Neither has scopes bound to worktrees, cross-scope refusal or borrows
  (from that page's testing).

### What remains unique to taba

The harnesses manage **their own session's** background processes. Claude
Code's design is the clearest: background tasks belong to the session and die
with it. That prevents leaks inside one harness, but it creates three gaps
that taba fills:

1. **Across agents and harnesses.** No harness documents a view of processes
   started by *other* sessions or other tools. `taba ps --all` and the
   shared state cover every agent on the machine.
2. **Ownership that outlives a session.** A server tied to the session can't
   be handed to the next agent (S1) or left running for a human on purpose.
   taba's scope is bound to the worktree, with `move` and `keep`.
3. **Refusing to break someone else's work.** Harness hooks are generic. None
   ships rules about which process belongs to which task. taba supplies that
   knowledge (owner scope, lease, borrows) to the hook mechanisms that Claude
   Code and Codex already provide.

Plus what [comparison.md](comparison.md) already verified: a port check that
names the holder, idempotent `run`, no resident process.

### Sources (all read 2026-11-24)

- Claude Code, interactive mode: <https://code.claude.com/docs/en/interactive-mode>
- Claude Code, hooks reference: <https://code.claude.com/docs/en/hooks>
- Codex, changelog: <https://developers.openai.com/codex/changelog> (redirects to <https://learn.chatgpt.com/docs/changelog>; latest entry 2026-11-24, CLI 0.172.0)
- Codex, hooks: <https://developers.openai.com/codex/hooks> (redirects to <https://learn.chatgpt.com/docs/hooks>)
- Codex, configuration reference: <https://developers.openai.com/codex/config-reference> (redirects to <https://learn.chatgpt.com/docs/config-file/config-reference>)
- Codex CLI: <https://developers.openai.com/codex/cli/features> (redirects to <https://learn.chatgpt.com/docs/codex/cli>)
- Gemini CLI, shell tool: <https://geminicli.com/docs/tools/shell>
- Gemini CLI, hooks reference: <https://geminicli.com/docs/hooks/reference/>
- Third party, not relied on: <https://codex.danielvaughan.com/2026/03/28/codex-background-terminal-dev-server-workflows/>

---

## 8. Prototype plan

Each milestone is releasable on its own and keeps every v0.1 test passing.
Acceptance tests use a throwaway `TABA_HOME`, two throwaway git worktrees of
one repository (`wt-a`, `wt-b`), and `python3 -m http.server {port}` as the
service, like `scripts/readme-screenshots.sh` and `bench/`.

### M1: `run --wait`, `--port auto`, cross-scope stop refusal

Includes the minimum these need: scope derivation from the git worktree,
`whoami`, `--json` envelopes and error codes for `run` and `stop`, and the
operation lock for `stop`.

1. In `wt-a`, `taba run --name web --port auto --wait --json -- python3 -m http.server {port}`
   returns `ok: true`, a `url` in 4100–4999, and `curl $url` answers right
   away.
2. Ten such runs in parallel (different names) get ten distinct ports, and
   all ten answer.
3. Re-running test 1 verbatim returns `status: already_running` with the same
   port and instance.
4. `--port auto:8001` while 8001 is held by an unmanaged process picks 8002
   (or the next free port) and starts.
5. `--wait=2 -- sh -c 'sleep 5; exec python3 -m http.server {port}'` returns
   `E_NOT_READY` with exit 4, and the process is still in `ps`. `-- false`
   returns `E_START_FAILED` with `details.log_tail`.
6. In `wt-b`, `taba stop <wt-a scope>/web` returns `E_NOT_OWNER` with exit 3,
   and the server still answers. With `--force --reason test` it stops, and
   `events.jsonl` has a `force_stop` line.
7. Two simultaneous `taba stop` calls on one service in `wt-a`: one gets
   `stopped`, the other `E_BUSY` or `not_running`, and SIGTERM is sent once.
8. With `TABA_SCOPE` and `--scope` unset outside any git repo, `run` returns
   `E_NO_SCOPE`. With `TABA_SCOPE=x` set, v0.1 behaviour is unchanged.
9. A v0.1 state directory (records without new fields) still lists, stops
   and removes correctly.

### M2: JSON everywhere and `ps --all`

1. Every command with `--json` prints exactly one JSON document on stdout,
   for success and failure, and stderr is empty.
2. `ps --all --json` lists a taba service (`managed: true`, scope and holder
   filled) and an unmanaged `python3 -m http.server` started with `nohup` in
   `wt-b` (`managed: false`, `directory` and `git.branch` correct).
3. `ps --all` signals nothing and writes nothing (the `TABA_HOME` mtime is
   unchanged).

### M3: agents, leases and gc

1. `TABA_AGENT=a1` starts a service. `TABA_AGENT=a2` in the same worktree
   gets `E_SCOPE_HELD` on `run`.
2. With `TABA_AGENT_PID` set to a `sleep` process that is then killed, `ps`
   shows the lease as `expired`, and the next `run` anywhere (lazy gc, after
   the grace period, set to 0 in tests) stops the service and frees its port.
3. `--ttl 2s`: after 3 s, `taba gc` drops the service. `keep` survives `gc`.
4. `taba lease end --agent a1` followed by `gc --dry-run` lists a1's scopes
   and stops nothing.

### M4: borrows and lifetimes

1. `wt-b` takes `borrow shared/db`. The owner's `stop` returns `E_BORROWED`
   naming `wt-b`. `--break-borrows` stops it, and then `borrow --check`
   from `wt-b` returns `E_DANGLING`.
2. While `wt-b` holds `borrow --mut shared/db -- sleep 5`, `borrow --mut`
   from `wt-a` returns `E_LOCKED` with an expiry, and with
   `--wait-lock 10` succeeds after the first one ends.
3. Killing the taba process of a `-- CMD` borrow frees the lock on the next
   call.

### M5: move

1. Holder a1 runs `move --to a2`. a2 can now stop the services, a1 gets
   `E_NOT_OWNER`, and the services keep their instance and URL throughout.
2. `move --take` by a2 while a1's lease is live returns `E_LEASE_LIVE`. After
   expiry it succeeds with `how: take_expired`.

### M6: hooks and skill

1. Feeding recorded PreToolUse JSON to `taba hook claude-code pre-tool-use`:
   `lsof -ti:<other scope's port> | xargs kill` is denied with a reason that
   contains the owner scope and a `taba run --port auto` suggestion. `kill
   <own pid>` is allowed. `pkill -f http.server` is denied if any match
   belongs to another scope. `taba stop x --force` is denied.
2. Unparseable commands are allowed and logged.
3. In a real Claude Code session: the SessionStart hook sets `TABA_AGENT`,
   visible in `taba whoami`. Ending the session marks the lease expired.
4. The same deny test passes for `taba hook codex pre-tool-use` once the
   hook is trusted in Codex.
5. The skill is rewritten (section 5), and a scripted two-worker scenario (S2
   plus S4) ends with both servers running and no cross-scope kill.

---

## 9. Open decisions

Each has a recommended default that the design above already assumes.

1. **Derived scope: worktree path only, or path plus branch?**
   *Recommended: path only* (`<dirname>-<hash4>`), with the branch recorded
   and a warning when it changes. Including the branch would orphan running
   services whenever an agent switches branch.

2. **Default lease TTL, log grace period, and callers without `TABA_AGENT`.**
   *Recommended:* TTL 4 h renewed on use; the lazy gc in `run` waits 10 min
   after expiry; logs of dropped services kept 24 h; a caller without
   `TABA_AGENT` is `human` with no TTL. That keeps v0.1 behaviour for
   people, at the cost that an agent without identity behaves like v0.1 and
   can leak.

3. **`stop` of a service that other scopes have borrowed: refuse or warn?**
   *Recommended: refuse* with `E_BORROWED` and require `--break-borrows`.
   Agents act on errors more reliably than on warnings. Expired-lease drops
   and `gc` break borrows without asking.

4. **Mutations by an unscoped human** (no `TABA_AGENT`, no `--scope`, not in
   a worktree, for example `taba stop login-fix/web` from `~`).
   *Recommended: allow without `--force`, but record an event.* Agents
   always run inside a worktree, so they always have a scope. Requiring
   `--force` here would only add friction for people.

5. **Hook decision for unmanaged processes on a port or in another
   worktree: `ask` or `deny`?**
   *Recommended: `ask`.* taba can't know whether an unmanaged process
   matters, but a human can. `deny` stays reserved for processes taba knows
   belong to another task.
