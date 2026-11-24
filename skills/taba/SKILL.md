---
name: taba
description: Start, reuse, inspect and stop long-running local processes (dev servers, databases, file watchers, workers) with the `taba` CLI, grouped by task scope. Use whenever you need a process that outlives a single command, need to check what is already running or on which port, read a background process's output, or clean up before handing work back.
---

# taba: long-running local processes, bundled per task

`taba` records which task (a **scope**) owns each process it starts, so that
you and the human can later see what is running and stop exactly your own
processes. Everything below uses only these commands:

```sh
taba run [--scope S] --name N [--port P] -- CMD...
taba ps [--scope S] [--json]
taba logs [-f] S/N
taba stop S/N
taba stop --scope S [--timeout SECS]
```

## Rules

1. **Start long-running processes with `taba run`.** Never use `&`, `nohup`,
   `disown`, `setsid` or a terminal you leave behind. `run` already detaches
   and returns immediately.
2. **Reuse before you start.** Run `taba ps --scope <scope> --json` first. If a
   service you need has `"status": "running"`, use its `url` and don't start
   another. Re-running the exact same `taba run` (same scope, name, command,
   directory and port) is safe: it prints `already running: ...` and starts
   nothing.
3. **Stay inside your own scope.** Never stop another scope's services, and
   never `kill` a process taba reports as not managed by taba.

## Choosing a scope

- If `$TABA_SCOPE` is set, use it. It is then the default for `run`, and for
  `logs` / `stop` when the target is a bare `NAME`.
- Otherwise use one scope per task or branch, and pass `--scope` on every
  `run`.
- Names: lowercase, short, stable, e.g. the branch or ticket (`login-fix`,
  `eng-142`). Service names describe the role: `web`, `api`, `db`, `worker`.
- `taba stop` with no target needs an explicit `--scope` even when
  `$TABA_SCOPE` is set.

## Ports

Give the port to **both** taba and the app, and make the app fail instead of
silently moving to another port:

```sh
taba run --scope login-fix --name web --port 5173 -- npm run dev -- --port 5173 --strictPort
taba run --scope login-fix --name api --port 8000 -- uvicorn app:app --port 8000
```

`--port` only tells taba what to track. taba doesn't configure the app.

If `run` fails with `port N is already claimed by taba service ...` or
`port N is in use by pid ... which is not managed by taba`, **do not kill
that process.** Pick another free port, or ask the human if it has to be that
port. Nothing was started.

## Logs

```sh
taba logs login-fix/web        # whole log so far (stdout + stderr)
taba logs login-fix/web -f     # follow until the service exits
```

`-f` blocks until the service exits. In a non-interactive tool call, prefer
the plain form, or pipe it through `tail`. The log path is also in
`taba ps --json` (`log_path`).

## Stopping

```sh
taba stop login-fix/web        # one service
taba stop --scope login-fix    # everything in your scope
```

taba only signals process groups it started and can still verify. It sends
SIGTERM, then SIGKILL after `--timeout` seconds (default 5). Stopping never
deletes logs or records.

## Hand-off

When you hand work back to the human, report each service you started, for
example:

- `login-fix/web`, http://localhost:5173, **left running** (needed for
  review), or **stopped**
- how to stop it: `taba stop --scope login-fix`

By default, stop what you no longer need with `taba stop --scope <scope>`.
Leave a service running only when the human will use it, and say so.
