# taba

Start, list and stop local processes (dev servers and the like), bundled by
work scope. Docker-style `run / ps / logs / stop`, without containers.
macOS only, single binary, no daemon. See [docs/plan.md](docs/plan.md) for
the design.

## Build

```sh
cargo build --release      # binary at target/release/taba
cargo test                 # integration tests (need python3)
```

## Usage

```sh
# Start a command detached, owned by scope "login-fix", named "web".
# --port is only declared/tracked; pass the port to your command yourself.
taba run --scope login-fix --name web --port 5173 -- npm run dev -- --port 5173 --strictPort

taba ps                      # SERVICE / STATUS / URL / DIRECTORY
taba ps --scope login-fix    # only one scope
taba ps --json               # all fields, machine readable

taba logs login-fix/web      # print the log
taba logs login-fix/web -f   # follow until the service exits

taba stop login-fix/web      # stop one service
taba stop --scope login-fix  # stop every service in a scope
```

`--scope` falls back to `$TABA_SCOPE` for `run`, and for `logs` / `stop` when
the target is given as a bare `NAME`. `ps` lists every scope unless
`--scope` is passed. `stop` with no target needs an explicit `--scope`.

## Behaviour

- **Detached:** the command runs in a new session / process group
  (`setsid`) with stdin from `/dev/null` and stdout+stderr appended to the
  log, so closing the launching terminal does not stop it. No supervisor
  process is used.
- **Idempotent run:** the same scope + name + command + directory (+ port)
  that is still running prints the existing entry and starts nothing. The
  same name with a different command, directory or port is an error while
  the old one is running. An *exited* entry is replaced by a new `run`.
- **Ports:** if the declared port is claimed by another running taba
  service, or anything is already listening on it, `run` fails, names the
  owning taba service (or the unmanaged pid/command found via `lsof`), and
  starts nothing.
- **Safe stop:** taba only signals process groups it started, and only if
  the recorded identity still matches: the leader pid with its exact start
  time, or (if the leader has exited) a surviving group whose members all
  started after the recorded start time. It sends SIGTERM to the group,
  then SIGKILL after `--timeout` seconds (default 5). Unmanaged processes are
  never signalled. Stopping never deletes files; logs and records stay.
- **Status:** `ps` shows `running` or `exited` (dead, or identity no longer
  matches).

## State

Everything lives under `~/.taba` (override with `TABA_HOME`):

```text
services/<scope>/<name>.json   scope, name, command, cwd, port, pid, pgid, start time, log path
logs/<scope>/<name>.log        stdout + stderr (appended across runs)
lock                           flock for concurrent `run`s
```

The child also gets `TABA_SCOPE` and `TABA_SERVICE` in its environment.

## Limitations (prototype)

- macOS only (uses `proc_pidinfo` / `proc_listallpids`).
- Port detection checks loopback (IPv4/IPv6) connects and a wildcard bind;
  a server bound only to a non-loopback address may go unnoticed.
- No command to delete exited records or rotate logs yet.
- If the started command moves its children into a different process group
  or session, those children are not tracked or stopped.
