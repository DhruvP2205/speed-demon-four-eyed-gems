#!/usr/bin/env bash
# 5 runs, drop highest/lowest, mean of the middle three -> T (and, by the same
# rule applied independently, M and the binary's own exec time). SHA-256 gate
# on every run's output. Auto-reruns the whole batch once if the kept wall
# times' spread exceeds 5%, mirroring the published timing protocol.
#
# WHY NOT just `docker run --rm ...` + read memory.peak after?
# The OCI runtime (runc) tears down a container's cgroup as part of its own
# exit cleanup the moment the wrapped process exits -- not when `docker rm`
# runs, and not conditional on whether `--rm` was passed to `docker run`. By
# the time our shell regains control after `docker start -a` / `docker run`
# returns, the cgroup (and anything in a tmpfs like /tmp) may already be
# gone, host-side path or not. The only observation point that reliably
# survives is INSIDE the container, before its own process tree exits.
#
# So this harness uses an in-container wrapper (mem_wrapper.sh) as the image's
# entrypoint: it times the real binary with its own clock and reads this
# container's own /sys/fs/cgroup/memory.peak (no host-side cgroup-driver/path
# guessing needed), writing both to files under /out -- a real bind-mounted
# host directory, not tmpfs, so it survives container exit. This means the
# image under test must be built with mem_wrapper.sh as its entrypoint (see
# harness/smoketest/Dockerfile for the prod/bench multi-stage pattern); plain
# submission images (no wrapper) can still be run through docker-run.sh for a
# faithful dry run of the official invocation, just not through bench.sh.
#
# This gives two time numbers, both reported:
#   wall    -- full `docker run` time, host-measured (container create/start/
#              teardown + the binary). Closest proxy to what the official
#              harness measures (which times "from process exec to exit" on
#              ITS side, i.e. roughly this, modulo docker's own overhead).
#   exec    -- just the wrapped binary's own start-to-exit time, wrapper-
#              measured. Useful for iterating on the program itself without
#              paying container spin-up/teardown cost on every measurement.
#
# Usage:
#   ./bench.sh <image> <data-dir> <out-base-dir> <expected-sha256-file> <track> [output-filename]
#
# <image> must be built from an entrypoint chain through mem_wrapper.sh.
# <expected-sha256-file> is a text file containing just the expected hex digest.
#
# Requires: docker, python3, bash 4+.
set -euo pipefail

if [[ $# -lt 5 ]]; then
    echo "usage: $0 <image> <data-dir> <out-base-dir> <expected-sha256-file> <track> [output-filename]" >&2
    exit 1
fi

IMAGE="$1"
DATA_DIR="$(cd "$2" && pwd)"
OUT_BASE="$3"
EXPECTED_SHA_FILE="$4"
TRACK="$5"
OUT_FILENAME="${6:-output}"

EXPECTED_SHA="$(tr -d '[:space:]' < "$EXPECTED_SHA_FILE")"

# Overridable for machines with fewer than 12 logical CPUs (e.g. a CI
# runner), where the real harness's own "0-11" would error outright.
CPUSET="${CPUSET:-0-11}"

# Overridable so the rerun-on-spread path can actually be exercised (set
# e.g. BENCH_SPREAD_THRESHOLD=0.001 to force it on noisy hardware/CI,
# where real-world spread almost always exceeds a threshold that low).
BENCH_SPREAD_THRESHOLD="${BENCH_SPREAD_THRESHOLD:-0.05}"

mkdir -p "$OUT_BASE"
OUT_BASE="$(cd "$OUT_BASE" && pwd)"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        python3 -c "import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest())" "$1"
    fi
}

run_once() {
    local run_idx="$1"
    local run_out="${OUT_BASE}/run${run_idx}"
    mkdir -p "$run_out"
    rm -f "${run_out}/.bench_time_ns" "${run_out}/.bench_mem_peak"

    local cid
    cid="$(docker create \
        --cpuset-cpus="$CPUSET" \
        --cpuset-mems="0" \
        --memory=8g --memory-swap=8g \
        --pids-limit=512 \
        --network=none \
        --read-only \
        --tmpfs /tmp:rw,size=1g,exec \
        --ulimit nofile=8192:8192 \
        -v "${DATA_DIR}:/data:ro" \
        -v "${run_out}:/out:rw" \
        -e "TRACK=${TRACK}" \
        -e "BENCH_TIME_FILE=/out/.bench_time_ns" \
        -e "BENCH_MEM_PEAK_FILE=/out/.bench_mem_peak" \
        "$IMAGE")"

    local t_start t_end wall exit_code
    t_start="$(python3 -c 'import time; print(time.time())')"
    set +e
    docker start -a "$cid" >/dev/null
    exit_code=$?
    set -e
    t_end="$(python3 -c 'import time; print(time.time())')"
    wall="$(python3 -c "print(${t_end} - ${t_start})")"

    if [[ "$exit_code" -ne 0 ]]; then
        echo "run ${run_idx}: FAILED (exit ${exit_code})" >&2
        docker rm "$cid" >/dev/null
        return 1
    fi
    docker rm "$cid" >/dev/null

    local out_file="${run_out}/${OUT_FILENAME}"
    if [[ ! -f "$out_file" ]]; then
        echo "run ${run_idx}: FAILED (no output file at ${out_file})" >&2
        return 1
    fi
    local got_sha
    got_sha="$(sha256_of "$out_file")"
    if [[ "$got_sha" != "$EXPECTED_SHA" ]]; then
        echo "run ${run_idx}: FAILED (sha256 mismatch: got ${got_sha}, want ${EXPECTED_SHA})" >&2
        return 1
    fi

    if [[ ! -f "${run_out}/.bench_time_ns" ]]; then
        echo "run ${run_idx}: FAILED (no .bench_time_ns -- was the image built with mem_wrapper.sh as entrypoint?)" >&2
        return 1
    fi
    local exec_ns exec_s mem_peak
    exec_ns="$(tr -d '[:space:]' < "${run_out}/.bench_time_ns")"
    exec_s="$(python3 -c "print(${exec_ns} / 1e9)")"
    mem_peak="$(tr -d '[:space:]' < "${run_out}/.bench_mem_peak" 2>/dev/null || echo -1)"

    echo "${wall} ${exec_s} ${mem_peak}"
}

do_batch() {
    local -a walls=() execs=() mems=()
    for i in 1 2 3 4 5; do
        local result
        if ! result="$(run_once "$i")"; then
            echo "batch aborted: run $i failed the correctness or execution gate" >&2
            exit 1
        fi
        local w e m
        w="$(echo "$result" | awk '{print $1}')"
        e="$(echo "$result" | awk '{print $2}')"
        m="$(echo "$result" | awk '{print $3}')"
        walls+=("$w"); execs+=("$e"); mems+=("$m")
        echo "run ${i}: wall=${w}s  exec=${e}s  mem_peak=${m} bytes" >&2
    done

    BENCH_SPREAD_THRESHOLD="$BENCH_SPREAD_THRESHOLD" python3 - "${walls[@]}" -- "${execs[@]}" -- "${mems[@]}" <<'PYEOF'
import os
import sys

args = sys.argv[1:]
sep1 = args.index("--")
sep2 = args.index("--", sep1 + 1)
walls = [float(x) for x in args[:sep1]]
execs = [float(x) for x in args[sep1+1:sep2]]
mems = [float(x) for x in args[sep2+1:]]
threshold = float(os.environ.get("BENCH_SPREAD_THRESHOLD", "0.05"))

def mid3_mean(values):
    s = sorted(values)
    kept = s[1:-1]
    return sum(kept) / len(kept), kept

wall_mean, wall_kept = mid3_mean(walls)
exec_mean, exec_kept = mid3_mean(execs)
mem_mean, mem_kept = mid3_mean(mems)

wall_spread = (max(wall_kept) - min(wall_kept)) / wall_mean if wall_mean else 0.0

print(f"wall T (mean of middle 3): {wall_mean:.4f} s   kept={wall_kept}")
print(f"exec T (mean of middle 3): {exec_mean:.4f} s   kept={exec_kept}")
print(f"M     (mean of middle 3): {mem_mean:.0f} bytes   kept={mem_kept}")
# .4f rather than .2f: BENCH_SPREAD_THRESHOLD can legitimately be set as
# low as 0.000001 (see the CI step that forces the rerun path on purpose)
# -- at .2f precision that threshold prints as a misleading "0.00%".
print(f"wall T spread: {wall_spread*100:.4f}% (threshold {threshold*100:.4f}%)")
# Single machine-readable line this script's own caller parses below to
# build the unambiguous FINAL line -- not meant for external consumers
# (the human-readable lines above carry the same numbers).
print(f"__RESULT__ {wall_mean} {exec_mean} {mem_mean:.0f}")
if wall_spread > threshold:
    print("SPREAD_EXCEEDED")
    sys.exit(2)
PYEOF
}

print_final() {
    local batch_num="$1"
    local output="$2"
    local result_line wall_mean exec_mean mem_mean
    result_line="$(echo "$output" | grep '^__RESULT__')"
    wall_mean="$(echo "$result_line" | awk '{print $2}')"
    exec_mean="$(echo "$result_line" | awk '{print $3}')"
    mem_mean="$(echo "$result_line" | awk '{print $4}')"
    echo "FINAL (batch ${batch_num}): wall T=${wall_mean}s  exec T=${exec_mean}s  M=${mem_mean} bytes"
}

threshold_pct() {
    # awk, not python3, for a trivial unit conversion -- awk is already a
    # hard dependency of this script (sha256_of's primary path), so this
    # doesn't add a new one, and avoids the whole python3-vs-python/PATH
    # question for something this small.
    awk "BEGIN { printf \"%.4f\", ${BENCH_SPREAD_THRESHOLD} * 100 }"
}

echo "=== batch 1 ===" >&2
# NOTE: must branch on $? captured in the `else` clause of a bare (un-negated)
# `if out=$(do_batch); then`, not via `if ! do_batch; then status=$?`.
# Negating with `!` makes bash rewrite the exit status to 0 or 1 (never the
# original value -- see `help [[` / POSIX "Pipelines"), so a prior version of
# this script could never actually distinguish "spread exceeded" (2) from a
# hard failure (1): every failure looked like status=0 and silently fell
# through to `exit 0`, meaning the rerun NEVER ran even when SPREAD_EXCEEDED
# was printed. Fixed, and verified against all four (status, capture)
# combinations with a mocked do_batch before trusting it here.
if batch1_output="$(do_batch)"; then
    status=0
else
    status=$?
fi
echo "$batch1_output" | { grep -v '^__RESULT__' || true; }

if [[ "$status" -eq 0 ]]; then
    print_final 1 "$batch1_output"
    exit 0
fi
if [[ "$status" -ne 2 ]]; then
    exit "$status"
fi

echo "wall T spread exceeded the ${BENCH_SPREAD_THRESHOLD} threshold ($(threshold_pct)%), re-running batch once (per timing protocol)..." >&2
echo "=== batch 2 (rerun) ===" >&2
if batch2_output="$(do_batch)"; then
    status=0
else
    status=$?
fi
echo "$batch2_output" | { grep -v '^__RESULT__' || true; }

if [[ "$status" -eq 0 ]]; then
    print_final 2 "$batch2_output"
    exit 0
fi
if [[ "$status" -eq 2 ]]; then
    echo "wall T spread STILL exceeded the ${BENCH_SPREAD_THRESHOLD} threshold ($(threshold_pct)%) after the rerun -- reporting batch 2's numbers anyway (timing protocol allows one retry, not unlimited retries); treat this measurement as noisy rather than re-running again" >&2
    print_final 2 "$batch2_output"
    exit 0
else
    exit "$status"
fi
