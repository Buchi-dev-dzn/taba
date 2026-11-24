#!/bin/sh
# Run every benchmark. Assumes Docker Desktop is running and taba is built.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
bench/pull.sh
python3 bench/timing.py --rounds 15
python3 bench/timing.py --vite --rounds 12 --port 18733
python3 bench/memory.py
python3 bench/scenarios.py
# Leftover check: nothing of ours should remain.
docker ps -aq --filter label=taba-bench=1
for p in 18731 18732 18733 18734 18735 18736 18737 18738; do
  lsof -nP -iTCP:$p -sTCP:LISTEN || true
done
echo "done; remove python:3-slim yourself only if you pulled it for this run"
