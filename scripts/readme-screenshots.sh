#!/usr/bin/env bash
# Regenerate the README screenshots in docs/images/ from real taba output.
#
# Runs the release binary against python3 http.server sample services under a
# throwaway TABA_HOME, captures each step's terminal output verbatim, renders
# it with scripts/term2svg.py, then stops everything and removes the demo dir.
#
#   scripts/readme-screenshots.sh            # uses /Users/Shared/demo
#   DEMO_ROOT=/some/dir scripts/readme-screenshots.sh
#
# The demo directories show up in the screenshots, so pick a neutral path.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
DEMO_ROOT=${DEMO_ROOT:-/Users/Shared/demo}
OUT="$REPO/docs/images"
CAP="$DEMO_ROOT/captures"

if [ -e "$DEMO_ROOT" ]; then
  echo "refusing to reuse existing $DEMO_ROOT" >&2
  exit 1
fi

cargo build --release --manifest-path "$REPO/Cargo.toml" >/dev/null
export PATH="$REPO/target/release:$PATH"
export TABA_HOME="$DEMO_ROOT/taba-home"
# Treat the demo's parent as $HOME so screenshots show `~/...` paths without
# revealing the real home directory.
export HOME=$(dirname "$DEMO_ROOT")
unset TABA_SCOPE
mkdir -p "$DEMO_ROOT/shop-login-fix" "$DEMO_ROOT/shop-billing" "$CAP" "$OUT"

UNMANAGED_PID=""
cleanup() {
  taba stop --scope login-fix >/dev/null 2>&1 || true
  taba stop --scope billing >/dev/null 2>&1 || true
  [ -n "$UNMANAGED_PID" ] && kill "$UNMANAGED_PID" 2>/dev/null || true
  rm -rf "$DEMO_ROOT"
}
trap cleanup EXIT

# step FILE CMD... : append "$ CMD" and its combined output to FILE.
step() {
  local file=$1; shift
  echo "\$ $*" >>"$CAP/$file"
  "$@" >>"$CAP/$file" 2>&1 || true
}
# show_cd FILE DIR : record and perform a cd.
show_cd() { echo "\$ cd $2" >>"$CAP/$1"; cd "$2"; }

# 1. run: two checkouts, two scopes; then an idempotent re-run.
show_cd run.txt "$DEMO_ROOT/shop-login-fix"
step run.txt taba run --scope login-fix --name web --port 8001 -- python3 -u -m http.server 8001
show_cd run.txt "$DEMO_ROOT/shop-billing"
step run.txt taba run --scope billing --name web --port 8002 -- python3 -u -m http.server 8002
step run.txt taba run --scope billing --name api --port 8003 -- python3 -u -m http.server 8003
step run.txt taba run --scope billing --name web --port 8002 -- python3 -u -m http.server 8002
sleep 1

# 2. ps: the hero shot.
step ps.txt taba ps

# 3. port conflicts: one held by taba, one by an unmanaged process.
( cd "$DEMO_ROOT" && exec python3 -m http.server 9000 >/dev/null 2>&1 ) &
UNMANAGED_PID=$!
disown "$UNMANAGED_PID"
sleep 1
step port-conflict.txt taba run --scope billing --name admin --port 8001 -- python3 -u -m http.server 8001
step port-conflict.txt taba run --scope billing --name admin --port 9000 -- python3 -u -m http.server 9000

# 4. logs: make one request so there is something to show.
curl -s -o /dev/null http://localhost:8002/
sleep 0.5
step logs.txt taba logs billing/web

# 5. stop a whole scope; the other scope (and the unmanaged server) keep running.
step stop-scope.txt taba stop --scope billing
step stop-scope.txt taba ps

# 6. rm: clean up the stopped scope's records and logs.
step rm.txt taba rm --scope billing
step rm.txt taba ps

# 7. JSON for agents (kept as text only, quoted in the README).
step ps-json.txt taba ps --scope login-fix --json

for name in run ps port-conflict logs stop-scope rm; do
  python3 "$REPO/scripts/term2svg.py" "$CAP/$name.txt" "$OUT/$name.svg" --title "taba — $name"
  cp "$CAP/$name.txt" "$OUT/$name.txt"
done
cp "$CAP/ps-json.txt" "$OUT/ps-json.txt"
echo "wrote:"; ls -1 "$OUT"
