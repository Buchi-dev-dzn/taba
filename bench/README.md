# bench

Scripts behind [docs/benchmarks.md](../docs/benchmarks.md). Python 3 stdlib only;
no hyperfine. Build taba first: `cargo build --release`.

| Script | What it measures | Output |
| --- | --- | --- |
| `pull.sh` | `docker pull python:3-slim` time and image size | `results/pull.txt` |
| `timing.py` | start → first HTTP 200, stop → port free, server RSS (N rounds, modes interleaved) | `results/timing-python.json` |
| `timing.py --vite --port 18733` | same for a minimal Vite app (local `npm install`, no global installs); taba and nohup only | `results/timing-vite.json` |
| `memory.py` | Docker Desktop host memory (RSS and `footprint`), idle and with one container | `results/memory-docker.json` |
| `scenarios.py` | re-runs, port conflicts, visibility, working-tree access, bind-mount read speed, logs after the terminal closes, disk | `results/scenarios.md` |
| `fsread.py` | helper for the read-speed test (runs on the host and inside the container) | |

`run-all.sh` runs them in order. Every container gets the label `taba-bench=1`
and is removed by that label; taba runs with a throwaway `TABA_HOME`. Ports
18731–18738 must be free.
