#!/usr/bin/env bash
# Runs every scenario against a fixed workload and checks the recordings hold
# real data.
#
# A scenario that exits 0 and writes every parquet file can still have collected
# nothing: a sampling group the PMU never scheduled, or one the kernel throttled,
# produces a recording with no `pmu_*` columns or with counters orders of
# magnitude low, and neither says so. This asserts what the recording contains,
# not what the exit status claims.
#
# usage: scenario-smoke.sh <mperf> [output-directory]
#   MPERF_SMOKE_QEMU        qemu-user binary, enables the mem and roofline checks
#   MPERF_SMOKE_QEMU_PLUGIN miniperf QEMU roofline plugin
#   MPERF_SMOKE_DYNAMORIO   DynamoRIO bundle, enables the roofline DynamoRIO check
#   MPERF_SMOKE_WORKLOAD    command to profile (default: a counted shell loop)
#   MPERF_SMOKE_ITERATIONS  iterations of the default workload (default 900000,
#                           roughly 3s on a desktop x86 and 4s on a slow board)
set -uo pipefail

mperf="${1:?usage: scenario-smoke.sh <mperf> [output-directory]}"
output_directory="${2:-$(mktemp -d "${TMPDIR:-/tmp}/mperf-smoke.XXXXXX")}"
iterations="${MPERF_SMOKE_ITERATIONS:-900000}"
mkdir -p "${output_directory}"

failures=0
skipped=0

# A workload has to burn CPU: `sleep` retires nothing and makes a working
# profiler look broken. The loop is counted rather than timed because `$SECONDS`
# is a bashism and `/bin/sh` is dash on Debian and Ubuntu, where a timed loop
# exits instantly and every check then fails on an empty recording. It also
# avoids forking a helper per iteration, whose work lands in a child process and
# goes uncounted wherever the kernel refuses inherited sampling groups.
if [[ -n "${MPERF_SMOKE_WORKLOAD:-}" ]]; then
    read -r -a workload <<<"${MPERF_SMOKE_WORKLOAD}"
else
    workload=(/bin/sh -c "i=0; while [ \$i -lt ${iterations} ]; do i=\$((i+1)); done")
fi

query() {
    "${mperf}" query -f json "$1" "$2" 2>/dev/null |
        python3 -c 'import json,sys
try:
    rows = json.load(sys.stdin)["rows"]
except Exception:
    sys.exit(1)
print(json.dumps(rows[0]) if rows else "{}")'
}

report() {
    printf '%-26s %-7s %s\n' "$1" "$2" "$3"
}

# Reports pass/fail for one recording: samples present, hardware counters
# present, a plausible instructions-per-cycle, and cycles consistent with the
# CPU time the run consumed.
#
# The clock check is the one with teeth. IPC does not move when every counter
# shrinks together, which is exactly what kernel sample-rate throttling does: a
# throttled TMA recording reported 4.3M cycles for a run that retired 8.8G, at a
# perfectly ordinary IPC of 0.67. Cycles divided by the time the counters were
# actually running has to land in the range a real core runs at, and a
# throttled group fails it by three orders of magnitude because its running
# time keeps accruing while the counters stand still.
check_recording() {
    local name="$1" directory="$2"
    local row
    row="$(query "${directory}" "SELECT COUNT(*) AS samples, SUM(pmu_cycles) AS cycles, SUM(pmu_instructions) AS insns, SUM(time_running) AS running_ns FROM pmu_counters")"
    if [[ -z "${row}" || "${row}" == "{}" ]]; then
        report "${name}" "FAIL" "no pmu_counters rows; the recording has no hardware counters"
        failures=$((failures + 1))
        return
    fi
    python3 - "${name}" "${row}" <<'PY'
import json
import sys

name, row = sys.argv[1], json.loads(sys.argv[2])
samples = row.get("samples") or 0
cycles = row.get("cycles") or 0
insns = row.get("insns") or 0

if samples == 0:
    print(f"{name:<26} FAIL    zero samples")
    raise SystemExit(1)
if cycles == 0 or insns == 0:
    print(f"{name:<26} FAIL    {samples} samples but cycles={cycles} instructions={insns}")
    raise SystemExit(1)
ipc = insns / cycles
if not 0.01 <= ipc <= 16:
    print(f"{name:<26} FAIL    implausible IPC {ipc:.3f} (cycles={cycles} instructions={insns})")
    raise SystemExit(1)

running_ns = row.get("running_ns") or 0
if running_ns == 0:
    print(f"{name:<26} FAIL    counters report no running time")
    raise SystemExit(1)
ghz = cycles / running_ns
if not 0.2 <= ghz <= 8.0:
    print(
        f"{name:<26} FAIL    {cycles} cycles over {running_ns / 1e9:.2f}s of counter runtime "
        f"implies {ghz:.3f} GHz; the counters were throttled or never scheduled"
    )
    raise SystemExit(1)
print(f"{name:<26} ok      {samples} samples, {cycles} cycles, IPC {ipc:.2f}, {ghz:.2f} GHz")
PY
    [[ $? -eq 0 ]] || failures=$((failures + 1))
}

run_scenario() {
    local name="$1" directory="${output_directory}/$1"
    shift
    rm -rf "${directory}"
    if ! "${mperf}" record -s "${name}" -o "${directory}" "$@" -- "${workload[@]}" \
        >"${output_directory}/${name}.log" 2>&1; then
        report "${name}" "FAIL" "record exited non-zero; see ${output_directory}/${name}.log"
        failures=$((failures + 1))
        return 1
    fi
    return 0
}

printf 'workload: %s\n\n' "${workload[*]}"

# `doctor` exits non-zero when it finds a blocker, which says nothing about the
# PMU, so read its report rather than its status.
doctor_report="$("${mperf}" doctor 2>/dev/null || true)"
if ! grep -q "hardware counters .*opens" <<<"${doctor_report}"; then
    echo "no PMU on this host; scenario checks skipped"
    exit 0
fi
grep -E "sampling rate ceiling|sampling group" <<<"${doctor_report}" |
    sed 's/^| */doctor: /; s/ *|.*$//' || true

for scenario in snapshot tma; do
    if run_scenario "${scenario}"; then
        check_recording "${scenario}" "${output_directory}/${scenario}"
    fi
done

accounting=()
if [[ -n "${MPERF_SMOKE_QEMU:-}" && -n "${MPERF_SMOKE_QEMU_PLUGIN:-}" ]]; then
    accounting=(--qemu "${MPERF_SMOKE_QEMU}" --qemu-plugin "${MPERF_SMOKE_QEMU_PLUGIN}")
elif [[ -n "${MPERF_SMOKE_DYNAMORIO:-}" ]]; then
    accounting=(--dynamorio "${MPERF_SMOKE_DYNAMORIO}")
fi

if [[ ${#accounting[@]} -gt 0 ]]; then
    for scenario in mem roofline; do
        if run_scenario "${scenario}" "${accounting[@]}"; then
            check_recording "${scenario}" "${output_directory}/${scenario}"
        fi
    done
else
    report "mem, roofline" "skip" "set MPERF_SMOKE_QEMU + MPERF_SMOKE_QEMU_PLUGIN, or MPERF_SMOKE_DYNAMORIO"
    skipped=$((skipped + 2))
fi

printf '\n%d failure(s), %d skipped. Recordings in %s\n' "${failures}" "${skipped}" "${output_directory}"
[[ "${failures}" -eq 0 ]]
