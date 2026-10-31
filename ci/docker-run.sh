#!/usr/bin/env bash
# Reproduces the scoring harness's exact `docker run` invocation locally, so our own
# timings are taken under the same constraints as the real run (cpuset, 8g mem/no swap,
# no network, read-only root, 1g tmpfs scratch).
#
# This is a faithful dry run of the OFFICIAL command only -- it does not attempt to
# recover memory.peak (the official harness's own cat of the container's cgroup path
# isn't something we can reproduce host-side after --rm tears it down; see bench.sh's
# header comment for why). For wall time + exec time + memory.peak measurement, use
# bench.sh against an image built with harness/mem_wrapper.sh as its entrypoint.
#
# Usage:
#   ./docker-run.sh <image> <data-dir> <out-dir> [track]
#
# Example:
#   ./docker-run.sh speeddemon/us:local ./data/track-a ./out/run1 A
set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <image> <data-dir> <out-dir> [track]" >&2
    exit 1
fi

IMAGE="$1"
DATA_DIR="$(cd "$2" && pwd)"
OUT_DIR="$3"
TRACK="${4:-}"

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

# NOTE: the real harness pins to the 6 P-cores (0-11 hyperthreaded) on an i5-13500.
# On a different dev machine, --cpuset-cpus=0-11 may not map onto P-cores at all, and
# a machine with fewer than 12 logical CPUs (e.g. a CI runner) can't satisfy "0-11" as
# a cpuset at all -- docker would error. CPUSET is overridable for exactly that case;
# unset, it defaults to the real harness's own "0-11".
CPUSET="${CPUSET:-0-11}"
docker run --rm \
    --cpuset-cpus="$CPUSET" \
    --cpuset-mems="0" \
    --memory=8g --memory-swap=8g \
    --pids-limit=512 \
    --network=none \
    --read-only \
    --tmpfs /tmp:rw,size=1g,exec \
    --ulimit nofile=8192:8192 \
    -v "${DATA_DIR}:/data:ro" \
    -v "${OUT_DIR}:/out:rw" \
    -e "TRACK=${TRACK}" \
    "$IMAGE"
