# taba Proposal v0.1

**Bundle what you start, by the work it belongs to.**

`taba` is a CLI for starting, inspecting and stopping local processes such as development servers, grouped by work item ("scope"). It provides only the Docker-style `run / ps / logs / stop` operations, without containers. It does not depend on Orca or any particular AI tool and is used from an ordinary terminal.

## Problem

When developing in parallel with AI agents, servers get started from many terminals and checkouts, and it becomes unclear for `localhost:5173` "which work item it belongs to, which directory it runs from, and whether it is safe to stop". `taba` records the owner (the work item) at start time so that anyone can later inspect and stop it by work name.

## Usage

```sh
taba run --scope login-fix --name web --port 5173 -- npm run dev -- --port 5173 --strictPort
taba ps            # SERVICE / STATUS / URL / DIRECTORY
taba ps --json
taba logs login-fix/web -f
taba stop login-fix/web
taba stop --scope login-fix
```

- If `--scope` is omitted, the `TABA_SCOPE` environment variable is used. If neither is present, it is an error.
- `--port` declares the port to track. Configuring the app is left to the start command. If the port is in use, show its user as far as can be determined, and start or stop nothing.

## Ownership

- The owner is a named work scope, not the AI or terminal that started it. It persists after the terminal is closed.
- Re-running `run` with the same scope, name, command and directory does not start a second instance; it returns the existing one. The same name with different conditions is an error.
- `stop --scope` stops only the managed processes of that scope. It only stops processes and does not delete files.
- Processes whose identity (PID, start time, etc.) cannot be confirmed are not stopped. Unmanaged processes are never touched.

## MVP scope

- macOS, single binary, user privileges only. No Docker, configuration file or administrator privileges required.
- Manage started processes as process groups and save stdout/stderr to log files.
- Out of scope: containers, isolation, automatic port allocation, proxy, refusing to stop while borrowed, detecting the end of an AI session.

## Acceptance criteria

1. Servers can be started on different ports from two checkouts of the same project.
2. `taba ps` in a third terminal shows which URL belongs to which directory.
3. Re-running the same start does not add a server.
4. Logs can still be read after the launching terminal is closed, and stopping one server leaves the other and unmanaged servers running.
