#!/bin/sh
# Runs the profiler against real hardware. Blocks commits when it fails.
#
# Stages the debug binaries into a package-shaped tree and runs the checks that
# prove the profiler still records and the viewer still opens what it recorded.
# Debug, not a release package: the gate has to be fast enough to run on every
# commit, and these checks exercise the same code paths either way. CI runs the
# full set against a real package.
set -e
cd "$(dirname "$0")/.."

cargo build -q -p mperf -p mperf-gui

stage="${CARGO_TARGET_DIR:-target}/gate-package"
mkdir -p "${stage}/bin" "${stage}/lib/miniperf"
for binary in mperf mperf-gui; do
    cp -f "target/debug/${binary}" "${stage}/bin/${binary}"
done
for library in target/debug/libmperf_collector.so target/debug/libmperf_libc.so; do
    [ -e "${library}" ] && cp -f "${library}" "${stage}/lib/miniperf/" || true
done

MPERF_PACKAGE="$(cd "${stage}" && pwd)" exec checks/run cli pmu-snapshot query
