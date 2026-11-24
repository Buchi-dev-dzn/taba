# taba vs Docker vs a plain shell: measurements

This page compares three ways to run the same long-running dev server on a
Mac. The goal is to find where taba is actually different, and where it is
not. All numbers come from the scripts in [`bench/`](../bench/README.md); the
raw data and full command transcripts are in `bench/results/`.

**Short version.** taba is not faster than a plain `nohup … &`. Its start
time is the same within noise, plus a fixed 200 ms wait inside `taba run`.
Against Docker, it saves about 0.3 s per start, avoids a ~2 GB resident VM, and
needs no image or bind mount. Its real differences are behavioural: a re-run
reuses the running service, a port conflict names the owner and starts
nothing, and `taba ps` maps each server to a task and a directory. Docker gives
isolation and a reproducible environment. taba gives neither.

## Machine and versions

| | |
| --- | --- |
| Chip | Apple M1, 16 GB RAM |
| OS | macOS 26.5.2 (25F84) |
| Docker | Docker Desktop 4.97.0, Engine 29.9.2 (linux/arm64 VM, 8 CPUs, 7.75 GiB memory limit) |
| taba | commit `db15210` (branch `feat/taba-prototype`), release build |
| Python | 3.15.2 on the host (Homebrew) and in `python:3-slim` (same version) |
| Node / Vite | Node 26.13.1, Vite 8.3.8 (installed locally into a temp directory) |
| Date | 2026-11-24 |

Docker Desktop was **already running** when the run started, so the engine
cold start was not measured. Other apps were open, so expect some noise,
especially in the p90 values.

## Method

The three modes run the same server on the same port:

| Mode | Start | Stop |
| --- | --- | --- |
| **A** plain shell | `sh -c 'nohup python3 -m http.server P > log 2>&1 &'` | `kill -TERM pid` |
| **B** taba | `taba run --scope bench --name web --port P -- python3 -m http.server P` | `taba stop bench/web` |
| **C** Docker | `docker run -d -p P:P python:3-slim python -m http.server P` | `docker stop ID` |
| **C-init** | same as C with `--init` | `docker stop ID` |

- **Start time** runs from just before the command is spawned to the first
  `HTTP 200` on `127.0.0.1:P`, polled every 2 ms with a raw socket.
- **Stop time** runs from just before the stop command to the moment the port
  is free (connections are refused and `0.0.0.0:P` can be bound).
- The modes are interleaved round by round (A, B, C, C-init, A, B, …), so drift
  on the machine affects all of them the same way. There are 15 rounds for
  Python and 12 for Vite. The p90 is nearest-rank.
- taba uses a throwaway `TABA_HOME`. Each container gets the label
  `taba-bench=1` and is removed by that label.
- `python:3-slim` was **not** present before the run. It was pulled once
  (`bench/pull.sh`) and removed at the end.

## Results

### Start and stop (python -m http.server, N = 15)

| | A nohup | B taba | C docker | C docker `--init` |
| --- | ---: | ---: | ---: | ---: |
| Start → first 200, median | **216 ms** | **226 ms** | 537 ms | 532 ms |
| Start → first 200, p90 | 478 ms | 324 ms | 1682 ms | 1689 ms |
| CLI returns after, median | 13 ms | 219 ms | 181 ms | 198 ms |
| Stop → port free, median | **3 ms** | 66 ms | **3211 ms** | 161 ms |
| Stop → port free, p90 | 4 ms | 82 ms | 3448 ms | 326 ms |

- A and B start at the same speed. Python's own startup (~150–200 ms) is most
  of it. `taba run` always sleeps 200 ms before it returns, to catch a command
  that exits right away. The server is usually already up by then, so the
  sleep barely shows in the time to first 200. It does make the CLI the
  slowest of the three to return.
- Docker adds about 0.3 s per start (container create, network setup, port
  forwarding from the VM) on top of the same Python startup.
- **`docker stop` without `--init` took 3.2 s every time.** Python runs as
  PID 1 in the container and ignores SIGTERM there, so Docker waits out its
  stop timeout and sends SIGKILL (exit code 137). The timeout was about 3 s on
  this install. With `--init`, `docker stop` takes 0.16 s.
- `taba stop` takes ~65 ms because it polls the process group every 50 ms
  after SIGTERM. A bare `kill` is faster because nothing waits.

### Start and stop (Vite dev server, N = 12)

A minimal Vite app (`index.html` + `main.js`, `npm install vite` into a
temporary directory, 27 s). Run as `./node_modules/.bin/vite --port P
--strictPort --host 127.0.0.1`.

| | A nohup | B taba |
| --- | ---: | ---: |
| Start → first 200, median | 786 ms | 522 ms |
| Start → first 200, p90 | 1373 ms | 1597 ms |
| Stop → port free, median | 10 ms | 85 ms |
| Server RSS (node + esbuild) | 111 MB | 101 MB |

The spread is large (363–2536 ms across rounds) and the two distributions
overlap. Read this as **no difference**, not as a taba win. **Docker was not
measured for Vite.** It would need a Node image that was not on the machine
(this benchmark may only pull `python:3-slim`) and a Linux `node_modules`
(esbuild and Rollup ship native binaries, so the macOS `node_modules` cannot
simply be bind-mounted).

### Memory

| | A nohup | B taba | C docker |
| --- | ---: | ---: | ---: |
| Server process RSS | 25.1 MB | 25.0 MB | 25.7 MB (inside the VM, `docker top`); cgroup 18.8 MiB (`docker stats`) |
| Resident tool overhead | 0 | **0**: no daemon or supervisor; `taba run` exits (peak 2.3 MB while running) | Docker Desktop: 11 host processes, **~2.07 GB footprint** (VM ~1.6 GB), RSS 0.9–1.5 GB |
| Extra per container | – | – | ~0 on the host; the VM already holds the memory |

The Docker Desktop numbers come from `bench/memory.py`: the macOS `footprint`
tool, the median of five samples, with no other containers running. The VM's
footprint was 912 MB before the image pull and the benchmark, then
1.4–1.6 GB after. The VM grows with use and does not shrink quickly. Its
limit is 7.75 GiB. RSS alone undercounts the VM, which is why footprint is
reported as well.

### Disk

| | taba | Docker |
| --- | ---: | ---: |
| Tool | 1.27 MB binary | Docker.app 2.1 GB |
| Per service | 414 B record + log (6.9 KB after 100 requests); 12 KB on disk | `python:3-slim` image 207 MB (46.7 MB compressed content) |
| Runtime | uses the host `python3` (85 MB, already installed) | VM disk `Docker.raw`: 1.2 GB allocated |
| One-time pull | – | 7.8 s for `python:3-slim` |

taba's log grows without bound, and nothing rotates it. `taba rm` deletes it.

### Behaviour

| Question | A nohup | B taba | C docker |
| --- | --- | --- | --- |
| **5. Same command again** | Starts a second process. With a fixed port it dies with `Address already in use`, and only its log file says so; `&` reports success. A dev server that falls back to the next port (Vite without `--strictPort`) runs **two copies**. | `already running bench/web → http://localhost:P`, exit 0, still one process. The same name with a different command or port is an error. | Always creates a new container. With the same `-p` it fails with `port is already allocated` (exit 125) and leaves a **Created** container behind. With `-p P` (random host port) it runs **two copies**. `--name` turns the duplicate into a name-conflict error, not a reuse. |
| **6. Port held by an unrelated process** | Shell prints the pid. The new process exits with `OSError: [Errno 48] Address already in use`, visible only in its log. | `port P is in use by pid N, which is not managed by taba: Python -m http.server P` / `nothing was started`, exit 1. When another taba scope holds the port, it names that service and its directory. | `ports are not available: … bind: address already in use`, exit 125. Leaves a **Created** container. |
| Is the holder killed? | No | No | No |
| **7. From a fresh terminal, which server belongs to which directory/task?** | `ps` shows `Python -m http.server P` with no directory. `lsof -a -p PID -d cwd` finds the directory. Nothing links it to a task. | `taba ps` shows scope/name, URL and directory in one line. `ps`/`lsof` still work as for A. | `docker ps` shows image, command and port, plus a random name. The host directory appears only in `docker inspect` (bind mounts). On the host, `lsof` shows the port held by `com.docker.backend`, not by the server. |
| **8. Edit a source file while running** | Visible on the next request (3 ms in the test) | Visible on the next request (3–4 ms) | Without a mount the server cannot see the working tree (404). With `-v $PWD:/srv -w /srv` the edit was visible after 6–45 ms (two runs). |
| **9. Logs after the launching terminal closes** | With `nohup … > file`: keeps running; log in the file you chose. Plain `&` without nohup: **killed** by SIGHUP, and the output is lost. | Keeps running (own session, no controlling terminal). `taba logs bench/web` from any terminal. | Keeps running. `docker logs ID`. |

The terminal-close test types each command into an interactive `zsh -f -i` on
a new pty, then closes the pty, which is what closing a terminal window does.
The full transcripts with the exact output of every command are in
[`bench/results/scenarios.md`](../bench/results/scenarios.md).

**Bind-mount read speed.** This is the cost of point 8 for Docker. Each value
is the median of 5 passes. "small" means walking, `stat`-ing and reading 2000
files of 4 KiB; "big" means reading one 64 MiB file. The test ran twice, and
the absolute values varied by about 2× between runs:

| | small files | 64 MiB file |
| --- | ---: | ---: |
| Native macOS (APFS) | 168–362 ms | 7–8 ms |
| Container, bind mount (virtiofs) | 595–1053 ms (~3–3.5× native) | 34–62 ms (~5–8× native) |
| Container, its own disk | 20–39 ms | 3–8 ms |

Reading from the container's own disk was the fastest of all, faster than
native macOS for small files. The bind mount was the slowest.

## Where taba wins

- **No resident cost.** No daemon, no VM, no image: 0 MB resident after
  `taba run` returns, against ~2 GB footprint for Docker Desktop. The disk cost
  is a 1.3 MB binary and a few KB per service, against 207 MB for the image
  plus 2.1 GB for Docker.app.
- **Faster start and stop than Docker.** Start takes ~0.23 s against ~0.53 s.
  Stop takes 66 ms against 3.2 s with the default `docker run` (0.16 s with
  `--init`).
- **Re-running is safe.** The same `taba run` returns the running service
  instead of starting a copy. Neither `nohup` nor `docker run` does this.
- **Port conflicts are explained before anything starts.** The message names
  the pid and command line, or the owning taba service, and leaves nothing
  behind. Docker leaves a Created container. nohup fails silently into a log.
- **Visibility by task.** `taba ps` shows scope, URL and directory together.
  With nohup you need `ps` plus `lsof -d cwd`, and there is still no task.
  With Docker you need `docker inspect`, and `lsof` points at
  `com.docker.backend`.
- **Works in place.** Edits are visible at native file speed, with no mount
  configuration and no bind-mount slowdown.

## Where taba does not win

- **Against plain `nohup … &`, taba is not faster.** Start time is the same
  within noise. Stop is ~60 ms slower because `taba stop` waits and verifies.
  `taba run` itself takes ~220 ms to return (a fixed 200 ms sleep) against
  ~13 ms for `&`. If a careful person uses `nohup` and a known log path, logs
  survive just as well. taba's gain over nohup is the bookkeeping (dedup,
  conflict message, `ps` by task, one place for logs), not speed.
- **Memory of the server itself is identical** (~25 MB for Python, ~100 MB for
  Vite). taba does not make servers lighter.
- **Logs are not rotated.** Docker's log drivers can rotate; taba's log grows
  until `taba rm`.

## Where Docker wins, or the comparison does not apply

- **Isolation.** A taba service is an ordinary host process with your user's
  full rights: your files, network, environment and `~/.ssh`. taba is **not a
  sandbox** and gives no file-system, network, PID or resource isolation.
  Docker runs the server in a Linux VM with namespaces and cgroups. If the
  reason to use Docker is to contain untrusted code, taba is no substitute.
- **Reproducibility.** Docker pins the OS, interpreter and system libraries in
  an image. The same image runs the same way on any machine and in CI. taba
  runs whatever `python3` or `node` is on your `PATH` today. Here, Python
  matched (3.15.2 in both) only by coincidence.
- **Linux parity and native dependencies.** Services that need Linux, or
  native modules built for Linux, belong in containers. taba runs on macOS
  only.
- **Resource limits, restart policies, health checks, networks between
  services, volumes.** Docker has all of these. taba has none: no restart on
  crash, no limits, no service-to-service DNS.
- **Fast container-local disk.** I/O inside the container's own file system
  was the fastest measured. Only bind-mounted host files are slow.

## Reproducing

```sh
cargo build --release
bench/run-all.sh          # needs Docker Desktop running and ports 18731–18738 free
```

The scripts touch only processes they start and containers labelled
`taba-bench=1`. They never remove images. If you pulled `python:3-slim` only for
this, remove it yourself with `docker image rm python:3-slim`.
