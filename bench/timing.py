#!/usr/bin/env python3
"""Start-to-first-200 and stop-to-port-free timings, plus memory samples.

Modes (all serve HTTP on the same port, one at a time, interleaved per round):
  A       sh -c 'nohup python3 -m http.server P > log 2>&1 &'   stop: kill -TERM pid
  B       taba run --scope bench --name web --port P -- python3 -m http.server P
                                                                 stop: taba stop bench/web
  C       docker run -d -p P:P python:3-slim python -m http.server P
                                                                 stop: docker stop <id>
  C-init  same as C with --init (tini as PID 1 forwards SIGTERM)

Usage: python3 bench/timing.py [--rounds N] [--port P] [--vite]
Writes bench/results/timing-python.json (or timing-vite.json with --vite).
"""

import argparse
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import (IMAGE, LABEL, TABA, docker_bench_cleanup, docker_host_procs,  # noqa: E402
                 footprint_mb, kill_quietly, now, port_free, run, rss_kb, save,
                 stats, tree_rss_kb, wait_200, wait_free)

VITE_VERSION = "8.3.8"


def sh_start(cmd, cwd, log):
    """Mode A: background a command with nohup from a non-interactive shell; return its pid."""
    r = run(["sh", "-c", f"nohup {cmd} > {log} 2>&1 & echo $!"], cwd=cwd)
    return int(r.stdout.strip())


class Python:
    name = "python"

    def __init__(self, port, work):
        self.port, self.work = port, work
        self.argv = ["python3", "-m", "http.server", str(port)]

    def cmd_a(self):
        return " ".join(self.argv)


class Vite:
    name = "vite"

    def __init__(self, port, work):
        self.port, self.work = port, work
        self.argv = ["./node_modules/.bin/vite", "--port", str(port), "--strictPort",
                     "--host", "127.0.0.1"]

    def cmd_a(self):
        return " ".join(self.argv)


def setup_vite(work):
    (work / "package.json").write_text('{"name":"bench-app","private":true,"type":"module"}\n')
    (work / "index.html").write_text(
        '<!doctype html><html><body><div id="app"></div>'
        '<script type="module" src="/main.js"></script></body></html>\n')
    (work / "main.js").write_text("document.getElementById('app').textContent = 'hello'\n")
    t0 = now()
    r = run(["npm", "install", "--no-audit", "--no-fund", "--silent", f"vite@{VITE_VERSION}"],
            cwd=work)
    if r.returncode != 0:
        sys.exit(f"npm install vite failed:\n{r.stderr}")
    return now() - t0


def one_round(mode, app, env, sample_mem):
    port, work = app.port, app.work
    assert port_free(port), f"port {port} busy before {mode}"
    mem = {}
    if mode == "A":
        t0 = now()
        pid = sh_start(app.cmd_a(), work, work / "nohup.log")
        t_ret = now() - t0
        t_up = wait_200(port, t0)
        if sample_mem:
            time.sleep(1)
            mem = {"server_rss_kb": tree_rss_kb(pid)}
        t1 = now()
        kill_quietly(pid)
        t_down = wait_free(port, t1)
    elif mode == "B":
        t0 = now()
        r = run([TABA, "run", "--scope", "bench", "--name", "web", "--port", str(port), "--",
                 *app.argv], cwd=work, env=env)
        t_ret = now() - t0
        assert r.returncode == 0, r.stderr
        t_up = wait_200(port, t0)
        if sample_mem:
            time.sleep(1)
            pid = int(run([TABA, "ps", "--scope", "bench", "--json"], env=env)
                      .stdout.split('"pid": ')[1].split(",")[0])
            leftover = run(["pgrep", "-x", "taba"]).stdout.split()
            mem = {"server_rss_kb": tree_rss_kb(pid), "taba_processes_after_run": len(leftover)}
        t1 = now()
        r = run([TABA, "stop", "bench/web"], env=env)
        assert r.returncode == 0, r.stderr
        t_down = wait_free(port, t1)
        run([TABA, "rm", "bench/web"], env=env)
    else:
        extra = ["--init"] if mode == "C-init" else []
        t0 = now()
        r = run(["docker", "run", "-d", "--label", LABEL, *extra, "-p", f"{port}:{port}",
                 IMAGE, "python", "-m", "http.server", str(port)])
        t_ret = now() - t0
        assert r.returncode == 0, r.stderr
        cid = r.stdout.strip()
        t_up = wait_200(port, t0)
        if sample_mem:
            time.sleep(1)
            top = run(["docker", "top", cid, "-o", "pid,rss,args"]).stdout.splitlines()[1:]
            st = run(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", cid]).stdout
            mem = {"server_rss_kb_in_vm": sum(int(l.split()[1]) for l in top),
                   "docker_top": top, "docker_stats_mem": st.strip()}
        t1 = now()
        r = run(["docker", "stop", cid])
        assert r.returncode == 0, r.stderr
        t_down = wait_free(port, t1)
        run(["docker", "rm", cid])
    return {"cli_return_s": t_ret, "start_to_200_s": t_up, "stop_to_free_s": t_down, **mem}


def docker_host_memory():
    procs = docker_host_procs()
    vm = [p for p in procs if "Virtualization.VirtualMachine" in p[2]]
    return {
        "host_process_count": len(procs),
        "host_rss_total_mb": round(sum(p[1] for p in procs) / 1024, 1),
        "vm_footprint_mb": footprint_mb(vm[0][0]) if vm else None,
        "docker_info_mem_total": run(["docker", "info", "--format", "{{.MemTotal}}"]).stdout.strip(),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=12)
    ap.add_argument("--port", type=int, default=18731)
    ap.add_argument("--vite", action="store_true")
    ap.add_argument("--modes", default=None)
    a = ap.parse_args()

    work = Path(tempfile.mkdtemp(prefix="taba-bench-")).resolve()
    env = {**os.environ, "TABA_HOME": str(work / "taba-home")}
    out = {"work_dir": str(work), "rounds": a.rounds, "port": a.port}
    if a.vite:
        app = Vite(a.port, work)
        out["npm_install_vite_s"] = setup_vite(work)
        out["vite_version"] = VITE_VERSION
        modes = (a.modes or "A,B").split(",")
    else:
        app = Python(a.port, work)
        (work / "index.html").write_text("<h1>bench</h1>\n")
        modes = (a.modes or "A,B,C,C-init").split(",")

    if any(m.startswith("C") for m in modes):
        out["docker_host_memory_before"] = docker_host_memory()

    runs = {m: [] for m in modes}
    try:
        for i in range(a.rounds):
            for m in modes:
                res = one_round(m, app, env, sample_mem=(i == 0))
                runs[m].append(res)
                print(f"round {i + 1:2d} {m:6s} up {res['start_to_200_s'] * 1000:8.1f} ms  "
                      f"down {res['stop_to_free_s'] * 1000:8.1f} ms", flush=True)
        if any(m.startswith("C") for m in modes):
            # One more container held running while sampling the host side.
            r = run(["docker", "run", "-d", "--label", LABEL, "-p", f"{a.port}:{a.port}",
                     IMAGE, "python", "-m", "http.server", str(a.port)])
            wait_200(a.port, now())
            time.sleep(2)
            out["docker_host_memory_with_container"] = docker_host_memory()
            run(["docker", "rm", "-f", r.stdout.strip()])
            wait_free(a.port, now())
    finally:
        if any(m.startswith("C") for m in modes):
            docker_bench_cleanup()
        run([TABA, "stop", "--scope", "bench"], env=env)

    out["taba_state_bytes"] = int(run(["du", "-sk", str(work / "taba-home")]).stdout.split()[0]) * 1024
    out["runs"] = runs
    out["summary"] = {
        m: {k: stats([r[k] for r in rs]) for k in ("cli_return_s", "start_to_200_s", "stop_to_free_s")}
        for m, rs in runs.items()
    }
    out["memory"] = {m: {k: v for k, v in rs[0].items() if not k.endswith("_s")} for m, rs in runs.items()}
    # taba's own peak memory while launching (it exits right after).
    r = run(["/usr/bin/time", "-l", TABA, "run", "--scope", "bench", "--name", "web", "--port",
             str(a.port), "--", *app.argv], cwd=work, env=env)
    for line in r.stderr.splitlines():
        if "maximum resident set size" in line:
            out["taba_run_peak_rss_bytes"] = int(line.split()[0])
    wait_200(a.port, now())
    run([TABA, "stop", "bench/web"], env=env)
    wait_free(a.port, now())
    run([TABA, "rm", "bench/web"], env=env)

    save(f"timing-{app.name}.json", out)
    for m, s in out["summary"].items():
        print(f"{m:6s} " + "  ".join(
            f"{k}: med {v['median'] * 1000:.0f} ms p90 {v['p90'] * 1000:.0f} ms" for k, v in s.items()))
    shutil.rmtree(work)


if __name__ == "__main__":
    main()
