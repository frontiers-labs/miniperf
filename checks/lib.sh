# Shared helpers for checks/<name>. Sourced, never executed.
#
# Exit codes: 0 pass, 1 fail, 3 skip (no PMU on this host). Only the PMU
# oracle may produce 3, so a skip can never come from miniperf's own code.
set -uo pipefail

: "${MPERF_PACKAGE:?set MPERF_PACKAGE to an unpacked package root}"
CHECK_WORK="${CHECK_WORK:-$(mktemp -d "${TMPDIR:-/tmp}/mperf-check.XXXXXX")}"
mkdir -p "${CHECK_WORK}/bin"
checks_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mperf="${MPERF_PACKAGE}/bin/mperf"
mperf_gui="${MPERF_PACKAGE}/bin/mperf-gui"
[[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* ]] && mperf_gui="${MPERF_PACKAGE}/mperf-gui.exe"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf 'SKIP: %s\n' "$*" >&2; exit 3; }
note() { printf '  %s\n' "$*"; }

# Compiles checks/fixtures/<name>.c once and echoes the binary path.
cc_fixture() {
    local name="$1"; shift
    local out="${CHECK_WORK}/bin/${name}"
    if [[ ! -x "${out}" ]]; then
        # Compilation failure must not leave a caller holding an empty path:
        # `fail` inside a command substitution exits only the subshell.
        cc -O2 -g -fno-omit-frame-pointer -o "${out}" \
            "${checks_root}/fixtures/${name}.c" "$@" >&2 || return 1
    fi
    printf '%s' "${out}"
}

# True when the kernel gives a 40-line C program a working cycles counter.
# Deliberately independent of miniperf: a regression in libprof's capability
# probing must not be able to look like absent hardware and be skipped.
pmu_oracle() {
    local oracle
    oracle="$(cc_fixture pmu_oracle)" || return 1
    "${oracle}"
}

require_pmu() {
    pmu_oracle || skip "no hardware counters on this host"
}

# The workload every recording check profiles. Counted, not timed: $SECONDS is
# a bashism and /bin/sh is dash here, where a timed loop exits instantly.
workload() {
    printf '%s\n' "/bin/sh" "-c" "i=0; while [ \$i -lt ${1:-900000} ]; do i=\$((i+1)); done"
}

# Records <scenario> once into CHECK_WORK and echoes the directory. Memoised on
# info.json existing, so a check can run alone and reruns are free.
need_recording() {
    local scenario="$1"; shift
    local dir="${CHECK_WORK}/rec-${scenario}"
    if [[ ! -f "${dir}/info.json" ]]; then
        local -a cmd
        if [[ $# -gt 0 ]]; then cmd=("$@"); else mapfile -t cmd < <(workload); fi
        "${mperf}" record -s "${scenario}" -o "${dir}" -- "${cmd[@]}" >"${dir}.log" 2>&1 \
            || { cat "${dir}.log" >&2; fail "record -s ${scenario} failed"; }
    fi
    printf '%s' "${dir}"
}

# Runs one SQL statement through `mperf query` and fails naming every column
# that did not come back true. Assertions are therefore SQL a reviewer can
# paste into `mperf query` by hand, and every check exercises the product's own
# analysis path on the way to its verdict.
assert_sql() {
    local dir="$1" sql="$2"
    local json
    json="$("${mperf}" query -f json "${dir}" "${sql}" 2>&1)" \
        || { printf '%s\n' "${json}" >&2; fail "query failed"; }
    printf '%s' "${json}" | python3 -c '
import json, sys
payload = sys.stdin.read()
try:
    rows = json.loads(payload)["rows"]
except Exception:
    sys.exit(f"query returned no rows object:\n{payload}")
if not rows:
    sys.exit("query returned zero rows")
row = rows[0]
# DuckDB reports booleans as 1/0 through the JSON envelope, so accept either.
def holds(value):
    return value is True or value == 1
bad = [k for k, v in row.items() if not holds(v)]
if bad:
    sys.exit("assertions failed: " + ", ".join(bad) + "\nrow: " + json.dumps(row, indent=2))
' || fail "$(printf 'SQL assertion failed\n%s' "${sql}")"
}
