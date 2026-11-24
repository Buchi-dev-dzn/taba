#!/usr/bin/env python3
"""Host-side memory of Docker Desktop, idle and with one bench container.

RSS undercounts the Linux VM (guest memory is mapped, not resident in the
usual sense), so the macOS `footprint` tool is used as well. Five samples
each, 2 s apart; medians are reported. Writes bench/results/memory-docker.json.
"""

import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import (IMAGE, LABEL, docker_bench_cleanup, docker_host_procs,  # noqa: E402
                 footprint_mb, now, run, save, wait_200, wait_free)

PORT = 18738


def sample():
    procs = docker_host_procs()
    vm = [p for p in procs if "Virtualization.VirtualMachine" in p[2]]
    return {
        "processes": len(procs),
        "rss_total_mb": sum(p[1] for p in procs) / 1024,
        "footprint_total_mb": sum(footprint_mb(p[0]) or 0 for p in procs),
        "vm_footprint_mb": footprint_mb(vm[0][0]) if vm else None,
    }


def series(n=5):
    xs = []
    for _ in range(n):
        xs.append(sample())
        time.sleep(2)
    return {k: statistics.median(x[k] for x in xs) for k in xs[0]} | {"samples": xs}


def main():
    running = run(["docker", "ps", "-q"]).stdout.split()
    out = {"containers_running_before": len(running),
           "vm_memory_limit_bytes": int(run(["docker", "info", "--format", "{{.MemTotal}}"]).stdout)}
    out["idle"] = series()
    r = run(["docker", "run", "-d", "--label", LABEL, "-p", f"{PORT}:{PORT}", IMAGE,
             "python", "-m", "http.server", str(PORT)])
    try:
        wait_200(PORT, now())
        time.sleep(2)
        out["one_container"] = series()
    finally:
        docker_bench_cleanup()
        wait_free(PORT, now())
    save("memory-docker.json", out)
    for k in ("idle", "one_container"):
        print(k, {a: round(b, 1) for a, b in out[k].items() if a != "samples"})


if __name__ == "__main__":
    main()
