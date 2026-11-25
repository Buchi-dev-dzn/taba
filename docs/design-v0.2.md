# taba design v0.2: traffic rules for agents sharing localhost

Status: M1 implemented on `feat/taba-prototype` (see "M1 implementation
notes" in section 8); M2 to M6 are design only. Date: 2026-11-24. Supersedes
[plan.md](plan.md) (v0.1). Describes the code on `feat/taba-prototype` as of
commit `ee809eb` wherever it says "today" or "v0.1". The owner's decisions
of 2026-11-24 are recorded in [section 9](#9-decisions-2026-11-24) and are
applied throughout.

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
9. [Decisions (2026-11-24)](#9-decisions-2026-11-24)

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
  recorded time (`src/proc/`, `group_is_ours`). It sends SIGTERM, then
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
derived scope. A's agent process is gone, so B's first taba call
(`taba whoami`) takes the scope over automatically, with both services and
their running instances, and records the hand-off (decision 6). A's later
`stop` is refused because A no longer holds the scope. If A's process is
still alive, B gets `E_SCOPE_HELD` naming A instead.*

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
class: data race. v0.2: the database is an ordinary service, owned by the
folder scope that started it. The other worktrees borrow it: shared borrows
(`&`) to use its URL, and an exclusive borrow (`borrow --mut -- CMD`) for the
reset, one at a time. The owner cannot stop or drop the database while it is
borrowed. When the owner's work ends first, the database is handed to the
next agent in its claim queue, where borrowers queue implicitly, instead of
being stopped (decisions 2 and 7).*

**S4. An agent "fixes" a port conflict by killing the holder.** `run` fails
with `port 5173 is in use by taba service billing/web`. The agent runs
`lsof -ti:5173 | xargs kill -9` and continues. Billing's worker loses its
server mid-test. *Bug class: writing someone else's memory. v0.2: taba
refuses cross-scope `stop` (`E_NOT_OWNER`), and the optional PreToolUse hook
denies the raw `kill` before it runs, with an explanation and the next step
(`--port auto`). The hook denies it just the same when the holder is an
unmanaged process (decision 5).*

**S5. An agent finishes without cleaning up.** A worker reports
`worker_done` and its session ends. Three servers and a watcher keep running
for days and hold ports that later workers trip over. *Bug class: leak. v0.2:
each scope has a lease. When the holder ends (SessionEnd hook, dead agent
pid, or TTL expiry, with the TTL configurable, decision 2) and no other agent
takes the scope over, `taba gc` first hands each service to a waiting
claimant (decision 7) and stops and removes the rest, unless a human pinned
it.*

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
| **Leak** (owned value never dropped) | A server keeps running after the agent or task that started it has ended (S5). | **Drop**: when the owner ends, what it owns is stopped. Owner end is detected by a lease (configurable TTL, agent pid, SessionEnd hook), and `gc` hands an expired owner's services to waiting claimants (borrowers included) and drops the rest. | `gc`, run lazily by `run` and explicitly. |
| **Double free** (two frees of one allocation) | Two agents stop or restart the same server: the second stop hits a replacement the first one started, or both start a replacement (S1, S6). | **Ownership**: only the owner scope (or an exclusive borrower) may stop or replace a service. **Operation lock**: one mutation per service at a time. | `stop`, `rm`, replacing `run` refuse non-owners (`E_NOT_OWNER`) and concurrent mutations (`E_BUSY`). |
| **Use-after-free** (dangling pointer) | An agent keeps calling `http://localhost:5173` after that server was stopped, or after a different server took the port (S1, S2). | **Lifetimes**: a borrow is tied to one service *instance*, and cannot outlive it. The owner cannot stop or drop a borrowed service (`E_BORROWED`); only `--force --reason` can, and it leaves the borrows dangling. Checking a borrow whose instance is gone returns `E_DANGLING`. | `borrow` records the instance; `stop` and `gc` refuse while borrowed; `borrow --check` / `release` detect stale ones. |
| **Data race** (unsynchronised writes to shared memory) | Two agents restart, reconfigure, migrate or reset the same server or shared database at once (S3, S6). | **Exclusive borrow (`&mut`)**: at most one mutator at a time, through a lock with an expiry. **Shared borrow (`&`)**: many readers at once. | `borrow --mut`, the per-service operation lock. |
| **Writing someone else's memory** (out-of-bounds or wild write) | `kill`, `pkill` or `lsof -ti:PORT \| xargs kill` on a process another task owns, or on an unmanaged server (S4). | **Borrow checker**: taba refuses cross-scope mutations, and the hook denies raw kills of processes owned by another scope and of unmanaged servers. **`unsafe`**: `--force --reason` is the only override, for humans too, and every override is recorded. | taba CLI; PreToolUse hook (Claude Code, Codex). |

### The rules

1. **Ownership.** Every service has exactly one owner: a **scope**. By
   default the scope is the folder: the git worktree root the agent works in
   (decision 1). A scope can also be named explicitly. One scope owns any
   number of named services (`web`, `api`, `worker`, `db`, ...), and they
   are listed, stopped and handed off together. One **agent** holds the
   scope at a time, under a lease. The scope outlives individual agents and
   terminals; the agent is replaceable. This is v0.1's rule ("the owner is
   the work item, not the terminal") plus an explicit holder.

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

4. **Move.** Ownership changes hands in two ways (decisions 2 and 6):
   - *A scope moves to another agent.* This is mostly automatic: when the
     holder's lease has ended (its agent process is gone, it ran
     `lease end`, or its TTL passed), the next agent that calls taba in that
     scope becomes the holder, with every service and running instance, and
     the hand-off is recorded. While the holder is alive, others get
     `E_SCOPE_HELD`. `taba move --to AGENT` and `taba move --take` stay as
     explicit tools for coordinators and humans; taking a live lease needs
     `--force --reason`.
   - *One service moves to another scope.* Explicitly with
     `taba move S/N --to SCOPE`, or automatically through the **claim
     queue** (decision 7): when the owner's lease ends, or the owner runs
     `taba disown S/N`, the service goes to the first live claimant
     (`taba claim S/N`), and after explicit claimants to its current
     borrowers. After a lease end, the claim queue waits for `claim_grace`
     (default 2 min) first, so that a successor agent in the same folder
     can take the whole scope over (decision 8). This is how a borrowed service such as a database outlives
     the work that started it.

   After a move, the old holder or scope is no longer the owner, and its
   mutations are refused like any other non-owner's. A move never restarts
   anything: instances, URLs and borrows stay valid.

5. **Drop.** When the holder's lease ends, a successor agent in the same
   folder has priority for `claim_grace` (default 2 min, decision 8): if it
   calls taba in that time, it takes the scope over with every service.
   After that, `gc` offers each of the scope's services to its claim queue
   (decision 7): explicit claimants in order, then current borrowers. A service with a live claimant is handed
   over, not stopped. What nobody waits for is dropped once no agent has
   taken the scope over within the grace period (`gc_grace`, decision 2):
   `gc` stops those services and removes their records. Logs are kept for
   `log_retention`. A human can mark a scope `--keep` to opt out of both,
   which is "leaking on purpose" (`std::mem::forget`) and is shown in `ps`.

6. **Lifetimes.** A borrow cannot outlive the instance it borrows, so the
   owner cannot end the instance while it is borrowed. Every borrow records
   the instance id (`pid` + `start_time`, which v0.1 already stores). When
   the owner stops a service that other scopes have borrowed, `stop` lists
   the borrowers and refuses with `E_BORROWED`; `gc` skips it the same way
   (decision 3). `--force --reason` overrides, and is recorded. After a
   forced stop, or a restart under an exclusive borrow, those borrows check
   as `E_DANGLING` with the new URL, if there is one. The one exception to
   the refusal is the holder of an exclusive borrow (rule 3), whose restart
   is exactly what shared borrowers are notified about. A borrow ends when the
   borrower releases it, when it expires, or when the borrower's own lease
   ends, so a dead borrower never pins a service forever.

7. **Borrow checker.** taba checks every mutation against rules 1 to 6 and
   refuses with a structured error that says why and what to do next. The
   optional agent hook extends the check to raw `kill`, `pkill`, `killall`,
   `fuser -k` and `lsof … | xargs kill` before the agent's shell runs them,
   and denies them for other scopes' services and for unmanaged servers
   alike (decision 5).

8. **`unsafe`.** `--force --reason TEXT` is the **only** override of the
   ownership, lease and borrow checks (decision 4). It applies to humans
   too: a human without a scope, or in another scope, who stops a service
   gets `E_NOT_OWNER` like an agent and needs `--force --reason`. Every use
   is appended to `events.jsonl` with the caller's agent, scope, directory
   and reason, and shows in `ps` as a warning on the affected scope. The
   hook does not let an agent's tool call use `--force` on its own: it
   returns "ask", so the human confirms each forced command before it runs
   (decision 9). Without the hook, nothing stops an agent from typing
   `--force`; the skill tells workers never to. This is not foolproof
   (section "Limits").

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
- **A hand-over keeps the same process.** Automatic take-over, `move`,
  `disown` and the claim queue never restart anything, so the new owner gets
  the service in whatever state it is in: its data, its configuration, its
  environment (`TABA_SCOPE` still names the old scope) and its working
  directory, which is the old worktree. If that worktree is deleted, the
  process may break, and taba will not notice until it exits. The new owner
  restarts it under its own scope if it needs a clean instance.
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

### One scope, many services

A scope is a folder, not a service. It holds any number of named services,
and the names are only unique inside the scope:

```text
login-fix-3f9a              (holder claude-code:5c1e9d, lease live)
├── web     :4113  npm run dev
├── api     :4114  cargo run -p api
├── worker  —      npm run worker
└── db      :5433  postgres -D .pg -p 5433
```

Everything that acts on the scope acts on all of its services together:
`taba ps --scope S` lists them, `taba stop --scope S` and `rm --scope S`
stop and remove them, an automatic or explicit hand-off moves all of them to
the new holder, and `gc` hands over or drops all of them (each service
goes to its own claim queue first).
Acting on one service uses `S/N` (or a bare `N` inside the scope).

### A database shared by several worktrees: borrowing

The folder scope does not answer one case on its own: one database used by
several worktrees (S3). Whichever worktree starts it owns it, like any other
service. The others borrow it, as in Rust:

- **Shared borrow (`&`)** to use its URL. Any number at once.
- **Exclusive borrow (`&mut`)** to reset or migrate it. One at a time.
- **Lifetimes.** A borrow cannot outlive the owner, so the owner cannot stop
  the database, and `gc` cannot drop it, while it is borrowed
  (`E_BORROWED`). `--force --reason` overrides, and the borrowers' next
  `borrow --check` returns `E_DANGLING`.
- **Hand-over.** When the owner's work ends but others still borrow the
  database, ownership of that one service passes to a borrower. It happens
  automatically through the claim queue when the owner's lease ends, or
  when the owner runs `taba disown S/db`; `taba move S/db --to <scope>`
  does it explicitly. Nothing restarts.

Example. `wt/login-fix` (scope `login-fix-3f9a`) starts Postgres;
`wt/billing` (scope `billing-91c0`) and `wt/search` (scope `search-0d4e`)
use it.

```sh
# wt/login-fix, agent A: start the database as an ordinary service
taba run --name db --port 5433 --wait -- postgres -D .pg -p {port}

# wt/billing and wt/search: borrow it and use the returned URL
taba borrow login-fix-3f9a/db          # -> url, instance, borrow id

# wt/billing: reset it while nobody else mutates it
taba borrow --mut login-fix-3f9a/db -- npm run db:reset

# wt/login-fix, agent A finishes its task and tries to clean up
taba stop --scope login-fix-3f9a       # web, api stop; db: E_BORROWED (billing-91c0, search-0d4e)

# A gives the database up instead; it goes to the first live claimant,
# here the first borrower
taba disown login-fix-3f9a/db
# billing-91c0 now owns `db` with the same instance and URL; its own borrow
# ends, search-0d4e's borrow stays valid and now points at billing-91c0/db

# Alternatively, wt/search wanted to own it after A and said so in advance:
taba claim login-fix-3f9a/db           # explicit claimants come before borrowers
```

If A's agent is gone before it disowns anything, nothing is lost either.
For `claim_grace` (default 2 min, decision 8) the scope waits for a
successor in `wt/login-fix`; if one calls taba in that time, it takes the
whole scope over, `db` included. Otherwise, at the next `gc` after the
grace (lazy, in any agent's `run` or `whoami`), `db` is handed to the first
live claimant (explicit claimants first, then borrowers in borrow order),
with an event and `W_HANDED_OVER` for the new owner. Only a service that nobody claims or borrows waits for
`gc_grace` and is dropped. `taba move S/db --to <scope>` remains for a human
or coordinator who wants to pick the new owner by hand.

Not in v0.2: Rc/Arc-style shared ownership, where a service belongs to all
its borrowers and is dropped when the last one releases it. It is a possible
later extension; v0.2 keeps exactly one owner and transfers it through the
claim queue or an explicit move.

### Claim queue

A claim says "if this service loses its owner, give it to me" (decision 7).
It is a queue per service, first in, first out:

```sh
taba claim S/N            # join the queue; the claimant is the caller's scope and agent
taba claim --cancel S/N   # leave it
```

- **When it is served.** When the owner's lease ends (agent process gone,
  `lease end`, TTL), at the first `gc` after `claim_grace` (default 2 min,
  decision 8) and before the drop; and at once when the owner runs
  `taba disown S/N` (or `disown --scope S` for all its
  services). An explicit `stop` is not a hand-over: it still refuses while
  the service is borrowed (`E_BORROWED`) and otherwise stops the service
  and discards its claims.
- **Who gets it.** The first entry whose agent is still alive: its
  `TABA_AGENT_PID` process exists, or, without a pid, its scope lease is
  live. Dead claimants are skipped and removed (`claim_skipped`). After the
  explicit claimants come the service's current borrowers, as implicit
  claimants, in the order they borrowed. A claimant whose scope already has
  a service with the same name is skipped for this hand-over and stays
  queued.
- **What happens.** The same as `taba move S/N --to <claimant scope>`:
  the record and log move, the instance and URL stay, the new owner's own
  borrow ends, other borrows point at the new name. The service now lives
  under the claimant scope's lease and TTL. A `service_moved` event with
  `how: claim` or `how: borrower` is recorded, and the new owner sees
  `W_HANDED_OVER` on its next taba call.
- **Nobody waiting.** Only then does the normal grace period and drop
  apply.
- **Same-folder successor first** (decision 8). Automatic scope take-over
  (decision 6) and the claim queue both react to an expired lease, and the
  successor has priority. For `claim_grace` after the lease ended, `gc`
  does not serve the queue. If an agent in the same scope calls taba in
  that time, it takes the whole scope over with every service, and the
  claims stay queued for the next expiry. Only after the grace does `gc`
  hand claimed services to their claimants; a successor that arrives later
  takes over what is left and can borrow the rest back.
- **`keep`** scopes are never handed over or dropped.
- A scope cannot claim its own service (`E_USAGE`). Claiming creates the
  claimant's scope record, as `borrow` does.

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
never set it, except that `human` is now subject to the ownership checks
like everyone else (decision 4).

`TABA_AGENT_PID` matters most for hand-off: it is what lets the next agent
take a scope over as soon as the previous agent's process is gone
(decision 6). Without it, a scope is only released by `lease end` or by its
TTL.

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
├── claims/<scope>/<name>.json        # new: the service's claim queue, in order
├── ops/<scope>/<name>.json           # new: in-flight mutation (operation lock)
├── events.jsonl                      # new: append-only audit log
├── config.toml                       # new, optional: lease_ttl, claim_grace, gc_grace, log_retention, stop_timeout
├── version                           # new: state schema version ("2")
├── btime/                            # Linux: boot-time cache and recovery copies
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
  "service_ttl_secs": null,
  "ready": { "check": "tcp", "ready_at": 1795502013200000 }
}
```

- `instance` is `pid-start_time`. It is what borrows point at. v0.1 already
  has both values; v0.2 only names their combination.
- `command_template` is what idempotence compares when `--port auto` was
  used, because the substituted port differs between runs.
- `service_ttl_secs` is an optional per-service limit
  (`run --service-ttl`), independent of the scope lease.
- `moves` (not shown) lists `service_moved` hand-offs of this one service
  between scopes, as the scope record does for agents.

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
  "lease": {
    "ttl_secs": 14400,
    "ttl_source": "config",
    "renewed_at": "2026-11-24T10:02:11Z",
    "expires_at": "2026-11-24T14:02:11Z"
  },
  "keep": false,
  "created_at": "2026-11-24T09:58:40Z",
  "moves": [
    { "at": "2026-11-24T09:58:40Z", "from": null, "to": "claude-code:5c1e9d", "how": "created" }
  ]
}
```

`ttl_secs` is `null` and `expires_at` is `null` when the effective TTL is
`none`. `ttl_source` is `flag`, `env`, `config` or `default` (see "Lease TTL
configuration"). `moves[].how` is `created`, `give`, `auto_take`,
`take_expired` or `take_forced`.

A scope record is created by the first `run` or `borrow` in that scope (a
borrower needs a lease, because its borrows end with it). It is removed by
`gc` (or by `rm --scope`) once the scope has no services and no borrows.

**Borrow record:**

```json
{
  "id": "b-7k2q",
  "kind": "shared",
  "service": "login-fix-3f9a/db",
  "instance": "40110-1795500000000000",
  "url": "http://localhost:5433",
  "borrower": { "agent": "codex:019a3e", "scope": "billing-91c0" },
  "taken_at": "2026-11-24T10:05:00Z",
  "expires_at": "2026-11-24T14:05:00Z",
  "holder_pid": null
}
```

`kind` is `shared` or `exclusive`. An exclusive borrow taken with
`-- CMD` records taba's own pid and start time as `holder_pid`, and the
borrow ends when that process ends. A borrow also ends when the borrower's
scope lease ends. When the service moves to another scope, `service` is
rewritten; a borrow held by the receiving scope itself is ended, because
the borrower is now the owner.

**Claim queue record** (`claims/<scope>/<name>.json`):

```json
{
  "service": "login-fix-3f9a/db",
  "queue": [
    { "scope": "search-0d4e", "agent": "codex:7b21aa", "pid": 51200, "pid_start_time": 1795501800000000, "at": "2026-11-24T10:20:00Z" }
  ]
}
```

Implicit claimants (borrowers) are not stored here; they are derived from
the borrow records when the queue is served.

**Event** (one JSON object per line in `events.jsonl`):

```json
{"at":"2026-11-24T11:40:02Z","event":"force_stop","agent":"human","caller_scope":null,"cwd":"/Users/me","target":"billing-91c0/web","reason":"stuck worker","instance":"41888-1795503300000000"}
```

Events: `created, moved` (with `how`: `give`, `auto_take`, `take_expired`,
`take_forced`, and the previous holder), `service_moved` (with `how`: `give`, `claim`,
`borrower`, `disown`), `claimed, claim_cancelled, claim_skipped,
claims_discarded, force_*, gc_drop, borrow_broken, lease_ended,
hook_denied` (when the hook reports back), `hook_unchecked, keep_set`. taba never rewrites
or truncates the file. Rotation is out of scope, like log rotation in v0.1.

### Leases: TTL renewed on use (recommended) vs heartbeat

| | Heartbeat | TTL renewed on use (**recommended**) |
| --- | --- | --- |
| How | The holder writes "alive" every N seconds. | The lease has an expiry. Any taba command by the holder in that scope extends it. `taba renew` extends it explicitly. |
| Needs | A resident process or a loop in every agent. | Nothing resident. |
| Fits taba's "no daemon" property | no | yes |
| Crash detection | fast (N seconds) | slow by TTL alone, but fast with `TABA_AGENT_PID` (dead pid = expired lease) and with the SessionEnd hook |
| Failure mode | A busy agent that misses beats loses its servers. | A long-silent agent can lose its lease. Mitigated by a generous, configurable TTL (default 4 h, `none` allowed, decision 2), by `gc` checking the holder pid first, and by the grace period before a drop. |

So a lease is **expired** when any of these is true:

1. The holder's `TABA_AGENT_PID` was recorded and that process (pid with its
   start time) no longer exists.
2. The holder ran `taba lease end` (the SessionEnd hook does this).
3. `now > expires_at` (never, when the effective TTL is `none`).

Renewal: `run`, `stop`, `rm`, `borrow`, `renew` and `whoami` by the holder
renew the lease. `ps` and `logs` renew too, but only when the lease is more
than 60 s old, so read-heavy agents don't write on every call. A `human`
holder has no pid and, by default, TTL `none`, so its lease ends only with
`lease end` (or an explicit TTL).

An expired lease is not dropped at once. For `claim_grace` (default
2 min) only a successor in the same scope can take it over (decision 8):
the next agent that calls taba in it takes it over with every service
(decision 6, "Hand-off" below). After `claim_grace`, `gc` serves the claim
queues; for the rest of `gc_grace` (default 10 min) the scope still waits
for a new holder. After that, `gc` may drop it.

### Lease TTL configuration

The lease TTL is user-configurable; 4 h is only the built-in default
(decision 2). The effective TTL comes from the first of these that is set:

| Order | Source | Example | `ttl_source` |
| --- | --- | --- | --- |
| 1 | `--ttl DURATION` on `run` (or `renew --ttl`) | `--ttl 8h` | `flag` |
| 2 | `$TABA_TTL` | `TABA_TTL=1h` | `env` |
| 3 | `$TABA_HOME/config.toml`, key `lease_ttl` | `lease_ttl = "4h"` | `config` |
| 4 | built-in default | 4 h for agents, `none` for `human` | `default` |

`DURATION` is an integer with a unit (`90s`, `30m`, `4h`, `2d`) or `none`
(no expiry by time; the lease still ends when the holder's process is gone
or on `lease end`). The TTL is resolved when a lease is created or taken
over and whenever `--ttl` is given, and is stored on the scope record with
its source. Later renewals keep the stored TTL. `taba whoami` and
`ps --json` show the effective TTL and where it came from.

The config file is optional, and taba works with zero configuration. All
keys are optional:

```toml
# $TABA_HOME/config.toml
lease_ttl     = "4h"    # default lease TTL; "none" = no expiry by time
claim_grace   = "2m"    # how long a same-folder successor has priority before claim queues are served
gc_grace      = "10m"   # how long an expired scope waits for a new holder before gc may drop it
log_retention = "24h"   # how long logs of dropped services are kept
stop_timeout  = "5s"    # how long gc waits after the graceful stop request before forcing
```

`claim_grace`, `gc_grace`, `log_retention` and `stop_timeout` come only from the config
file or the built-in default (no flag or environment variable in v0.2).
Both graces count from the moment the lease ended. A drop never happens
before claims have been served, so with `claim_grace` longer than
`gc_grace` the drop waits for `claim_grace` too. An invalid
value, in the file or in `$TABA_TTL`, fails the command with `E_CONFIG`
naming the source and key; taba does not fall back silently. An invalid
`--ttl` is `E_USAGE`.

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

Hand-off is mostly automatic (decision 6). An agent that hits its usage
limit is stopped abruptly, so it can rarely run `taba move --to` before it
goes. taba therefore does not depend on the old agent doing anything.

```text
          run (first)            holder alive: others get E_SCOPE_HELD
 (none) ─────────────▶ held(A) ─────────────────────────────────────────┐
                          │                                             │
                          │ A's process gone / lease end / TTL          │ move --to B (by A)
                          ▼                                             │ move --take --force --reason
                      expired(A) ── next taba call by B in the scope ──▶ held(B)
                          │          (auto_take, no command needed)
                          │ nobody within gc_grace
                          ▼
                      gc: Drop (borrowed services are kept)
```

- **Automatic take-over.** When the holder's lease has ended (its
  `TABA_AGENT_PID` process is gone, it ran `lease end`, or its TTL passed),
  the next agent whose call resolves to that scope becomes the holder. This
  happens on any command that would renew a lease (`whoami`, `run`, `stop`,
  `rm`, `borrow`, `renew`, `keep`); read-only `ps` and `logs` only report
  `"takeover": "available"`. The scope keeps every service and running
  instance, the caller gets `W_TOOK_OVER` (previous holder, why the lease
  ended, the services and their URLs), and a `moved` event with
  `how: auto_take` is recorded. Agents call `taba whoami` first, so this
  normally happens before they do anything else.
- **Holder still alive.** If the lease is live, a mutation in that scope by
  another agent fails with `E_SCOPE_HELD`, naming the holder, its pid, when
  it last renewed and when the lease expires. A holder without
  `TABA_AGENT_PID` counts as alive until `lease end` or its TTL; with TTL
  `none`, only an explicit move releases it.
- **Explicit give:** the holder runs `taba move --to AGENT`. Useful for a
  planned hand-off, rarely possible for an agent that is being cut off.
- **Explicit take:** `taba move --take` succeeds without force if the lease
  has ended (`how: take_expired`; the same as automatic take-over, for
  callers that want it explicit). If the lease is live it fails with
  `E_LEASE_LIVE`, and `--force --reason` overrides (`how: take_forced`).
  This is the tool for coordinators and humans.
- A scope move transfers the scope with all its services, and keeps every
  running instance. Borrows by other scopes stay valid, because the instance
  did not change.
- **Moving one service** to another scope: `taba move S/N --to SCOPE`. The
  caller must hold `S`, or `S`'s lease must have ended; otherwise
  `--force --reason`. The receiving scope must exist (it has a scope record,
  which a `run` or `borrow` there creates) and must not already have a
  service named `N` (`E_CONFLICT`). The record moves to
  `services/<SCOPE>/<N>.json` and the log file is renamed alongside it (the
  running process keeps its open file descriptor, so logging continues). The
  instance, pid and URL do not change. The child's environment still says
  `TABA_SCOPE=S`; taba never relies on that value. A `service_moved` event
  is recorded. The claim queue (below) performs the same move
  automatically when the owner's lease ends or the owner runs `disown`.
- The coordinator case: an orchestrator that dispatches worker B into
  worker A's worktree normally does nothing: if A's process has ended, B's
  first `taba whoami` takes the scope over. If A is still alive (for
  example, stuck at a usage-limit prompt), the coordinator or a human runs
  `taba move --scope S --to B --force --reason "A hit usage limit"`. When
  the coordinator is itself a hooked agent, the hook turns that command
  into an "ask": the human confirms it before it runs (decision 9).

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
- **Durations.** `--ttl`, `--service-ttl`, `$TABA_TTL` and the config file
  take `90s`, `30m`, `4h`, `2d` or (for lease TTLs) `none`.
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
  preference. It never suggests `kill` or `--force` to an agent. When only a
  human can resolve the error, the list ends with `{"do": "ask the human",
  ...}`, and the message says what the human can do (for example stop the
  process themselves, or use `--force --reason`).

- **Exit codes.** `0` ok. `1` runtime, state or config error. `2` usage (clap already
  uses 2). `3` refused by the ownership rules or by the state of the service
  (`E_NOT_OWNER`, `E_SCOPE_HELD`, `E_LEASE_LIVE`, `E_LOCKED`, `E_BUSY`,
  `E_BORROWED`, `E_NO_CLAIMANT`, `E_CONFLICT`, `E_STILL_RUNNING`). `4` the
  service could not be started or got no port (`E_PORT_IN_USE`,
  `E_NO_FREE_PORT`, `E_START_FAILED`, `E_NOT_READY`). `5` not found
  (`E_NOT_FOUND`, `E_DANGLING`).

### Error codes

| Code | Exit | Meaning | Typical `next` |
| --- | --- | --- | --- |
| `E_USAGE` | 2 | Bad flags or arguments. For an argument error, the full message names the argument; `details` has `arguments` (the offending flags), and if any `value`, `subcommand` and `usage`. | the subcommand's help, for example `taba run --help` |
| `E_NO_SCOPE` | 2 | No scope from `--scope`, `$TABA_SCOPE` or a git worktree. | pass `--scope`, or `cd` into the worktree |
| `E_INVALID_IDENT` | 2 | Scope or name fails `validate_ident`. | — |
| `E_NOT_FOUND` | 5 | No such service, scope or borrow. | `taba ps --json` |
| `E_CONFLICT` | 3 | `run` with the same name but a different command, directory or port while the old one runs (v0.1's "already running with a different command"). | `taba stop S/N` then re-run, or use another `--name` |
| `E_PORT_IN_USE` | 4 | Port held by a taba service or an unmanaged process (details say which). taba never offers to kill the holder, managed or not (decision 5). | `--port auto`; `taba borrow` if it is a taba server you wanted; for an unmanaged holder, ask the human, who can stop it themselves |
| `E_PORT_CLAIMED` | 4 | Port reserved by another taba service that is starting or running. | `--port auto` |
| `E_NO_FREE_PORT` | 4 | `--port auto` found nothing in its range. | `--port auto:<other start>`; `taba gc` |
| `E_START_FAILED` | 4 | The process exited before it was ready. `details.log_tail` has the last 20 log lines. | `taba logs S/N` |
| `E_NOT_READY` | 4 | `--wait` timed out. The process is still running (and is reported). | `taba logs S/N`; `taba stop S/N` |
| `E_NOT_OWNER` | 3 | Mutation of a service in another scope than the caller's, by an agent or a human (decision 4). | `taba borrow S/N`; ask the holder; ask the human (who can use `--force --reason`) |
| `E_SCOPE_HELD` | 3 | Mutation in the caller's own scope while another agent holds it under a live lease (holder process alive). Names the holder, its pid, last renewal and expiry. | wait: the scope passes automatically once the holder's process ends; ask the coordinator or the human |
| `E_LEASE_LIVE` | 3 | `move --take` while the lease is live. | wait until the holder ends or `expires_at`; ask the human |
| `E_LOCKED` | 3 | Another agent holds an exclusive borrow. | retry after `expires_at`, or `--wait-lock SECS` |
| `E_BUSY` | 3 | Another `stop` or replacing `run` is in flight. | retry; `taba ps S/N` |
| `E_BORROWED` | 3 | `stop` or `rm` of a service with live borrows from other scopes (decision 3). Details list the borrowers and the claim queue. | `taba disown S/N` (hands it to the first claimant or borrower); `taba move S/N --to <borrower scope>`; ask the human (`--force --reason`) |
| `E_NO_CLAIMANT` | 3 | `disown` of a service with no live claimant and no borrower. | `taba stop S/N` (nothing depends on it) |
| `E_DANGLING` | 5 | Borrow's instance is gone (stopped, replaced, or the port now belongs to something else). | `taba borrow S/N` again (new URL in details) |
| `E_STILL_RUNNING` | 3 | `rm` of a running service (v0.1 behaviour). | `taba stop S/N` |
| `E_UNVERIFIABLE` | 1 | The recorded pid is alive but is no longer the process taba started (start time differs). v0.1 reports this case as `exited` and never signals it; v0.2 keeps the no-signal rule and names the case. | `taba ps --all` |
| `E_SIGNAL_FAILED` | 1 | `kill` on the process group failed. | — |
| `E_FORCE_NEEDS_REASON` | 2 | `--force` without `--reason`. | — |
| `E_CONFIG` | 1 | `$TABA_HOME/config.toml` or `$TABA_TTL` is unreadable or has an invalid value. Details name the source and key. | fix or remove the value |
| `E_STATE` | 1 | State directory unreadable, or the schema is too new. An empty or damaged service or scope record (a missing one just means "no record") gives `details.reason: "unreadable"` with the `path`; taba never replaces or rewrites it on its own. | `taba rm S/N --force --reason …` (unreadable service record, and its log) or `taba rm --scope S --force --reason …` (the scope's unreadable records and its unreadable scope record) |

Warnings (`warnings[]`, never fatal): `W_BRANCH_CHANGED`,
`W_LEASE_RENEWED`, `W_TOOK_OVER` (this call took over a scope whose holder
had ended; lists the previous holder and the services), `W_BORROW_BROKEN`
(a borrower's instance was replaced or force-stopped), `W_HANDED_OVER` (this scope now
owns a service through the claim queue; names the service, previous owner
and URL), `W_OWNER_GONE` (the owner of a service this scope borrows has
ended and the hand-over has not happened yet, for example because every
claimant has a name conflict; includes the `taba move S/N --to <this
scope>` command), `W_FORCED` (a `--force`
override happened in this scope), `W_UNMANAGED_PORT` (an unmanaged process
listens on a port in this worktree), `W_STATE_SKIPPED` (a listing such
as `ps` or `gc` skipped an empty or damaged service or scope record; names
the record and its file).

### `taba whoami` (new)

```sh
taba whoami [--json]
```

Prints the resolved scope, its source, the agent, the lease (with the
effective TTL and where it came from) and the scope's services. Agents call
it first. It renews the lease if the caller is the holder, and takes the
scope over if the holder has ended (decision 6). Example: the holder is
still alive.

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
  "holder_pid": 38811,
  "lease": {
    "state": "live",
    "ttl": "4h",
    "ttl_source": "config",
    "config_path": "/Users/me/.taba/config.toml",
    "expires_at": "2026-11-24T14:02:11Z"
  },
  "takeover": null,
  "services": ["login-fix-3f9a/web", "login-fix-3f9a/db"],
  "next": [{ "do": "the scope passes to you automatically when the holder's process ends; until then ask the coordinator or the human" }]
}
```

When the call takes the scope over, `holder` is the caller,
`you_hold_scope` is `true`, `takeover` is
`{ "from": "claude-code:5c1e9d", "why": "holder_pid_gone" }` (or
`lease_ended`, `ttl_expired`), and `warnings` has `W_TOOK_OVER`. `ttl` is
`"none"` and `expires_at` is `null` for a lease without expiry.

Errors: `E_CONFIG, E_STATE`. Without a scope it still succeeds, with
`"scope": null`.

### `taba run` (extended)

```sh
taba run [--scope S] --name N
         [--port P | --port auto | --port auto:START]
         [--wait[=SECS]] [--ready tcp | http:PATH | log:REGEX]
         [--ttl DURATION] [--service-ttl DURATION] [--json] -- CMD...
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
- **`--ttl DURATION`** sets the scope's **lease** TTL (`8h`, `none`), and
  overrides `$TABA_TTL`, the config file and the default (decision 2). It
  is recorded with `ttl_source: "flag"` and applies to the whole scope.
- **`--service-ttl DURATION`** (`30m`, `2h`) stops this one service at the
  next `gc` after that long without a renewal, even if the scope's lease is
  live. Use it for one-off previews. (Called `--ttl` in earlier drafts.)
- **Ownership:** `run` in a scope held by another agent under a live lease
  fails with `E_SCOPE_HELD`. If the holder's lease has ended, `run` takes
  the scope over first (decision 6). The first `run` in a scope creates the
  scope record with the caller as holder. A replacing `run` (exited record,
  or `E_CONFLICT` resolved by stop) takes the operation lock.
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
E_CONFIG, E_STATE`.

### `taba ps` (extended) and `taba ps --all` (new)

```sh
taba ps [--scope S] [--running] [--json]
taba ps --all [--json]
```

`ps` keeps every v0.1 JSON field (`service, scope, name, status, url, port,
directory, command, pid, pgid, start_time, log_path`) and adds `instance`,
`holder`, `lease`, `keep`, `borrows` (count by kind) and `locked_by`.
`lease` is an object with `state` (`live` / `expired` / `none`), `ttl`,
`ttl_source` and `expires_at`, the same fields `whoami` shows, so the
effective TTL and its source are visible per scope. Each service also has
`claims`: the queue in hand-over order, explicit claimants first, then
borrowers, for example `[{"scope": "search-0d4e", "agent": "codex:7b21aa",
"kind": "explicit", "alive": true, "at": "…"}, {"scope": "billing-91c0",
"kind": "borrower", "alive": true}]`. The text table shows the next live
claimant in a `NEXT` column. When the caller could take the scope over, the
output also has `"takeover": "available"`. `status` gains `starting` and `stopping`, which
appear while an operation lock is held. The text table gains an `OWNER`
column (`holder` with a mark when the lease has expired).

`ps --all` leaves services, records and leases unchanged. It answers "what is
listening on this machine, and whose is it?". It lists every TCP socket in LISTEN state owned by the current
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
      "service": "login-fix-3f9a/web", "scope": "login-fix-3f9a", "holder": "claude-code:5c1e9d",
      "lease": { "state": "live", "ttl": "4h", "ttl_source": "default", "expires_at": "2026-11-24T14:02:11Z" }
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

#### `taba ps --watch --json` (v0.2.2, issue #14)

```sh
taba ps --watch --json [--scope S] [--running] [--interval DURATION]
```

A stream for other UIs (`taba ui`, Orca): newline-delimited JSON, one full
`ps --json` document per line (`{"ok": true, "services": [...], "warnings":
[]}`, compact). The first line is printed at once, then a new line only
when the `services` rows differ from the last line printed (status, port,
PID, lease, borrows, claims, a service added or removed). Full snapshots
instead of diffs keep consumers simple; the state is small.

- State is re-read every `--interval` (default 1 s, minimum 250 ms; `SECS`
  or a number with `ms` / `s`).
- Read-only: the rows come from the same code as `ps`, but without `ps`'s
  lease peek, so a watcher never renews a lease, drains notices or writes
  anything. There is therefore no `takeover` field and `warnings` is empty.
- Ends with exit 0 on SIGINT / Ctrl+C (Windows: Ctrl+C or Ctrl+Break, via a
  console control handler) and when stdout is closed: a failed write ends
  it, and an idle stream checks a pipe or socket stdout every 50 ms for a
  vanished reader without writing to it (Linux: `poll` `POLLERR` /
  `POLLHUP`; macOS: kqueue `EVFILT_WRITE` `EV_EOF`; Windows:
  `NtQueryInformationFile` pipe state `FILE_PIPE_CLOSING_STATE`).
- An error (for example `E_STATE`) is printed as one `{"ok": false, ...}`
  line and ends the stream with that error's exit status.
- `--watch` without `--json` (or `TABA_OUTPUT=json`) is rejected with
  `E_USAGE` for now; `--watch` with `--all` is a usage error.

#### `taba ui` (v0.2.3, issue #15)

A keyboard-driven terminal UI in the same binary (ratatui with the crossterm
backend: macOS, Linux and Windows terminals). Design and decisions: issue
#10.

- **Data:** the same `ps_snapshot` code as `ps` and `ps --watch`, called
  in-process once per second; like `--watch` it skips `ps`'s lease peek,
  so the UI never renews a lease or writes state on its own.
- **Tree:** every scope with its services; the caller's scope (resolved as
  for any command) first and unfolded, other scopes folded. ←/→/Space fold
  and unfold (view only), ↑/↓ move, `/` filters by scope or service name
  (unfolding scopes with a matching service), `l` follows a log like
  `logs -f`, `q` / Ctrl+C quit and leave every service running. The
  terminal is restored on exit and on panic.
- **`x`:** after a one-key confirmation (`y`), a running service is stopped
  with the body of `taba stop SCOPE/NAME` (`stop_services`), an exited one
  removed with the body of `taba rm SCOPE/NAME` (`rm_services`), and a scope
  line stops each of its services as `SCOPE/NAME` (not `stop --scope`, which
  would act *as* that scope). The caller is the UI's own, never forced, so
  `E_NOT_OWNER`, `E_BORROWED`, `E_LOCKED`, `E_SCOPE_HELD`, `E_BUSY` and
  `E_STILL_RUNNING` refuse exactly as on the command line; the refused row
  shows the CLI's message. Actions run on a worker thread so the view keeps
  refreshing during a stop's timeout (5 s, the CLI default).
- **Header:** the logo (`docs/logo/taba-icon.svg`) in block characters,
  brackets `#FF5533` and dots `#FFE0BF`; 24-bit color when the terminal
  advertises it, else the nearest 256-color values (203 and 223), none with
  `NO_COLOR` or `TERM=dumb`; below 40 columns a one-line `[⋮] taba vX.Y.Z`.
- **Colors:** restrained and theme-adaptive. Orange only in the logo and
  the `(current)` mark; status dots in ANSI green / yellow / bright black
  (the terminal theme's own), status words default (exited: dim); scope
  names bold default, counts and notes dim, errors ANSI red. The cursor row
  is reverse video across the full width with all colors removed, so no
  cell is filled with a status color.
- **Tests:** the state and key handling (`ui/app.rs`) and the drawing
  (`ui/view.rs`, rendered to ratatui's `TestBackend` at fixed sizes) are
  unit-tested, and the `x` rules run against a temporary state directory
  with real processes. Raw-mode input and the real terminal are not
  automated.

### `taba logs`

```sh
taba logs S/N [--tail N] [-f] [--json]  # or a bare name in the caller's scope
```

Text output is the raw log. `--tail N` limits the initial output to the last
`N` lines, counting an unterminated final line. `-f` then streams appended
bytes until the service exits. `--tail 0 -f` starts at the end of the file.
Logs are readable by any scope (shared borrow without a record).

With `--json` (or `TABA_OUTPUT=json`), `log` contains at most the last
16 KiB of source data, subject to any smaller `--tail` limit. The byte cap
can start the result mid-line. `bytes_total` is the log size in bytes at the
start of the read, and `truncated` is true if either limit omitted earlier
data. Errors use the JSON envelope. `-f` conflicts with `--json` and ignores
`TABA_OUTPUT=json`.

Errors: `E_NOT_FOUND, E_INVALID_IDENT, E_NO_SCOPE` (bare `NAME` without a
scope), `E_STATE`.

### `taba stop` (extended)

```sh
taba stop S/N | N | --scope S
          [--timeout SECS] [--force --reason TEXT] [--json]
```

v0.1 rules stay: verify identity before signalling, SIGTERM then SIGKILL
after `--timeout`, never signal unmanaged processes (not even with
`--force`: taba has no command that kills a process it did not start,
decision 5), never delete files.
`stop` with no target still needs an explicit `--scope`, even when the scope
could be derived.

New:

- **Cross-scope refusal.** The caller must hold the service's scope (or hold
  an exclusive borrow on that service). Otherwise: `E_NOT_OWNER` for a
  service in another scope, `E_SCOPE_HELD` in the caller's own scope while
  another agent holds it. This applies to `human` callers too, including a
  human with no resolvable scope (decision 4).
- **Expired lease.** If the target scope's lease has expired, a caller in
  that same scope takes it over first (decision 6) and then stops as the
  holder. A caller in another scope may stop it without `--force`: that is
  Drop, done early, and it is recorded as `gc_drop`.
- **Lifetimes.** If other scopes hold live borrows: `E_BORROWED` with the
  borrowers and a `next` that suggests `taba disown S/N` (hand it to the
  first claimant or borrower, decision 7) or `taba move S/N --to <borrower
  scope>`. Claims alone do not block `stop`; a stopped service's claims are
  discarded (`claims_discarded`). This also applies to expired-lease stops. Only `--force --reason`
  stops a borrowed service; the borrows are then marked broken, and the
  borrowers get `W_BORROW_BROKEN` on their next taba call and `E_DANGLING`
  from `borrow --check`. A caller that holds the exclusive borrow on the
  service is not refused for shared borrows (rule 3); those borrowers see
  `E_DANGLING` with the new instance.
- **Operation lock:** concurrent stops or replaces of one service give
  `E_BUSY` instead of double signalling.
- **`--force --reason`** is the only override (decision 4). It skips the
  ownership, lease and borrow checks, never the identity check. It is
  recorded.

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

Errors: `E_NOT_FOUND, E_NO_SCOPE, E_NOT_OWNER, E_SCOPE_HELD, E_BORROWED,
E_BUSY, E_LOCKED, E_UNVERIFIABLE, E_SIGNAL_FAILED, E_FORCE_NEEDS_REASON`.

### `taba rm` (extended)

Same syntax as v0.1 plus `--force --reason`. Same ownership rule as `stop`.
`E_STILL_RUNNING` stays as v0.1's refusal. `rm --scope S` also deletes the
scope record when nothing is left.

Errors: `E_NOT_FOUND, E_NOT_OWNER, E_SCOPE_HELD, E_STILL_RUNNING, E_BUSY,
E_STATE`.

### `taba borrow` and `taba release` (new)

```sh
taba borrow S/N [--ttl DURATION] [--json]                 # shared (&)
taba borrow --mut S/N [--ttl DURATION] [--wait-lock SECS] [--json]   # exclusive (&mut), until release
taba borrow --mut S/N [--wait-lock SECS] -- CMD...         # exclusive for the length of CMD
taba borrow --check BORROW_ID [--json]
taba release BORROW_ID | S/N [--json]
```

A shared borrow returns the URL and instance, and registers the borrower so
that the owner sees it and cannot stop the service while it lasts. The
default TTL is the borrower's scope lease, and the borrow ends with that
lease. An exclusive borrow takes the per-service lock. Holding it lets the
borrower `stop` and re-`run` that one service even though the borrower does
not own its scope: it is the sanctioned way for worker C to reset or
restart the database another worktree owns in S3. Other shared borrowers
then see `E_DANGLING` with the new instance. The `-- CMD` form runs `CMD` in the foreground while holding
the lock, sets `TABA_BORROWED_URL` in its environment, and releases the lock
when `CMD` exits, also on Ctrl-C.

```json
{
  "ok": true,
  "borrow": "b-7k2q",
  "kind": "shared",
  "service": "login-fix-3f9a/db",
  "url": "http://localhost:5433",
  "port": 5433,
  "instance": "40110-1795500000000000",
  "expires_at": "2026-11-24T14:05:00Z",
  "other_borrows": { "shared": 2, "exclusive": null }
}
```

`--check` returns the same shape if the instance is still running, otherwise
`E_DANGLING` with `details.current` (the new instance and URL, if the service
was restarted). If the service moved to another scope, `service` shows the
new `S/N` and the borrow stays valid. If the owner's lease ended more than
`claim_grace` ago, the check serves the claim queue first; if that handed the service to the
caller, the result carries `W_HANDED_OVER`, and if no hand-over was
possible, `W_OWNER_GONE`.

Errors: `E_NOT_FOUND, E_LOCKED` (exclusive held elsewhere, with holder and
expiry), `E_BUSY, E_DANGLING, E_NO_SCOPE` (a borrower needs a scope).

### `taba move` (new)

```sh
taba move [--scope S] --to AGENT [--json]                 # give the scope: caller must hold S
taba move [--scope S] --take [--force --reason TEXT] [--json]   # take the scope: lease must have ended, or force
taba move S/N --to SCOPE [--force --reason TEXT] [--json] # move one service to another scope
```

Taking a scope over normally needs no command: it happens on the next taba
call after the holder ends (section 3, "Hand-off"). `move` is the explicit
tool for coordinators and humans, and for a planned hand-off.

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

`how` is `give`, `take_expired` or `take_forced`.

Moving one service (`S/N --to SCOPE`) is allowed for the holder of `S`, or
for anyone once `S`'s lease has ended; otherwise it needs `--force --reason`.
It returns the service's old and new name, its unchanged instance and URL,
and the borrows that were rewritten or ended:

```json
{
  "ok": true,
  "service": "billing-91c0/db",
  "from": "login-fix-3f9a/db",
  "how": "give",
  "instance": "40110-1795500000000000",
  "url": "http://localhost:5433",
  "borrows": { "rewritten": ["b-2c8d"], "ended": ["b-7k2q"] }
}
```

Errors: `E_NO_SCOPE, E_NOT_FOUND` (service, or receiving scope with no
record), `E_NOT_OWNER` (give by a non-holder), `E_LEASE_LIVE`, `E_CONFLICT`
(the receiving scope already has a service with that name), `E_BUSY`,
`E_FORCE_NEEDS_REASON`.

### `taba claim` and `taba disown` (new)

```sh
taba claim S/N [--json]                # join the service's claim queue
taba claim --cancel S/N [--json]       # leave it
taba disown S/N | --scope S [--json]   # owner gives the service up to its claim queue
```

`claim` returns the caller's position and the queue as `ps --json` shows
it. It is refused with `E_USAGE` for a service of the caller's own scope,
and with `E_CONFLICT` if the caller's scope already has a service with that
name. Claiming twice is idempotent.

`disown` is the owner's explicit hand-over (decision 7). It serves the
queue right away (explicit claimants, then borrowers) and returns the same
object as `move S/N --to SCOPE`, with `how: "disown"` and the claimants
skipped. If nobody live is waiting, it fails with `E_NO_CLAIMANT` and
changes nothing. `disown --scope S` does this for every service of the
scope and reports each one, like `stop --scope`.

Errors: `E_NO_SCOPE, E_NOT_FOUND, E_USAGE, E_CONFLICT, E_NOT_OWNER`
(`disown` by a non-holder), `E_SCOPE_HELD, E_NO_CLAIMANT, E_BUSY`.

### `taba renew`, `taba lease end`, `taba keep` (new)

```sh
taba renew [--scope S] [--ttl DURATION] [--json]
taba lease end [--scope S | --agent A] [--json]     # used by the SessionEnd hook
taba keep [--scope S] [--off] [--force --reason TEXT] [--json]
```

`renew` returns the new lease. `renew --ttl` changes the scope's lease TTL
(`ttl_source: "flag"`), like `run --ttl`. `lease end` marks every lease held by the
agent as expired right away, and does nothing else (it must fit the
SessionEnd hook's 1.5 s budget, section 5). `keep` pins a scope so `gc` never
drops it. Agents may keep their own scope only with `--reason`, which is
recorded and shown in `ps`, so a human can see why something was left
running.

Errors: `E_NO_SCOPE, E_NOT_OWNER, E_SCOPE_HELD, E_FORCE_NEEDS_REASON,
E_CONFIG`.

### `taba gc` (new)

```sh
taba gc [--dry-run] [--json]
```

For every scope whose lease has ended and which is not `keep`, first serve
each service's claim queue (decision 7): hand it to the first live explicit
claimant, else to the first live borrower, as described in section 3,
"Claim queue". This happens once the lease ended more than `claim_grace`
ago (default 2 min), so that a successor in the same folder can take the
scope over first (decision 8). Then, once the lease ended more than
`gc_grace` ago
(default 10 min, decision 2): stop the services nobody claimed (with the
identity check), delete their records, end the borrows the scope itself
holds, and remove the scope record. A borrowed service is never stopped by
`gc` (decision 3); if no borrower can take it (for example every one has a
name conflict), it is listed under `kept` with reason `borrowed` and its
borrowers get `W_OWNER_GONE`. Also: end borrows whose borrower lease has
ended, remove claims whose claimant agent is gone, drop services past their
own `--service-ttl`, delete borrow and operation records whose holder
process is gone, and delete logs of dropped services after `log_retention`
(default 24 h). Every hand-over and every drop is an event.

`run` and `whoami` perform a **bounded lazy gc**, at most 5 services per
call: claim queues of scopes whose lease ended more than `claim_grace` ago
are served, and scopes whose lease ended more than `gc_grace` ago are
dropped (in `run`, before port
selection). A drop stops the service as `taba stop` does: SIGTERM (Windows:
Ctrl+Break), then SIGKILL (Windows: TerminateJobObject) after
`stop_timeout` (default 5 s). `taba gc` waits out that timeout. The lazy
pass inside `run` and `whoami` waits at most 1 s per service so that an
unrelated command is not held up. It leaves a slower service running, records
when it asked (`stop_requested_at` in the service record), and forces it only
when a later cleanup finds `stop_timeout` has passed since then. The
`gc_drop` event records the signal that stopped the service (`TERM`, `KILL`,
or `null` if it had already exited). That
releases ports leaked by crashed agents without any daemon. No launchd job
in v0.2.

```json
{
  "ok": true,
  "dry_run": false,
  "handed_over": [
    { "service": "login-fix-3f9a/db", "to": "billing-91c0/db", "how": "borrower", "skipped": [{ "scope": "search-0d4e", "why": "agent_gone" }] }
  ],
  "dropped": [
    { "scope": "old-task-77aa", "reason": "holder_pid_gone", "services": ["old-task-77aa/web"], "ended_borrows": ["b-1x9m"] }
  ],
  "kept": [
    { "scope": "demo-0c21", "reason": "keep: human review tomorrow" }
  ]
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
tasks. It is not needed for shared infrastructure: a database used by
several worktrees stays in the scope that started it and is borrowed
(section 3).

### Skill changes (`skills/taba/SKILL.md`)

Rewrite the skill around a short procedure, keep the v0.1 rules, and add:

1. **First call:** `TABA_OUTPUT=json taba whoami`. If the previous agent in
   this worktree has ended, this call takes its services over (`takeover`,
   `W_TOOK_OVER`): use them, don't start duplicates. If `you_hold_scope` is
   false because the holder is still alive, follow `next` (wait or ask).
2. **Start:** `taba run --name web --port auto --wait -- <cmd with {port}>`,
   and read `url` from the JSON. Never invent a port. Never pass the port
   only to taba.
3. **Errors:** read `error.code` and do the first item of `error.next`. Never
   respond to `E_PORT_IN_USE`, `E_NOT_OWNER`, `E_SCOPE_HELD`, `E_BORROWED`
   or `E_LOCKED` with `kill`, `pkill`, `lsof | xargs kill` or `--force`,
   whether the holder is a taba service or an unmanaged process. Pick another
   port with `--port auto`, or ask the human, who can stop it themselves.
4. **Using someone else's server:** `taba borrow S/N` and use the returned
   URL. Before a long test run against it, `taba borrow --check ID`.
5. **Restarting or resetting a server or DB another worktree owns:**
   `taba borrow --mut S/N -- <migration or reset command>`.
6. **Hand-off and usage limits:** taking over is automatic (step 1), so you
   don't need to do anything to be replaced. In status reports, list each
   service's name, URL and status. Run `taba move --to <next agent>` only if
   you know the next agent and still have time.
7. **Finish:** `taba stop --scope <scope>` and `taba rm --scope <scope>`,
   unless the human needs a server. In that case use
   `taba keep --reason "<why>"` and say so in the hand-off. If `stop`
   returns `E_BORROWED` for a service, run `taba disown S/N` (from
   `error.next`) to hand it to a borrower instead of forcing it.
9. **Depending on another task's service beyond its owner's life:**
   `taba claim S/N`. If the owner ends, the service becomes yours without a
   restart (`W_HANDED_OVER`); it is the same process, in whatever state the
   owner left it.
8. **Remove** the v0.1 advice to choose scope names by hand. The folder is
   the scope; shared databases are borrowed, not put in a separate scope.

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
   `||` and `|`. Drop redirections, parse `$(…)` and backticks (without
   running them), and strip `NAME=value` prefixes, shell keywords and
   wrappers (`sudo`, `doas`, `command`, `env`, `exec`, `nohup`, `nice`,
   `timeout N`, `stdbuf`, `setsid`, `time`) to reach the program that runs.
   Find these patterns:
   - `kill [-SIG | -s SIG] PID…` and `kill -- -PGID`
   - `kill $(lsof -t -i…)`, `kill $(pgrep …)`, and `lsof -t -i… | xargs
     [OPTIONS] kill` (address forms `:P`, `tcp:P`, `-iTCP:P`, comma lists
     and short ranges, with `-sTCP:LISTEN`, `-n`, `-P`)
   - `fuser -k PORT/tcp` and `kill-port PORT` (directly or through `npx`,
     `pnpx`, `bunx`, `pnpm dlx`, `yarn dlx`)
   - `pkill [-SIG | -s SIG] [-f] PATTERN` and `killall [-SIG] NAME`
   - `taba … --force` and `taba move … --take --force`
   - `taba` commands that act as another agent: `--agent X`, a
     `TABA_AGENT=X` prefix or assignment, `export`, `unset` or `env -i`/`-u`,
     when X is not the hook's own session identity
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
   | taba service in a scope the caller holds, not borrowed by another scope | **allow** (the skill already says to prefer `taba stop`) |
   | taba service in a scope the caller holds, borrowed by another scope | **deny**, with `next`: `taba stop S/N` (which explains `E_BORROWED` and the move) |
   | taba service in another scope, lease live | **deny** |
   | taba service in another scope, lease expired | **deny**, with `next`: `taba stop S/N` (which is allowed for expired leases that nobody borrows) |
   | unmanaged process listening on a TCP port, or whose cwd is in another git worktree | **deny** (decision 5), with `next`: `--port auto`, or ask the human, who can stop it themselves |
   | any other unmanaged process | **allow** |
   | `taba … --force` | **ask** (decision 9): the human confirms or rejects the forced command in the agent's permission prompt; the reason names the target and the `--reason` text |
   | mutating `taba` command acting as another agent (`whoami`, `ps`, `logs` excepted) | **ask**, like `--force` |
   | command not parseable (variables, `eval`, scripts, `sh -c "$X"`), or a `kill`, `pkill`, `killall`, `fuser`, `kill-port` or `xargs kill` whose targets can't all be resolved (unknown options, PIDs from files or other commands, `lsof` without a numeric port) | **allow**, and record `hook_unchecked` in `events.jsonl` |

5. Explain. A deny uses `hookSpecificOutput.permissionDecision: "deny"` and
   a `permissionDecisionReason`, which Claude sees. The `--force` case uses
   `permissionDecision: "ask"` instead (decision 9), so Claude Code shows the
   human a confirmation prompt with the same kind of reason; the forced
   command is recorded by taba as usual once it runs. The reason states the
   fact, the rule and the next step, for example:

   ```text
   taba: blocked `lsof -ti:5173 | xargs kill -9`.
   Port 5173 is billing-91c0/web, owned by another task (holder codex:019a3e,
   worktree ~/wt/billing, lease live until 14:02). Killing it would break that
   task's server.
   Instead: taba run --name web --port auto --wait -- npm run dev -- --port {port} --strictPort
   If you really need that server stopped, ask the human.
   ```

   For an unmanaged holder the reason says so and gives the same two ways
   forward:

   ```text
   taba: blocked `kill 39001`.
   Process 39001 (node …/vite, cwd ~/wt/billing, branch feat/billing) listens
   on port 5173. taba did not start it, so it can't tell whose work it is, and
   killing it might break another task.
   Instead: taba run --name web --port auto --wait -- npm run dev -- --port {port} --strictPort
   If it really has to go, ask the human; they can stop it themselves.
   ```

   This also denies an unmanaged server that the agent started itself
   (`npm run dev &`), because taba can't tell who started it. The skill
   says to start servers through taba; a harness's own background tasks
   are stopped with the harness's tool (Claude Code's `/tasks`), which the
   Bash matcher does not see.

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
uses the identity `codex:<session_id>`. The
[Codex hook reference](https://developers.openai.com/codex/hooks#pretooluse)
explicitly says `permissionDecision: "ask"` is unsupported for PreToolUse.
The Codex hook thus denies `--force` and tells the agent to ask a human to
run it separately. Claude Code asks for every hooked agent; taba cannot
identify whether a tool call came from a coordinator or a worker. Codex requires non-managed
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
- **Platform gaps.** taba runs on macOS, Linux and Windows (section 8,
  "Platform notes"). `ps --all` (M2) lists listeners through
  `proc::listeners`, which exists on all three; the working directory of an
  unmanaged process needs `PROC_PIDVNODEPATHINFO` (macOS), `/proc/PID/cwd`
  (Linux) or a PEB read (Windows), still to be written.
- **Security between users or against a malicious agent.** Identity is
  claimed, not proven (section 2, "Limits").
- **Proxies and stable hostnames** (`web.login-fix.localhost`). These would
  remove port conflicts at the root, but they are a separate tool.
- **Database-level locking.** `borrow --mut` is an advisory lock that agents
  take before touching a shared DB. taba does not understand schemas,
  connections or transactions.
- **Log rotation.** Unchanged from v0.1. (A TUI, `taba ui`, came later in
  v0.2.3; see [the TUI section](#taba-ui-v023-issue-15).)

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
   (or the next free port) and starts. `--port 8001` returns
   `E_PORT_IN_USE` naming the unmanaged pid, and its `next` contains
   `--port auto` and "ask the human" and no `kill`.
5. `--wait=2 -- sh -c 'sleep 5; exec python3 -m http.server {port}'` returns
   `E_NOT_READY` with exit 4, and the process is still in `ps`. `-- false`
   returns `E_START_FAILED` with `details.log_tail`.
6. In `wt-b`, `taba stop <wt-a scope>/web` returns `E_NOT_OWNER` with exit 3,
   and the server still answers. With `--force --reason test` it stops, and
   `events.jsonl` has a `force_stop` line. The same holds for a human: with
   `TABA_AGENT` unset and the current directory outside any repo,
   `taba stop <wt-a scope>/web` returns `E_NOT_OWNER`, and only
   `--force --reason` stops it.
7. Two simultaneous `taba stop` calls on one service in `wt-a`: one gets
   `stopped`, the other `E_BUSY` or `not_running`, and SIGTERM is sent once.
8. With `TABA_SCOPE` and `--scope` unset outside any git repo, `run` returns
   `E_NO_SCOPE`. With `TABA_SCOPE=x` set, v0.1 behaviour is unchanged.
9. A v0.1 state directory (records without new fields) still lists, stops
   and removes correctly.
10. One scope, many services: in `wt-a`, `web`, `api` and `worker` (no
    port) all run under the derived scope; `ps --scope` lists all three, and
    `stop --scope` stops all three.

#### M1 implementation notes

Where the design left a choice open, M1 does this:

- `--port auto:START` tries 1000 ports, `START` to `START + 999` (capped at
  65535). `{port}` is substituted for explicit ports too, and `PORT` is set
  whenever there is a port. The "app may ignore `PORT`" warning is
  `W_PORT_NOT_IN_COMMAND` and is given for `--port auto` only.
- `--ready` requires `--wait`; `--ready tcp` or `http:` without `--port` is
  `E_USAGE`.
- `run` spawns while it holds the global lock (spawning is fast) and records
  the service with its pid before releasing it, so no `starting` record
  without a pid is ever written. The ready wait runs under the operation
  lock (`op: start` for a new service, `replace` for an exited one), and
  `ps` shows `starting` meanwhile.
- `--scope` names the caller, as everywhere: `stop S/N --scope X` acts as
  scope `X`, and `stop --scope S` (no target) acts as `S` and so always
  owns its targets. Until M3 adds leases, nothing tells a live holder from
  anyone else naming the same scope.
- `rm` gets the same ownership rule as `stop` (section 4, `taba rm`), with
  `force_rm` events, although M1 lists only `stop`.
- `--force` is recorded (`force_stop`, `force_rm`) whenever it is given and
  the stop or removal goes ahead, also by the owner; `overrode` lists the
  checks it skipped.
- `E_UNVERIFIABLE` is reported as result `not_running` with
  `"unverifiable": true` instead of an error, so that `ps` (which shows
  `exited`) and `stop` agree and v0.1 behaviour (exit 0) is kept.
- `whoami` has no `holder`, `lease` or `takeover` yet (M3), and
  `W_BRANCH_CHANGED` needs the scope record of M3. `ps --json` is still a
  bare array (M2) with `instance` added. `--agent` exists on `run`, `stop`,
  `rm` and `whoami`.
- The v0.1 acceptance tests that stopped or removed services from a
  directory without a scope now pass `--scope`: decision 4 deliberately
  changes that behaviour, so "keeps every v0.1 test passing" holds only
  with that change.

#### Platform notes (Windows and Linux ports)

taba was macOS-only through M1. The process code now sits behind one
module, `src/proc/` (`mod.rs` is the API the rest of taba uses), with
`macos.rs` and `linux.rs` for process facts, `unix.rs` shared by both
(process groups, `setsid`, signals), and `windows.rs`. Records, JSON output,
error codes and exit statuses are the same on every platform. CI runs the
whole test suite on macOS, Linux and Windows.

Linux reads `/proc/PID/stat` (process group, state, start time),
`/proc/PID/cmdline` (argv) and `/proc/net/tcp{,6}` with `/proc/PID/fd`
(listeners; like `lsof`, only this user's processes). Groups and signals
work as on macOS. The start time is the boot time (`btime`) plus the start
tick (10 ms resolution). The kernel recomputes `btime` from the wall clock,
which moves when the clock is stepped (NTP, or WSL catching up after the
host slept), so taba saves the first value by boot ID in `$TABA_HOME/btime/`.
A private directory lock serializes initialization and repair. Each entry
is published by atomic rename and read only when regular, owned by the
effective user, and mode `0600`. Either of the two files can recover the
other if it is deleted mid-boot. On upgrade, an owner-checked legacy cache
with no group or world write permissions can seed the new files.

Windows differs where the OS has no equivalent:

- **Group = Job Object.** `run` starts the process suspended, puts it in a
  new job named `Local\taba-PID-START`, gives the process an inheritable
  handle to the job, and resumes it. Children join the job, like a process
  group; the job's name stays valid while any process holds that handle
  (children started with handle inheritance, such as Node's or cmd's, do).
  `pgid` is recorded as the pid, as on Unix. When no process holds the job
  any more (the leader exited and its children did not inherit the handle,
  e.g. Python's `subprocess` default), taba falls back to the leader's
  descendants by parent pid, each started after the leader (and before any
  reuse of the leader's pid). Breaking away from the job is the Windows
  counterpart of leaving the process group: such children are not tracked.
- **Detaching.** `CREATE_NO_WINDOW` gives the service its own hidden
  console (the counterpart of a new session: closing the launching terminal
  does not end it), `CREATE_NEW_PROCESS_GROUP` its own console process group,
  and `CREATE_BREAKAWAY_FROM_JOB` takes it out of the launching terminal's or
  CI runner's job when that job allows breakaway; when it does not, the
  service stays in that job (nested) and ends with it. taba's own standard
  handles are made non-inheritable before spawning, so a service never holds
  the pipe of whoever reads taba's output.
- **No SIGTERM.** `stop` sends Ctrl+Break to the service's console (through
  a hidden helper, `taba __ctrl-break PID PGID`, because only a process
  attached to a console can signal it). Node, Python and Go see it as
  SIGBREAK and exit by default. After `--timeout` it calls
  `TerminateJobObject` (or `TerminateProcess` on the verified descendants
  when there is no job). JSON keeps `"signal": "TERM"` / `"KILL"` for the
  two steps; human output says "forced termination".
- **Port checks.** `GetExtendedTcpTable` (IPv4 and IPv6) gives listeners and
  their pids and is consulted first, because a refused loopback connect
  takes about two seconds on Windows (taba's connect timeout is 150 ms
  there). A bind refused with `WSAEACCES` (a reserved range, `netsh int
  ipv4 show excludedportrange`) counts as in use, so `--port auto` skips it.
- **Programs** resolve through `PATH` and `PATHEXT`, so `npm` runs
  `npm.cmd`. Paths are canonicalized without the `\?\` prefix, `~` stands
  for `%USERPROFILE%` (or `$HOME` when set), and the state lock is
  `LockFileEx` (through `File::lock`, which is `flock` on Unix). `log:`
  ready checks match CRLF output like LF.

WSL and Windows are separate process spaces, each with its own taba state.
WSL forwards `localhost` ports to Windows (`wslrelay.exe` in NAT mode; in
mirrored mode the ports are shared both ways), so a WSL server's port looks
taken from Windows while its process is invisible there. taba reports such
a port as `E_PORT_IN_USE` with `holder.kind: "unknown"`: forwarded from WSL
(`"outside": "wsl"`, with the relay's pid) when the listener is
`wslrelay.exe`, else "a process taba cannot identify", which may be another
user's or one outside this OS. taba never signals such a holder. The same
holds the other way round for taba inside WSL in mirrored mode.

### M2: JSON everywhere and `ps --all`

1. Every command with `--json` prints exactly one JSON document on stdout,
   for success and failure, and stderr is empty.
2. `ps --all --json` lists a taba service (`managed: true`, scope and holder
   filled) and an unmanaged `python3 -m http.server` started with `nohup` in
   `wt-b` (`managed: false`, `directory` and `git.branch` correct).
3. `ps --all` signals nothing and writes nothing (the `TABA_HOME` mtime is
   unchanged).

#### M2 implementation notes

- `ps --json` is now an envelope like every other command:
  `{"ok": true, "services": [...], "warnings": []}`. The rows keep every
  v0.1 field. This changes the v0.1 output (a bare array); the v0.1 and M1
  tests read `services` now.
- `logs --json` returns `{"ok": true, "service", "log_path", "log"}` with
  the whole log as text. `logs -f` streams raw text, so `-f --json` is
  `E_USAGE`, and `TABA_OUTPUT=json` does not apply to `-f`.
- `rm --json` reports `results` like `stop` (`removed`, or `refused` with
  the error).
- `stop` results keep `"signal": "TERM"` / `"KILL"` on every platform and
  add `platform_action`: `sigterm` / `sigkill` on macOS and Linux,
  `ctrl_break` / `terminate_job_object` on Windows (owner decision, M2).
- Commands in `next` are quoted for the platform's shell: POSIX `sh` on
  macOS and Linux, PowerShell on Windows (single quotes, `''` inside;
  `{port}` is quoted because PowerShell would read it as a script block).
  In cmd.exe, use double quotes instead (owner decision, M2).
- `ps --all` lists listeners with `lsof -a -u UID -iTCP -sTCP:LISTEN` and
  each process's directory with `lsof -d cwd` on macOS, `/proc/net/tcp{,6}`
  and `/proc/PID/cwd` on Linux, and `GetExtendedTcpTable` with the
  process's token user and a PEB read of its current directory on Windows
  (64-bit processes; others show `directory: null`). `pgid` is `null` on
  Windows. `holder` and `lease` are added in M3. `managed` matches the
  pid to a verified record through the service's process group (Windows:
  its job). It opens the state read-only and renews nothing.
- `ps --all` shows `W_NO_LSOF` when `lsof` is missing on macOS.

### M3: agents, leases and gc

1. `TABA_AGENT=a1` with `TABA_AGENT_PID` set to a live `sleep` starts a
   service. `TABA_AGENT=a2` in the same worktree gets `E_SCOPE_HELD` on
   `run` and `stop`, naming a1 and its pid.
2. With a1's `TABA_AGENT_PID` process then killed, `ps` shows the lease as
   `expired`, and the next `run` in `wt-b` (lazy gc, with `gc_grace = "0s"`
   in a test `config.toml`) stops the service and frees its port.
3. `--service-ttl 2s`: after 3 s, `taba gc` drops the service. `keep`
   survives `gc`.
4. `taba lease end --agent a1` followed by `gc --dry-run` lists a1's scopes
   and stops nothing.
5. TTL precedence: with `lease_ttl = "1h"` in `config.toml`, `whoami --json`
   shows `ttl: "1h", ttl_source: "config"`; with `TABA_TTL=2h` also set,
   `2h` / `env`; with `run --ttl 30m` as well, `30m` / `flag`. With no
   config file and no `TABA_TTL`, an agent gets `4h` / `default` and a
   `human` gets `none` / `default`. `ps --json` shows the same `lease`
   fields.
6. `--ttl none`: `expires_at` is `null`, and the service survives
   `gc` long after any TTL would have passed, until the holder's pid is
   killed.
7. Zero config: with no `config.toml` every command works. An invalid
   `lease_ttl = "soon"` or `TABA_TTL=soon` gives `E_CONFIG` naming the
   source; `--ttl soon` gives `E_USAGE`.
8. Claim queue: a1 runs `web` in `wt-a`. Agents in `wt-b` and then `wt-c`
   run `taba claim <wt-a scope>/web`; `ps --json` shows `claims` with
   `wt-b` first and `wt-c` second, and the text table shows `wt-b` under
   `NEXT`. a1's `TABA_AGENT_PID` is killed; with `claim_grace = "0s"` and
   `gc_grace = "1h"`, the next `whoami` in another folder hands `web` to
   `wt-b` at once: `ps` shows
   `<wt-b scope>/web` with the same instance, URL and pid, a
   `service_moved` event with `how: claim` is recorded, `wt-b`'s next call
   has `W_HANDED_OVER`, and the service now follows `wt-b`'s lease and TTL.
9. Dead claimants are skipped: as in test 8, but `wt-b`'s claimant agent
   pid is killed first. The hand-over goes to `wt-c`, and `wt-b`'s claim is
   removed with a `claim_skipped` event.
10. Successor first (decision 8): as in test 8, but with the default
    `claim_grace` (2 min). A `whoami` from `wt-c` right after a1's pid is
    killed hands nothing over. a2's first `whoami` in `wt-a` within the
    grace takes the scope over with `web` (`W_TOOK_OVER`), and `wt-b`'s and
    `wt-c`'s claims stay queued.
11. `claim --cancel` removes the claim. With an empty queue the expired
    scope waits for `gc_grace` and is then dropped as in test 2. `claim` of
    one's own scope's service returns `E_USAGE`.
12. `disown`: the holder runs `taba disown <wt-a scope>/web` with `wt-c`
    queued; the service moves to `wt-c` with `how: disown`. With nobody
    queued, `disown` returns `E_NO_CLAIMANT` and changes nothing. A plain
    `stop` of a claimed but unborrowed service succeeds and discards the
    claims.

#### M3 implementation notes

- A live lease checks both the agent name and a recorded PID/start-time pair.
  Without a PID it checks the name only. Both are still self-reported, not
  credentials. `--scope` alone does not let a caller impersonate the holder.
- TTL expires even if the agent process is still alive. The lease tests
  exercise this with a live fixture process; they do not establish whether
  any real harness exits at a usage-limit prompt.
- If a PID has disappeared, grace starts when taba first observes its absence;
  the OS may no longer know the process's exact exit time. After a TTL expiry
  or explicit lease end, grace starts at the recorded time.
- Cross-scope `stop` and `rm` still require `--force --reason`; use `gc` for
  expired services. The v0.1 test fixture continues to pass the owning scope
  explicitly (decision 4), and M1's audit assertion now filters force events.

### M4: borrows and lifetimes

Setup: `wt-a` runs `db` (a third throwaway worktree, `wt-c`, is used as a
second borrower).

1. `wt-b` takes `borrow <wt-a scope>/db` and gets its URL. The owner's
   `stop` returns `E_BORROWED` naming `wt-b`, with a `taba move` suggestion
   in `next`, and the server still answers. `stop --force --reason test`
   stops it, records `force_stop`, and then `borrow --check` from `wt-b`
   returns `E_DANGLING`.
2. While `wt-b` holds `borrow --mut <wt-a scope>/db -- sleep 5`,
   `borrow --mut` from `wt-c` returns `E_LOCKED` with an expiry, and with
   `--wait-lock 10` succeeds after the first one ends.
3. Killing the taba process of a `-- CMD` borrow frees the lock on the next
   call.
4. Borrowers inherit: `wt-a` runs `web` and `db`; `wt-b` and then `wt-c`
   borrow `db`. `wt-a`'s holder pid is killed and `claim_grace` and
   `gc_grace` are 0. `gc`
   hands `db` to `wt-b` (`handed_over`, `how: borrower`) and drops `web`.
   The database never stops: same instance and URL, `wt-b`'s borrow has
   ended because it is now the owner, and `wt-c`'s `borrow --check`
   succeeds and names `<wt-b scope>/db`.
5. Explicit claimants come first: as in test 4, but `wt-c` also ran
   `taba claim <wt-a scope>/db` after `wt-b` borrowed it. `db` goes to
   `wt-c` (`how: claim`), and `wt-b`'s borrow stays valid.
6. Owner stop still refuses: while `wt-b` borrows `db`, the live owner's
   `stop` returns `E_BORROWED` with `taba disown` in `next`; `taba disown`
   then hands `db` to `wt-b` (`how: borrower`) without a restart.
7. No live taker: `wt-b` borrows `db` and its own lease then ends (pid
   killed), and `wt-a`'s holder pid is killed. `gc` ends `wt-b`'s borrow,
   finds no live claimant, and drops `db` after `gc_grace`.

#### M4 implementation notes

- Borrows are stored by ID under `borrows/`, so a service hand-over can
  update them without changing IDs. The acquiring scope's borrow ends.
- With `--json`, a maintenance command's output is captured in the result;
  otherwise it uses the terminal. If taba dies, the next access no longer
  considers that command's borrow live; it does not promise to kill the child.
- An exclusive borrower restarts an existing stopped service by running
  `taba run --scope OWNER --name NAME …` from its own scope. The replacement
  retains the exclusive lock; the old shared borrows become dangling.

### M5: move

1. Automatic take-over: a1 (with `TABA_AGENT_PID` set to a `sleep`) runs
   `web` and `api` in `wt-a`. The `sleep` is killed. a2's first
   `taba whoami` in `wt-a` returns `you_hold_scope: true`, a `takeover`
   naming a1 and `holder_pid_gone`, and `W_TOOK_OVER`; `events.jsonl` has a
   `moved` event with `how: auto_take`. Both services keep their instance
   and URL, and a2 can stop them. a1 (back with a new live pid) gets
   `E_SCOPE_HELD` naming a2.
2. Take-over beats Drop: with `gc_grace = "1h"`, a2's take-over in test 1
   happens after a1's lease ended and nothing was stopped in between.
3. Holder a1 runs `move --to a2`. a2 can now stop the services, a1 gets
   `E_SCOPE_HELD`, and the services keep their instance and URL throughout.
4. `move --take` by a2 while a1's process is alive returns `E_LEASE_LIVE`.
   With `--force --reason test` it succeeds with `how: take_forced` and is
   recorded; without `--reason` it returns `E_FORCE_NEEDS_REASON`.
5. Moving one service: `wt-b` and `wt-c` borrow `<wt-a scope>/db`. a1 runs
   `taba move <wt-a scope>/db --to <wt-b scope>`. `ps` shows
   `<wt-b scope>/db` with the same instance and URL, `wt-b`'s borrow has
   ended, `wt-c`'s `borrow --check` succeeds and names the new service,
   `wt-a`'s `stop --scope` no longer touches `db`, and logs still grow in
   the moved log file. Moving into a scope that already has a `db` returns
   `E_CONFLICT`. The same move by `wt-c` while a1 is alive returns
   `E_NOT_OWNER`; after a1's pid is killed it succeeds without `--force`.

#### M5 implementation notes

- An explicit scope give initially binds the recipient by agent name. The
  recipient's first renewal with `TABA_AGENT_PID` binds the process identity.
- Service moves keep the live process, URL, original working directory and
  log stream. Active borrows follow the instance except the recipient's,
  which ends. Target name conflicts and in-flight service operations are
  refused before moving.

### M6: hooks and skill

1. Feeding recorded PreToolUse JSON to `taba hook claude-code pre-tool-use`:
   `lsof -ti:<other scope's port> | xargs kill` is denied with a reason that
   contains the owner scope and a `taba run --port auto` suggestion. `kill
   <own pid>` is allowed. `pkill -f http.server` is denied if any match
   belongs to another scope. `taba stop x --force --reason r` returns
   "ask" (decision 9), with the target in the reason. `kill <pid>`
   of an unmanaged `python3 -m http.server` started with `nohup` is
   **denied** (not ask), and the reason contains `--port auto` and "ask
   the human".
2. Unparseable commands are allowed and logged.
3. In a real Claude Code session: the SessionStart hook sets `TABA_AGENT`,
   visible in `taba whoami`. Ending the session marks the lease expired.
4. The same deny test passes for `taba hook codex pre-tool-use` once the
   hook is trusted in Codex.
5. The skill is rewritten (section 5), and a scripted two-worker scenario (S2
   plus S4) ends with both servers running and no cross-scope kill.

---

#### M6 implementation notes

- The hook inspects common Bash command shapes without running them. It
  returns no permission decision for an ordinary allowed command, so normal
  harness permissions still apply. Unparseable commands are logged and left
  to the harness. Native PowerShell commands, shell scripts, and expansion
  are outside its coverage.
- Claude Code's SessionStart writes the session name to `CLAUDE_ENV_FILE`;
  it does not infer an agent PID from an unverified parent process. SessionEnd
  marks leases for that identity ended without stopping services. TTL remains
  the backstop when the harness does not send an end event.

## 9. Decisions (2026-11-24)

Decided by the owner on 2026-11-24. They replace the open decisions of the
first v0.2 draft, and the sections above already follow them. The numbers
are kept so that "decision N" references above stay valid.

1. **Scope = the folder.** By default the scope is derived from the git
   worktree root path only (`<dirname>-<hash4>`), with the branch recorded
   and `W_BRANCH_CHANGED` when it changes. One scope holds any number of
   named services (`web`, `api`, `worker`, `db`, ...), which are listed,
   stopped and handed off together. `--scope` / `$TABA_SCOPE` stay for
   orchestrators that want task-named scopes. (Section 3.)

2. **Lease TTL is user-configurable, and a database shared by several
   worktrees is borrowed, not given its own scope.**
   - TTL precedence: `--ttl` on `run` (or `renew`) > `$TABA_TTL` >
     `$TABA_HOME/config.toml` (`lease_ttl`) > built-in default (4 h for
     agents, `none` for `human`). `none` means no expiry by time. The
     config file is optional and also holds `claim_grace` (default 2 min,
     decision 8), `gc_grace` (default 10 min) and `log_retention`
     (default 24 h); taba works with zero
     configuration. `whoami` and `ps --json` show the effective TTL and its
     source. Consequence: the per-service limit that was `run --ttl` is
     renamed `--service-ttl`.
   - Shared database: owned by the folder scope that started it, like any
     service. Other worktrees take shared borrows (`&`) for its URL and an
     exclusive borrow (`&mut`) to reset or migrate it, one at a time. A
     borrow cannot outlive the owner, so the owner cannot stop it and `gc`
     cannot drop it while borrowed. When the owner's work ends first,
     ownership of that one service passes to a borrower through the claim
     queue (decision 7), or explicitly with `taba move S/N --to SCOPE`. The earlier recommendation of a
     separate `shared` scope is withdrawn. Rc/Arc-style shared ownership
     (dropped when the last holder releases) is a possible later
     extension, not v0.2. (Section 3, "A database shared by several
     worktrees" and "Hand-off".)
   - A caller without `TABA_AGENT` is `human`.

3. **Stopping a borrowed service is refused** with `E_BORROWED`, by `stop`,
   `rm`, expired-lease stops and `gc` alike. `--force --reason` overrides
   and leaves the borrowers dangling (`W_BORROW_BROKEN`, `E_DANGLING` on
   `borrow --check`). The separate `--break-borrows` flag of the first
   draft is removed. Borrows by dead borrowers end with the borrower's
   lease, so they never pin a service forever.

4. **`--force --reason` is the only override, for humans too.** A human
   (no `TABA_AGENT`) mutating a service in another scope, or with no scope
   at all, gets `E_NOT_OWNER` / `E_SCOPE_HELD` like an agent and needs
   `--force --reason`, which is recorded. This replaces the draft's
   recommendation to allow unscoped humans without `--force`. In agent
   tool calls the hook turns `--force` into an "ask", so the human confirms
   it (decision 9; this replaces "the hook denies `--force`").

5. **Unmanaged processes: deny, not ask.** The hook denies killing an
   unmanaged process that listens on a TCP port or runs in another git
   worktree, and the CLI has no way to kill a process taba did not start.
   The deny message explains how to proceed: pick another port with
   `--port auto`, or ask the human, who can stop it themselves.

6. **Hand-off is mostly automatic.** If the current holder's agent process
   is gone (`TABA_AGENT_PID` liveness), or its lease ended otherwise, the
   next agent in the same scope takes over on its next taba call, with no
   command, and a `moved` event with `how: auto_take` is recorded. If the
   holder is still alive, the call fails with `E_SCOPE_HELD` naming the
   holder. `taba move --to` / `--take` stay as explicit tools for
   coordinators and humans; a forced take needs `--force --reason`.
   `move --to` by the departing agent itself is rarely possible, because
   usage limits stop agents abruptly. (Section 3, "Hand-off".)

7. **Claim queue before drop** (added later on 2026-11-24). `taba claim
   S/N` registers interest in owning a service (FIFO, recorded with the
   claimant scope, agent and time; `taba claim --cancel` leaves). When the
   owner's lease ends or its agent is gone, `gc` first offers the service
   to the queue (after `claim_grace`, decision 8): the first claimant whose agent is still alive becomes the
   new owner by an automatic move of that one service, recorded as an
   event, under the claimant's lease and TTL. Dead claimants are skipped
   and removed. Current borrowers are implicit claimants after the explicit
   ones, so a borrowed database is handed to a borrower instead of being
   stopped. Only if nobody is waiting do the grace period and the drop
   apply. An explicit `stop` by the owner still refuses while borrowed
   (`E_BORROWED`); the owner gives a service up to the queue with
   `taba disown S/N`. `ps` and `ps --json` show the queue. The hand-over
   keeps the same process, so the new owner gets whatever state it is in
   (section 2, "Limits"). (Section 3, "Claim queue"; section 4,
   `taba claim` and `taba disown`.)

8. **A same-folder successor beats the claim queue** (added later on
   2026-11-24). When a lease expires, `gc` waits for a short grace period,
   `claim_grace` (configurable in `config.toml`, default 2 min), before it
   serves any claim queue of that scope. If a successor agent in the same
   scope (the same folder) calls taba during the grace, it takes the scope
   over automatically (decision 6) with every service, and the claims stay
   queued. After the grace, claims are served as in decision 7, and the
   drop still waits for `gc_grace`. This replaces "whichever runs first
   wins". (Section 2, rules 4 and 5; section 3, "Claim queue" and "Leases";
   section 4, `taba gc`.)

9. **`--force` from a hooked agent asks the human** (added later on
   2026-11-24). When a hooked agent's tool call contains a taba command
   with `--force` (the coordinator case: a forced `move --take` or `stop`
   for a stuck worker), the PreToolUse hook returns
   `permissionDecision: "ask"` instead of `"deny"`, so the human confirms
   or rejects it in the agent's permission prompt. The hook cannot tell a
   coordinator from a worker (both are just agents), so this applies to
   every hooked agent; the skill still tells workers never to use
   `--force`. Raw kills stay denied (decision 5). If a harness does not
   support "ask" (Codex: not confirmed), its hook keeps denying. (Section
   2, rule 8; section 3, "Hand-off"; section 5, "Claude Code hooks".)

### Still open

- **Does a usage-limited agent's process exit?** Automatic take-over
  depends on the holder's `TABA_AGENT_PID` disappearing. If Claude Code or
  Codex stays alive at a usage-limit prompt, the next agent gets
  `E_SCOPE_HELD` until the TTL passes or someone forces a take. To verify
  in M6 for each harness.
- **Coordinators that are agents.** Resolved by decision 9: a forced
  command from a hooked coordinator asks the human. Still **unknown**:
  whether Orca can run taba commands itself at dispatch time, outside any
  agent's Bash tool (section 7), which would need no confirmation.
- **Agent identity for Codex and Orca** (section 5): unchanged, still
  unknown.
