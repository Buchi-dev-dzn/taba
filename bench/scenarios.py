#!/usr/bin/env python3
"""Behavioural comparisons: re-runs, port conflicts, visibility, working-tree
access, logs after the launching terminal closes, and disk use.

Every command and its real output is written to bench/results/scenarios.md.
Only processes and containers started here are touched; containers carry the
label taba-bench=1 and are removed by that label at the end.

Usage: python3 bench/scenarios.py [--port 18735]
"""

import argparse
import os
import pty
import select
import shutil
import signal
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import (IMAGE, LABEL, TABA, docker_bench_cleanup, http_get,  # noqa: E402
                 http_status, kill_quietly, now, port_free, run, save_text,
                 wait_200, wait_free)

OUT = []
STARTED_PIDS = []


def md(text=""):
    OUT.append(text)
    print(text, flush=True)


def show(cmd, cwd=None, env=None, label=None):
    """Run cmd, record command, exit code and output."""
    r = run(cmd, cwd=cwd, env=env)
    shown = label or " ".join(str(c) for c in cmd)
    body = (r.stdout + r.stderr).rstrip()
    md(f"```console\n$ {shown}\n{body}\n[exit {r.returncode}]\n```")
    return r


def note(text):
    md(f"> {text}\n")


def listeners(port):
    """pid list listening on port (host side)."""
    r = run(["lsof", "-nP", f"-iTCP:{port}", "-sTCP:LISTEN", "-t"])
    return sorted(set(int(x) for x in r.stdout.split()))


def http_servers_on(port):
    r = run(["pgrep", "-f", f"http.server {port}$"])
    return sorted(int(x) for x in r.stdout.split())


def nohup_start(cmd, cwd, log):
    r = run(["sh", "-c", f"nohup {cmd} > {log} 2>&1 & echo $!"], cwd=cwd)
    pid = int(r.stdout.strip())
    STARTED_PIDS.append(pid)
    return pid


def docker_run(*args):
    return run(["docker", "run", "-d", "--label", LABEL, *args])


# ---------------------------------------------------------------- 5 re-run

def rerun(work, env, P):
    md("## 5. Re-running the same command\n")
    d = work / "rerun"
    d.mkdir()

    md("### A. `nohup python3 -m http.server P &`, twice\n")
    p1 = nohup_start(f"python3 -m http.server {P}", d, d / "a1.log")
    wait_200(P, now())
    p2 = nohup_start(f"python3 -m http.server {P}", d, d / "a2.log")
    time.sleep(1.5)
    alive = [p for p in (p1, p2) if run(["ps", "-p", str(p)]).returncode == 0]
    md(f"first pid {p1}, second pid {p2}; alive after 1.5 s: {alive}; "
       f"listeners on :{P}: {listeners(P)}\n")
    md("Second log (`a2.log`), last line:\n")
    md("```\n" + ((d / "a2.log").read_text().strip().splitlines() or ["(empty)"])[-1] + "\n```")
    note("The shell reports nothing: `&` returns success immediately. The second "
         "server only fails because the port is taken; the error is in its log file.")
    for p in (p1, p2):
        kill_quietly(p)
    wait_free(P, now())

    md("### A'. Same, with a dev server that picks the next free port (vite without `--strictPort`)\n")
    vite = work / "vite-app" / "node_modules" / ".bin" / "vite"
    if vite.exists():
        v1 = nohup_start(f"{vite} --port {P} --host 127.0.0.1", vite.parents[2], d / "v1.log")
        wait_200(P, now())
        v2 = nohup_start(f"{vite} --port {P} --host 127.0.0.1", vite.parents[2], d / "v2.log")
        t0 = now()
        while now() - t0 < 10 and http_status(P + 1) != 200:
            time.sleep(0.05)
        md(f"listeners on :{P}: {listeners(P)}, on :{P + 1}: {listeners(P + 1)}\n")
        md("```\n" + "\n".join(l for l in (d / "v2.log").read_text().splitlines()
                                if "Port" in l or "Local" in l) + "\n```")
        note("A plain re-run of a typical dev server starts a second copy on another port.")
        for p in (v1, v2):
            kill_quietly(p)
        wait_free(P, now())
        wait_free(P + 1, now())
    else:
        md("(skipped: vite scaffold missing)\n")

    md("### B. `taba run`, twice\n")
    cmd = [TABA, "run", "--scope", "bench", "--name", "web", "--port", str(P), "--",
           "python3", "-m", "http.server", str(P)]
    lbl = f"taba run --scope bench --name web --port {P} -- python3 -m http.server {P}"
    show(cmd, cwd=d, env=env, label=lbl)
    wait_200(P, now())
    show(cmd, cwd=d, env=env, label=lbl)
    md(f"python http.server processes for :{P}: {http_servers_on(P)}\n")
    md("Same name, different command:\n")
    show([*cmd[:-1], str(P + 1)], cwd=d, env=env, label=lbl[:-len(str(P))] + str(P + 1))
    show([TABA, "stop", "bench/web"], env=env, label="taba stop bench/web")
    run([TABA, "rm", "bench/web"], env=env)
    wait_free(P, now())

    md("### C. `docker run -d -p P:P ...`, twice\n")
    dl = f"docker run -d -p {P}:{P} {IMAGE} python -m http.server {P}"
    show(["docker", "run", "-d", "--label", LABEL, "-p", f"{P}:{P}", IMAGE, "python", "-m",
          "http.server", str(P)], label=dl)
    wait_200(P, now())
    show(["docker", "run", "-d", "--label", LABEL, "-p", f"{P}:{P}", IMAGE, "python", "-m",
          "http.server", str(P)], label=dl)
    show(["docker", "ps", "-a", "--filter", f"label={LABEL}", "--format",
          "{{.ID}}  {{.Status}}  {{.Ports}}"], label="docker ps -a --filter label=taba-bench=1")
    note("The second `docker run` fails on the port, but leaves a container in the Created state.")
    docker_bench_cleanup()
    wait_free(P, now())
    md("Without a fixed host port (`-p P` publishes to a random host port), twice:\n")
    for _ in range(2):
        docker_run("-p", str(P), IMAGE, "python", "-m", "http.server", str(P))
    show(["docker", "ps", "--filter", f"label={LABEL}", "--format", "{{.ID}}  {{.Status}}  {{.Ports}}"],
         label="docker ps --filter label=taba-bench=1")
    docker_bench_cleanup()
    md("With `--name`, twice:\n")
    nl = f"docker run -d --name taba-bench-web -p {P}:{P} {IMAGE} python -m http.server {P}"
    for _ in range(2):
        show(["docker", "run", "-d", "--label", LABEL, "--name", "taba-bench-web", "-p",
              f"{P}:{P}", IMAGE, "python", "-m", "http.server", str(P)], label=nl)
    note("`--name` prevents a duplicate, but as an error, not by returning the running container.")
    docker_bench_cleanup()
    wait_free(P, now())


# ---------------------------------------------------------------- 6 conflict

def conflict(work, env, P):
    md("## 6. Port conflict\n")
    d = work / "conflict"
    d.mkdir()
    holder = nohup_start(f"python3 -m http.server {P}", d, d / "holder.log")
    wait_200(P, now())
    md(f"An unrelated process holds :{P}: `python3 -m http.server {P}` (pid {holder}, "
       "started with nohup, not by taba).\n")

    md("### A. nohup\n")
    pa = nohup_start(f"python3 -m http.server {P}", d, d / "a.log")
    time.sleep(1.5)
    md(f"Shell prints the pid only. New pid {pa} alive after 1.5 s: "
       f"{run(['ps', '-p', str(pa)]).returncode == 0}. Last log line:\n")
    md("```\n" + (d / "a.log").read_text().strip().splitlines()[-1] + "\n```")

    md("### B. taba\n")
    show([TABA, "run", "--scope", "bench", "--name", "web", "--port", str(P), "--",
          "python3", "-m", "http.server", str(P)], cwd=d, env=env,
         label=f"taba run --scope bench --name web --port {P} -- python3 -m http.server {P}")
    show([TABA, "ps", "--scope", "bench"], env=env, label="taba ps --scope bench")

    md("### C. docker\n")
    show(["docker", "run", "-d", "--label", LABEL, "-p", f"{P}:{P}", IMAGE, "python", "-m",
          "http.server", str(P)], label=f"docker run -d -p {P}:{P} {IMAGE} python -m http.server {P}")
    show(["docker", "ps", "-a", "--filter", f"label={LABEL}", "--format", "{{.ID}}  {{.Status}}"],
         label="docker ps -a --filter label=taba-bench=1")
    md(f"Holder pid {holder} still alive: {run(['ps', '-p', str(holder)]).returncode == 0}, "
       f"still answering: {http_status(P) == 200}\n")
    docker_bench_cleanup()

    md("### B. taba, port held by another taba scope\n")
    kill_quietly(holder)
    wait_free(P, now())
    run([TABA, "run", "--scope", "other", "--name", "api", "--port", str(P), "--",
         "python3", "-m", "http.server", str(P)], cwd=d, env=env)
    wait_200(P, now())
    show([TABA, "run", "--scope", "bench", "--name", "web", "--port", str(P), "--",
          "python3", "-m", "http.server", str(P)], cwd=d, env=env,
         label=f"taba run --scope bench --name web --port {P} -- python3 -m http.server {P}")
    run([TABA, "stop", "--scope", "other"], env=env)
    run([TABA, "rm", "--scope", "other"], env=env)
    wait_free(P, now())


# ---------------------------------------------------------------- 7 visibility

def visibility(work, env, P):
    md("## 7. Visibility from a fresh terminal\n")
    dirs = {}
    for k in ("checkout-a", "checkout-b", "checkout-c"):
        dirs[k] = work / k
        dirs[k].mkdir()
    pa = nohup_start(f"python3 -m http.server {P}", dirs["checkout-a"], dirs["checkout-a"] / "a.log")
    run([TABA, "run", "--scope", "login-fix", "--name", "web", "--port", str(P + 1), "--",
         "python3", "-m", "http.server", str(P + 1)], cwd=dirs["checkout-b"], env=env)
    docker_run("-v", f"{dirs['checkout-c']}:/srv", "-w", "/srv", "-p", f"{P + 2}:{P + 2}",
               IMAGE, "python", "-m", "http.server", str(P + 2))
    for p in (P, P + 1, P + 2):
        wait_200(p, now())
    md(f"Three servers: A (nohup) in `checkout-a` on :{P}, B (taba, scope login-fix) in "
       f"`checkout-b` on :{P + 1}, C (docker, checkout-c bind-mounted) on :{P + 2}.\n")
    show(["sh", "-c", f"lsof -nP -iTCP -sTCP:LISTEN | awk 'NR==1 || /:{P}|:{P + 1}|:{P + 2}/'"],
         label="lsof -nP -iTCP -sTCP:LISTEN   # filtered to the three ports")
    show(["sh", "-c", f"ps -axo pid,command | grep '[h]ttp.server {str(P)[:-1]}'"],
         label="ps -axo pid,command | grep http.server")
    pids = " ".join(str(p) for p in listeners(P) + listeners(P + 1))
    show(["sh", "-c", f"for p in {pids}; do lsof -a -p $p -d cwd -Fn | sed -n 's/^n//p'; done"],
         label="lsof -a -p PID -d cwd   # for the A and B pids")
    show([TABA, "ps"], env=env, label="taba ps")
    show(["docker", "ps", "--filter", f"label={LABEL}", "--format",
          "table {{.ID}}\t{{.Image}}\t{{.Command}}\t{{.Ports}}\t{{.Names}}"], label="docker ps")
    show(["sh", "-c", f"docker inspect --format '{{{{range .Mounts}}}}{{{{.Source}}}}{{{{end}}}}' "
          f"$(docker ps -q --filter label={LABEL})"], label="docker inspect --format '{{range .Mounts}}{{.Source}}{{end}}' ID")
    kill_quietly(pa)
    run([TABA, "stop", "--scope", "login-fix"], env=env)
    run([TABA, "rm", "--scope", "login-fix"], env=env)
    docker_bench_cleanup()
    for p in (P, P + 1, P + 2):
        wait_free(p, now())


# ---------------------------------------------------------------- 8 working tree

def working_tree(work, env, P):
    md("## 8. Working-tree access\n")
    d = work / "tree"
    d.mkdir()
    f = d / "probe.txt"

    def edit_visible(port):
        f.write_text("v1\n")
        time.sleep(0.2)
        before = http_get(port, "/probe.txt")
        f.write_text("v2\n")
        t0 = now()
        while now() - t0 < 5:
            if http_get(port, "/probe.txt") == b"v2\n":
                return before, now() - t0
            time.sleep(0.001)
        return before, None

    rows = []
    pa = nohup_start(f"python3 -m http.server {P}", d, d / "a.log")
    wait_200(P, now())
    rows.append(("A nohup", *edit_visible(P)))
    kill_quietly(pa)
    wait_free(P, now())

    run([TABA, "run", "--scope", "bench", "--name", "web", "--port", str(P), "--",
         "python3", "-m", "http.server", str(P)], cwd=d, env=env)
    wait_200(P, now())
    rows.append(("B taba", *edit_visible(P)))
    run([TABA, "stop", "bench/web"], env=env)
    run([TABA, "rm", "bench/web"], env=env)
    wait_free(P, now())

    docker_run("-p", f"{P}:{P}", IMAGE, "python", "-m", "http.server", str(P))
    wait_200(P, now())
    f.write_text("v1\n")
    rows.append(("C docker, no mount", http_get(P, "/probe.txt"), None))
    docker_bench_cleanup()
    wait_free(P, now())

    docker_run("-v", f"{d}:/srv", "-w", "/srv", "-p", f"{P}:{P}", IMAGE, "python", "-m",
               "http.server", str(P))
    wait_200(P, now())
    rows.append(("C docker, -v $PWD:/srv", *edit_visible(P)))
    docker_bench_cleanup()
    wait_free(P, now())

    md("| Mode | GET /probe.txt before edit | edit visible after |\n|---|---|---|")
    for name, before, dt in rows:
        b = before.decode().strip() if before else "404 (file not in container)"
        md(f"| {name} | {b} | {'-' if dt is None else f'{dt * 1000:.1f} ms'} |")
    md()

    md("### Read speed: native vs bind mount\n")
    data = work / "fsdata"
    (data / "small").mkdir(parents=True)
    for i in range(2000):
        (data / "small" / f"f{i:04d}.txt").write_bytes(os.urandom(4096))
    (data / "big.bin").write_bytes(os.urandom(64 * 1024 * 1024))
    here = Path(__file__).resolve().parent
    r1 = run(["python3", str(here / "fsread.py"), str(data)])
    r2 = run(["docker", "run", "--rm", "--label", LABEL, "-v", f"{data}:/data:ro", "-v",
              f"{here}:/bench:ro", IMAGE, "sh", "-c",
              "python /bench/fsread.py /data && cp -r /data /tmp/data && python /bench/fsread.py /tmp/data"])
    lines = r2.stdout.strip().splitlines()
    md("Each line: median of 5 passes. `small` = walk + stat + read 2000 files of 4 KiB; "
       "`big` = read one 64 MiB file.\n")
    md("```\nnative macOS (APFS):        " + r1.stdout.strip() +
       "\ncontainer, bind mount:       " + (lines[0] if lines else r2.stderr) +
       "\ncontainer, its own disk:     " + (lines[1] if len(lines) > 1 else "") + "\n```")


# ---------------------------------------------------------------- 9 logs

def pty_session(lines, settle=2.0):
    """Open an interactive zsh on a new pty, type lines, then close the pty
    (what a terminal window does when it is closed: the shell gets SIGHUP)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("zsh", ["zsh", "-f", "-i"])
    buf = b""
    for line in lines:
        os.write(fd, line.encode() + b"\n")
    t0 = now()
    while now() - t0 < settle:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                buf += os.read(fd, 4096)
            except OSError:
                break
    os.close(fd)
    os.waitpid(pid, 0)
    return buf.decode(errors="replace")


def logs_after_close(work, env, P):
    md("## 9. Logs after the launching terminal closes\n")
    d = work / "term"
    d.mkdir()
    md("Each case types the command into an interactive `zsh -f -i` on a new pty, waits 2 s, "
       "then closes the pty (the shell receives SIGHUP, as when a terminal window is closed). "
       "Then one request is made from outside and the logs are read.\n")
    cases = [
        ("A0 plain `&`", f"cd {d} && python3 -m http.server {P} &"),
        ("A nohup", f"cd {d} && nohup python3 -m http.server {P} > {d}/a.log 2>&1 &"),
        ("B taba", f"cd {d} && TABA_HOME={env['TABA_HOME']} {TABA} run --scope bench --name web "
                   f"--port {P} -- python3 -m http.server {P}"),
        ("C docker", f"docker run -d --label {LABEL} -p {P}:{P} {IMAGE} python -m http.server {P}"),
    ]
    md("| Case | still serving after close | where the log is | request after close in log |\n|---|---|---|---|")
    for name, line in cases:
        pty_session([line], settle=2.5)
        time.sleep(1.0)
        alive = http_status(P) == 200
        if alive:
            http_status(P, "/after-close")
            time.sleep(0.3)
        if name.startswith("A0"):
            where, logged = "the closed terminal (lost)", False
        elif name.startswith("A "):
            where = "`a.log` (path chosen at launch)"
            logged = "/after-close" in (d / "a.log").read_text()
        elif name.startswith("B"):
            where = "`taba logs bench/web`"
            logged = "/after-close" in run([TABA, "logs", "bench/web"], env=env).stdout
        else:
            cid = run(["docker", "ps", "-q", "--filter", f"label={LABEL}"]).stdout.strip()
            where = "`docker logs ID`"
            logged = bool(cid) and "/after-close" in run(["docker", "logs", cid]).stderr
        md(f"| {name} | {'yes' if alive else 'no'} | {where} | {'yes' if logged else '-' if not alive else 'no'} |")
        # stop whatever this case left behind
        for p in http_servers_on(P):
            kill_quietly(p)
        run([TABA, "stop", "bench/web"], env=env)
        run([TABA, "rm", "bench/web"], env=env)
        docker_bench_cleanup()
        wait_free(P, now())
    md()


# ---------------------------------------------------------------- 4 disk

def disk(work, env, P):
    md("## 4. Disk\n")
    d = work / "disk"
    d.mkdir()
    run([TABA, "run", "--scope", "bench", "--name", "web", "--port", str(P), "--",
         "python3", "-m", "http.server", str(P)], cwd=d, env=env)
    wait_200(P, now())
    for _ in range(100):
        http_status(P)
    run([TABA, "stop", "bench/web"], env=env)
    show(["sh", "-c", f"cd {env['TABA_HOME']} && find . -type f -exec ls -l {{}} + | awk '{{print $5, $9}}'"],
         label="find $TABA_HOME -type f   # size path, after one service and 100 requests")
    show(["du", "-sk", env["TABA_HOME"]], label="du -sk $TABA_HOME")
    run([TABA, "rm", "bench/web"], env=env)
    show(["ls", "-l", TABA], label="ls -l target/release/taba")
    show(["docker", "image", "inspect", "--format", "{{.Size}}", IMAGE],
         label=f"docker image inspect --format '{{{{.Size}}}}' {IMAGE}")
    show(["du", "-sh", "/Applications/Docker.app"], label="du -sh /Applications/Docker.app")
    raw = Path.home() / "Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"
    if raw.exists():
        show(["du", "-h", str(raw)], label="du -h ~/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw")
    show(["sh", "-c", "du -sh $(python3 -c 'import sys; print(sys.base_prefix)')"],
         label="du -sh <host python3 prefix>   # what A and B rely on instead of an image")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18735)
    a = ap.parse_args()
    P = a.port
    for p in (P, P + 1, P + 2):
        assert port_free(p), f"port {p} busy"
    work = Path(tempfile.mkdtemp(prefix="taba-scen-")).resolve()
    env = {**os.environ, "TABA_HOME": str(work / "taba-home")}
    vite_app = work / "vite-app"
    vite_app.mkdir()
    (vite_app / "package.json").write_text('{"name":"bench-app","private":true,"type":"module"}\n')
    (vite_app / "index.html").write_text("<!doctype html><p>hi</p>\n")
    run(["npm", "install", "--no-audit", "--no-fund", "--silent", "vite@8.3.8"], cwd=vite_app)
    md("# Scenario transcripts\n")
    md("Generated by `bench/scenarios.py`. Port numbers are the bench ports; "
       "`$TABA_HOME` is a throwaway directory.\n")
    try:
        rerun(work, env, P)
        conflict(work, env, P)
        visibility(work, env, P)
        working_tree(work, env, P)
        logs_after_close(work, env, P)
        disk(work, env, P)
    finally:
        for p in STARTED_PIDS:
            kill_quietly(p)
        for p in (P, P + 1, P + 2):
            for pid in http_servers_on(p):
                kill_quietly(pid)
        run([TABA, "stop", "--scope", "bench"], env=env)
        docker_bench_cleanup()
        save_text("scenarios.md", "\n".join(OUT) + "\n")
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
