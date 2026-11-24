#!/bin/sh
# Pull python:3-slim once and record whether it was present before, the pull
# time and the image size. Writes bench/results/pull.txt.
set -eu
cd "$(dirname "$0")/.."
mkdir -p bench/results
out=bench/results/pull.txt
img=python:3-slim
if docker image inspect "$img" >/dev/null 2>&1; then
  echo "present_before=yes" > "$out"
else
  echo "present_before=no" > "$out"
fi
start=$(python3 -c 'import time; print(time.time())')
docker pull -q "$img" >/dev/null
end=$(python3 -c 'import time; print(time.time())')
python3 -c "print(f'pull_seconds={$end - $start:.1f}')" >> "$out"
echo "image_id=$(docker image inspect --format '{{.Id}}' "$img")" >> "$out"
echo "image_size_bytes=$(docker image inspect --format '{{.Size}}' "$img")" >> "$out"
echo "python_version_in_image=$(docker run --rm --label taba-bench=1 "$img" python --version)" >> "$out"
docker image ls "$img" >> "$out"
cat "$out"
