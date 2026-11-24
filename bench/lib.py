"""Shared helpers for the taba vs Docker benchmarks (stdlib only)."""

import json
import math
import os
import signal
import socket
import statistics
import subprocess
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TABA = str(REPO / "target" / "release" / "taba")
IMAGE = "python:3-slim"
LABEL = "taba-bench=1"
RESULTS = REPO / "bench" / "results"


def now():
    return time.perf_counter()


def http_status(port, path="/", timeout=0.5):
    """Status code of GET http://127.0.0.1:port/path, or None on any failure."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as s:
            s.settimeout(timeout)
            s.sendall(f"GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n".encode())
            data = s.recv(64)
            if not data.startswith(b"HTTP/"):
                return None
            return int(data.split()[1])
    except (OSError, ValueError, IndexError):
        return None


def http_get(port, path="/", timeout=1.0):
    """Body of a GET, or None."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as s:
            s.settimeout(timeout)
            s.sendall(f"GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n".encode())
            chunks = []
            while True:
                c = s.recv(65536)
                if not c:
                    break
                chunks.append(c)
        raw = b"".join(chunks)
        head, _, body = raw.partition(b"\r\n\r\n")
        if not head.startswith(b"HTTP/1.0 200") and not head.startswith(b"HTTP/1.1 200"):
            return None
        return body
    except OSError:
        return None


def wait_200(port, t0, limit=60.0, path="/"):
    """Seconds from t0 until GET returns 200 (polls every 2 ms)."""
    while True:
        if http_status(port, path, timeout=0.5) == 200:
            return now() - t0
        if now() - t0 > limit:
            raise TimeoutError(f"no HTTP 200 on :{port} within {limit}s")
        time.sleep(0.002)


def port_free(port):
    """True when nothing accepts on 127.0.0.1:port and 0.0.0.0:port can be bound."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.2):
            return False
    except OSError:
        pass
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("0.0.0.0", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def wait_free(port, t0, limit=60.0):
    while True:
        if port_free(port):
            return now() - t0
        if now() - t0 > limit:
            raise TimeoutError(f":{port} still busy after {limit}s")
        time.sleep(0.002)


def run(cmd, **kw):
    kw.setdefault("capture_output", True)
    kw.setdefault("text", True)
    return subprocess.run(cmd, **kw)


def stats(xs):
    xs = sorted(xs)
    n = len(xs)
    p90 = xs[math.ceil(0.9 * n) - 1]  # nearest-rank
    return {
        "n": n,
        "median": statistics.median(xs),
        "p90": p90,
        "min": xs[0],
        "max": xs[-1],
    }


def rss_kb(pid):
    r = run(["ps", "-o", "rss=", "-p", str(pid)])
    return int(r.stdout.strip()) if r.stdout.strip() else None


def tree_pids(root):
    """root and all its descendants, from ps."""
    r = run(["ps", "-axo", "pid=,ppid="])
    kids = {}
    for line in r.stdout.splitlines():
        pid, ppid = map(int, line.split())
        kids.setdefault(ppid, []).append(pid)
    out, todo = [], [root]
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(kids.get(p, []))
    return out


def tree_rss_kb(root):
    return sum(rss_kb(p) or 0 for p in tree_pids(root))


def footprint_mb(pid):
    r = run(["footprint", str(pid)])
    for line in r.stdout.splitlines():
        if "Footprint:" in line:
            val, unit = line.split("Footprint:")[1].split()[:2]
            return float(val) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[unit]
    return None


def docker_host_procs():
    """(pid, rss_kb, command) of every host process that belongs to Docker Desktop."""
    r = run(["ps", "-axo", "pid=,rss=,command="])
    out = []
    for line in r.stdout.splitlines():
        pid, rss, cmd = line.strip().split(None, 2)
        if "/Applications/Docker.app/" in cmd or "com.apple.Virtualization.VirtualMachine" in cmd:
            out.append((int(pid), int(rss), cmd))
    return out


def docker_bench_cleanup():
    """Remove only containers carrying our label."""
    ids = run(["docker", "ps", "-aq", "--filter", f"label={LABEL}"]).stdout.split()
    if ids:
        run(["docker", "rm", "-f", *ids])
    return ids


def kill_quietly(pid, sig=signal.SIGTERM):
    try:
        os.kill(pid, sig)
    except ProcessLookupError:
        pass


def save(name, obj):
    RESULTS.mkdir(parents=True, exist_ok=True)
    (RESULTS / name).write_text(json.dumps(obj, indent=2) + "\n")


def save_text(name, text):
    RESULTS.mkdir(parents=True, exist_ok=True)
    (RESULTS / name).write_text(text)
