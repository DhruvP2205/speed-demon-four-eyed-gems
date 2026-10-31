#!/bin/sh
# Entrypoint wrapper for LOCAL benchmarking only. Exec's the real binary,
# then (if asked, via env vars) records two diagnostics a host-side `docker
# run --rm ... ; docker stats` can never recover, because the container's
# cgroup and mounts are torn down by the OCI runtime the moment the process
# exits -- before `docker rm` even runs, regardless of whether --rm was
# passed to `docker run`. See bench.sh's header comment for the full story.
#
# BENCH_TIME_FILE      if set, write the wrapped command's wall time in
#                      nanoseconds (wrapper-measured, via `date +%s%N`,
#                      i.e. wall-clock, not a true monotonic clock -- there
#                      is no monotonic-clock primitive in POSIX sh/coreutils;
#                      over a run lasting seconds this is what everyone
#                      means by "monotonic clock in the wrapper" in practice)
# BENCH_MEM_PEAK_FILE  if set, write /sys/fs/cgroup/memory.peak (this
#                      container's own cgroup, read from the inside -- no
#                      host-side cgroup-driver/path guessing needed) to it
#
# Both are written to paths under /out, which is bind-mounted to a real host
# directory (unlike /tmp, which is tmpfs and dies with the container's mount
# namespace). Neither env var is set by the official scoring harness or by
# docker-run.sh (which mirrors it exactly), so in the real scored run this
# wrapper is a silent passthrough: no extra files, no extra output.
#
# Usage in a Dockerfile (bench-only image variant -- do NOT ship this in the
# submission image; see harness/smoketest/Dockerfile for the prod/bench
# multi-stage pattern):
#   COPY mem_wrapper.sh /mem_wrapper.sh
#   ENTRYPOINT ["/mem_wrapper.sh", "/app/run"]

start_ns=$(date +%s%N)
"$@"
code=$?
end_ns=$(date +%s%N)

if [ -n "${BENCH_TIME_FILE:-}" ]; then
    echo $((end_ns - start_ns)) > "$BENCH_TIME_FILE"
fi

if [ -n "${BENCH_MEM_PEAK_FILE:-}" ]; then
    cat /sys/fs/cgroup/memory.peak > "$BENCH_MEM_PEAK_FILE" 2>/dev/null || true
fi

exit $code
