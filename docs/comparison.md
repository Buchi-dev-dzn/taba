# taba compared with local process managers

taba is not a Docker alternative. It does not isolate processes or make
environments reproducible (see [benchmarks.md](benchmarks.md)). It does the
same job as the local process managers developers already use for dev
servers. This page compares taba with those tools, says where each one is
better, and marks how each claim was checked.

**Short version.** Most of what taba does, other tools do too. pm2,
process-compose and dekit (the new name of mprocs) all keep processes running
after the terminal closes and give a JSON list. dekit can also start a single
command with no config file, group tasks by path, and give each checkout its
own runner. taba's real differences are smaller than "only tool that does X":

- it has **no resident process**: no daemon, runner or tmux server;
- it **checks the port before starting** and names the process that holds it,
  even one taba did not start, then starts nothing;
- a repeated `run` **reuses** the running service instead of failing or
  starting a copy;
- `taba ps --json` gives **directory, port, URL, pid and log path** for every
  service, across all tasks, from any directory.

Every peer except foreman and hivemind beats taba on restarts. Most beat it
on platforms (taba is macOS only), and several have a TUI, dependencies and
readiness checks.

## Method

- **Date:** 2026-11-24. **Machine:** Apple M1, macOS 26.5.2, tmux 3.7b,
  Node 26.13.1, Ruby 4.0.6, Python 3.15.2.
- **taba:** 0.1.0, commit `d14e3ae` on `feat/taba-prototype`, release build.
- **Test service:** `python3 -m http.server <port>` (or the same server as a
  Procfile / YAML entry), in two throwaway git checkouts `co-a` and `co-b`.
- **"Terminal closes":** the start command ran inside a fresh tmux session
  on its own socket. The session was killed (which sends SIGHUP), then
  `curl` checked that the port still answered.
- **No global installs.** pm2 ran via `npx pm2@7.1.2` with `PM2_HOME` in a
  temp dir. foreman was installed with `GEM_HOME` pointing at a temp dir.
  overmind, hivemind, process-compose and dekit ran as release binaries from
  a temp dir. Everything, including daemons, sockets, temp files, the npx
  cache entry and dekit's `~/.local/share/dekit`, was removed afterwards.
- **Marks:** **[run]** = seen by running the tool. **[docs]** = from the
  README, `--help` or the built-in help of that version, not run.
  **[?]** = not checked. Anything without a mark is not claimed.

### Sources

| Tool | Version tested | Source read |
| --- | --- | --- |
| pm2 | 7.1.2 (npm, released 2026-11-03) | <https://github.com/Unitech/pm2> README (last changed 2026-07-02), `pm2 --help` |
| overmind | 2.5.1 (released 2024-03-26) | <https://github.com/DarthSim/overmind> README (last changed 2025-04-04), `overmind start --help` |
| hivemind | 1.1.0 (released 2021-12-03) | <https://github.com/DarthSim/hivemind> README (last changed 2020-07-20), `hivemind --help` |
| foreman | 0.90.0 (RubyGems) | <https://github.com/ddollar/foreman> README (last changed 2024-04-12), `foreman help` |
| process-compose | 1.124.0 (released 2026-10-26) | <https://github.com/F1bonacc1/process-compose> README (last changed 2026-08-17), `--help` of each command |
| dekit (formerly mprocs) | 0.10.3 (released 2026-10-25) | <https://github.com/pvolok/dekit> README (last changed 2026-10-08), [README-mprocs.md](https://github.com/pvolok/dekit/blob/master/README-mprocs.md), `dekit help <topic>` |

**About mprocs.** The `pvolok/mprocs` repository now redirects to
`pvolok/dekit`. Its README says "dekit is the next version of mprocs". The
classic mprocs TUI ships as `dekit mprocs` and runs `mprocs.yaml` unchanged.
The npm package `mprocs` is still at 0.9.6. This page tests both: dekit's own
runner, and the legacy `dekit mprocs` mode.

## Feature table

| | taba | pm2 | overmind | hivemind / foreman | process-compose | dekit (mprocs) |
| --- | --- | --- | --- | --- | --- | --- |
| **Survives closing the terminal** | yes [run] | yes [run] | only with `-D` [run]; the default foreground mode stopped [run] | no, foreground only [run] | with `up -D` [run] | yes [run]; legacy `dekit mprocs` TUI: no [run] |
| **Resident process** | none [run] | "God Daemon" [run] | overmind + a tmux server per project [run] | the foreground process itself | server process with an HTTP API [run] | one runner per project [run] |
| **Start one command without a config file** | yes [run] | yes, `pm2 start <cmd> -- args` [run] | no, needs a Procfile [docs] | hivemind reads a Procfile from stdin (`-`) [docs]; foreman needs a Procfile, `foreman run` is foreground only [docs] | no, needs `process-compose.yaml` [docs] | yes, `dekit spawn <path> -- <cmd>`, but only inside a git repo, `dekit.yaml` or `package.json` [run] |
| **Group and stop by group** | scopes, `stop --scope` [run] | namespaces, `pm2 stop <namespace>` [run] | one Procfile = one group, `overmind quit` [run] | one Procfile = one group (Ctrl-C) | namespaces, `namespace stop` [run] | task paths, globs and tags, `stop 'login-fix/**'` [run] |
| **Two checkouts side by side** | yes, if you give them different scopes [run] | yes with different namespaces, but both wrote to the same log file `logs/web-out.log` [run] | yes, socket per directory [run] | yes if ports differ [docs] | not with defaults: both use API port 8080, and `down` in checkout B stopped checkout A [run]. Works with distinct `-p` [run] | yes, a runner per project root, found automatically [run] |
| **Same start command twice** | returns the running service, exit 0 [run] | command + args: a second copy, which crash-looped [run]; script file: error "Script already launched" [run]; ecosystem file: restarts the app [run] | error "already running" [run] | second copy, fails on its own port | `up -D` printed "Starting…", exit 0, but failed on port 8080 (only in the log file) [run] | error "a task already exists", exit 1 [run] |
| **Port already in use** | checks first, names the owner (taba service or other pid + command), starts nothing [run] | no check; restarted ~15 times, then `errored` [run] | no check; process exits and overmind quits the whole group [run] | no check | no check; process ends with exit 1 [run] | no check; task shows `exited (code 1)` [run] |
| **Shows the process's directory** | `ps` column and `--json` [run] | `pm_cwd` in `jlist` / `describe`, not in `ls` [run] | implicit (you run it in that directory) | implicit | `project state` lists the config file path; not in `list -o json` [run] | `runner list --json` gives each runner's root; not per task [run] |
| **Shows ports** | `ps` URL column, `--json` [run] | no [run] | assigns `$PORT` (5000, +100 each) [docs] | assigns `$PORT` [docs] | `process ports <name>` lists listening ports [run] | no; a `ready.tcp` check can name one [docs] |
| **JSON list** | `ps --json` [run] | `jlist` [run] | no [docs] | no | `list -o json` [run] | `ls --json` (path, state, exit code; no pid, port or dir) [run] |
| **Logs after the terminal is gone** | file per service, `taba logs` [run] | files in `$PM2_HOME/logs`, `pm2 logs` [run] | `overmind echo` while it runs; no log file [docs] | none | `process logs` from the server [run]; per-process or single log files can be configured [docs] | `dekit screen` (current screen) [run]; a log file only if `log:` is set in `dekit.yaml` [docs] |
| **Can stop processes it did not start** | no, and it never signals unmanaged ones [run] | no: `pm2 stop <foreign pid>` says "id unknown" [run] | no (names from the Procfile) [docs] | no | no (names from the config) [docs] | no (task paths) [docs] |
| **PID-reuse safety** | checks leader pid + start time before signalling [docs: taba README] | daemon is the parent [run]; if the daemon dies, children keep running but drop out of `pm2 ls` [run] | parent supervisor [run] | parent supervisor | parent supervisor [run] | parent runner [run] |
| **Restarts on crash** | no | yes (default) [run] | `-r` auto-restart [docs] | no | restart policies [docs] | `autorestart` in `dekit.yaml`; never for spawned tasks [docs] |
| **TUI** | no | `pm2 monit` [docs] | tmux windows, `overmind connect` [docs] | no | yes [docs] | yes [docs] |
| **Dependencies / readiness checks** | no | no [?] | no | no | yes [docs] | yes [docs] |
| **Install footprint** | 1.3 MB binary, build from source [run] | Node.js + 16 MB `node_modules` (44 packages) [run] | 4.3 MB binary + tmux [run] | hivemind 3.3 MB binary [run]; foreman Ruby + 0.7 MB gems [run] | 42 MB binary [run] | 4.4 MB binary [run] |
| **Platforms** | macOS only | Linux, macOS, Windows [docs] | Linux, *BSD, macOS [docs] | hivemind: Linux, *BSD, macOS [docs]; foreman: anywhere Ruby runs [?] | Linux, macOS, Windows [docs] | Linux, macOS, Windows [docs] |

## Per-tool notes

### pm2

The most complete tool here, built for production Node.js. A resident daemon
supervises everything. It is better than taba at restarts, cluster mode
(load-balanced Node.js workers), boot-time startup scripts, log rotation
(`pm2-logrotate`), monitoring and platforms. Things to watch in a multi-task
dev setup: starting `python3 ... -m http.server` twice made a second copy
with the same name, which crash-looped against the port. Two apps named `web`
in different namespaces shared `web-out.log`, so `pm2 logs web` showed the
other checkout's output. After `kill -9` on the daemon, the old server kept
holding its port but no longer answered HTTP, and the new daemon did not list
it.

### overmind

A Procfile runner on top of tmux. Better than taba for running a whole
Procfile together, for `overmind connect` into a live process (debuggers,
REPLs), and for auto-restart, formations and `can-die`. Two checkouts work
side by side because each has its own `.overmind.sock`. Without `-D` it stops
when the terminal closes. In one of the two foreground runs, the terminal
close left a stale `.overmind.sock`, and the next start refused until that
file was removed. There is no ad-hoc start, no JSON and no log file.

### hivemind and foreman

Foreground Procfile runners: they live and die with the terminal, which is
what they are meant to do. They are not alternatives for detached services.
foreman is better than taba at one thing: `foreman export` writes the
Procfile out as launchd, systemd, supervisord, upstart, runit, bluepill,
inittab or daemon configs. hivemind is the minimal version of overmind,
without tmux.

### process-compose

A docker-compose-style config for plain processes. Better than taba at
dependencies, readiness and liveness probes, restart policies, its TUI, a
REST API, an MCP server for AI assistants, namespaces, file watching and
platforms. `process ports` shows the
ports a process listens on, which taba cannot do (taba only knows the port
you declare). Each project runs a server with an HTTP API on
`localhost:8080` by default. So with default settings a second checkout does
not start: `up -D` still printed "Starting…" and exited 0, and the error was
only in the log file. Worse, `process-compose down` in the second checkout
stopped the first checkout's project. A different `-p` per checkout (or Unix
sockets) avoids this.

### dekit (formerly mprocs)

**The closest peer, and it weakens taba's positioning.** Like taba, it keeps
tasks after the terminal closes, starts a single command without a config
file (`dekit spawn`), groups by path (`dekit stop 'login-fix/**'`), lists as
JSON and says it is "a full CLI for humans and agents". It also gives each
checkout its own runner automatically (by git root), so two checkouts never
clash, which taba leaves to you through the scope name. It is better than taba
at the TUI, dependencies, readiness checks, `wait`, autorestart for configured
tasks, JavaScript scripting and platforms. Where taba is still different:
dekit keeps a runner process per project, `spawn` outside a git repo or
project fails, a second `spawn` of the same path is an error instead of a
reuse, there is no port check, and `ls --json` has no pid, port, directory or
log path. Its `ls` is per project, and `runner list` is the only overview of
every project. The legacy `dekit mprocs` TUI is foreground only: it stopped
when the terminal closed.

### supervisord and launchd (not tested)

Both run long-lived services from a config file (`supervisord.conf`, a
launchd plist) and restart them. supervisord needs its own daemon. launchd is
macOS's init system. They fit "always on" services, not ad-hoc dev servers per
task. foreman can export to both.

## What this means for taba

- **Not unique:** surviving the terminal, ad-hoc start, grouping and a JSON
  list. pm2 and dekit do all four; process-compose does three.
- **Different, verified here:** no resident process; a port check before
  start that names the holder and starts nothing; idempotent `run`; one
  `ps --json` across every task with directory, port, URL, pid and log path;
  stopping only processes whose identity (pid + start time) still matches.
- **Missing compared with peers:** restarts, a TUI, dependencies and
  readiness, multi-process config files, Linux and Windows.
