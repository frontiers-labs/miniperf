#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 /path/to/tir" >&2
    exit 2
fi

tir=$(realpath "$1")
output=$(realpath "$(dirname "$0")")/spec
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

inputs=(
    backends/riscv/defs/main.tmdl
    backends/riscv/defs/base.tmdl
    backends/riscv/defs/multiplication.tmdl
    backends/riscv/defs/float.tmdl
    backends/riscv/defs/float_extra.tmdl
    backends/riscv/defs/float_round.tmdl
    backends/riscv/defs/compressed.tmdl
    backends/riscv/defs/atomics.tmdl
    backends/riscv/defs/zifencei.tmdl
    backends/riscv/defs/zicsr.tmdl
    backends/riscv/defs/perf.tmdl
    backends/riscv/defs/vector.tmdl
    backends/riscv/defs/vector_int.tmdl
    backends/riscv/defs/vector_mask.tmdl
    backends/riscv/defs/vector_red.tmdl
    backends/riscv/defs/vector_perm.tmdl
    backends/riscv/defs/vector_widen.tmdl
    backends/riscv/defs/vector_fixed.tmdl
    backends/riscv/defs/vector_mem.tmdl
    backends/riscv/defs/vector_float.tmdl
    backends/riscv/defs/syntacore_scr1.tmdl
)

mkdir -p "$output"
(
    cd "$tir"
    cargo run -p tmdl --bin tmdlc -- \
        --action=emit-ast-json \
        --output="$temporary/riscv-ast-v1.pretty.json" \
        "${inputs[@]}"
)
jq -c . "$temporary/riscv-ast-v1.pretty.json" > "$output/riscv-ast-v1.json"
git -C "$tir" rev-parse HEAD > "$output/TIR_REVISION"
cp "$tir/LICENSE" "$output/LICENSE"

echo "updated $output from TIR $(<"$output/TIR_REVISION")"
