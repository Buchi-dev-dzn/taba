# Project services (`taba.toml`, `taba up`, `taba down`)

Issue #50. The owner decided on 2026-11-26 to build this. This note comes
before the implementation.

## What it is

`taba up` finds the nearest `taba.toml` in the current directory or its
ancestors. The file supplies service definitions and the invoking directory
still determines the scope (`--scope` > `$TABA_SCOPE` > git worktree, as for
`run`). Each selected service goes through the existing run and readiness
logic, in file order by default or in the order requested. The first startup
or readiness failure ends the command and earlier results remain available.
`taba down` applies the normal stop rules to the selected names in the
caller's scope. Missing service records are already down.

The file is optional. `taba run` with explicit flags is unchanged and stays
the form agents are taught; `taba.toml` saves retyping several `run`
commands. taba still has no config file it *requires*.

## File format

```toml
[services.web]
cmd = ["npm", "run", "dev", "--", "--port", "{port}", "--strictPort"]
port = "auto"            # a number, "auto" or "auto:START" (as --port)
ready = "http:/"         # tcp | http:PATH | log:REGEX (as --ready)
wait = 60                # seconds for the ready check (default 30)
env = { NODE_ENV = "development" }
cwd = "frontend"         # relative to taba.toml (default: its directory)

[services.worker]
cmd = "npm run worker"   # a string is split on whitespace
service_ttl = "8h"       # as --service-ttl
```

- The only top-level key is `services`; each `[services.NAME]` table is one
  service. `NAME` follows the usual name rules.
- `cmd` is required: an array of strings, or a string split on whitespace.
  taba runs no shell, so a string with quotes or shell syntax is rejected;
  write `["sh", "-c", "..."]` for that.
- `env` cannot set `PORT`, `TABA_SCOPE` or `TABA_SERVICE`, which taba sets.
- Unknown keys are errors (`E_CONFIG` with `details.source = "project"`),
  so a typo or a supervision setting such as `restart` is not ignored.

## Behaviour

- `taba up [NAME...] [--scope S] [--agent A] [--ttl D] [--json]` runs
  `run --wait=<wait>` for each selected service. A service that is already
  running with the same command, directory, port request and `env` (names
  and values) is reused (`already_running`); one running with different
  settings is `E_CONFLICT`, as for `run`. The record keeps only the `env`
  names and a keyed fingerprint of the values (see `run --env` in the
  README), never the values.
- On the first failure `up` stops. The reply has `ok: false`, `results` with
  each service handled so far (the failed one with `status: "failed"` and its
  error), and `error` with the failed service's code, `details.not_started`
  and `details.cause`. Started services are left running.
- `taba down [NAME...] [--scope S] [--agent A] [--timeout SECS] [--json]`
  stops the selected services of the caller's scope with `stop`'s rules
  (lease checks, borrows, op locks). Services in the scope that the file
  does not declare are left alone; use `taba stop --scope S` for those.
  A declared service without a record is reported as `absent`.
- No `taba.toml` found is `E_NO_PROJECT` (exit 5). An unknown `NAME` is
  `E_USAGE`.
- `taba run ... --save [--force]` writes a `run` to this file: to the
  `taba.toml` `up` would read from the current directory, else a new one at
  the worktree root. It parses the file and checks for a different
  definition of the name (`E_CONFLICT` without `--force`) before starting
  anything, writes only after the service started, and edits the file in
  place (other tables and comments are kept). Flags the run did not give
  (`--ready`, `--wait`, `--service-ttl`) keep the saved definition's values.
  `env` values are written in plain text (`W_ENV_SAVED`) and shown as
  `<hidden>` in replies.

## Out of scope

Restart on crash, health monitoring after start, dependency graphs beyond
the file's order, a resident daemon and autostart at login (design v0.2 §6).
Use process-compose or pm2 for those.
