#!/usr/bin/env bash
# Installs the pinned DuckDB where .cargo/config.toml points DUCKDB_LIB_DIR.
#
# This has to run before cargo, not from a build script: libduckdb-sys declares
# `links = "duckdb"`, so its own build script and compilation both happen before
# any dependent's build script could materialise the library, and rustc bundles
# the static archive into the rlib as it compiles.
#
# usage: setup-duckdb.sh [target-triple]   (defaults to the host target)
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

target="${1:-$(rustc -vV | awk '/^host: /{ print $2 }')}"
case "${target}" in
    x86_64-unknown-linux-gnu) platform=linux-x86_64 ;;
    aarch64-unknown-linux-gnu) platform=linux-aarch64 ;;
    riscv64gc-unknown-linux-gnu) platform=linux-riscv64 ;;
    aarch64-apple-darwin) platform=macos-aarch64 ;;
    x86_64-pc-windows-msvc) platform=windows-x86_64 ;;
    *)
        printf 'miniperf-store does not support %s\n' "${target}" >&2
        exit 1
        ;;
esac

destination="${deps_repository_root}/deps/cache/duckdb"
stamp="${destination}/.stamp"
want="${platform} $(deps_manifest_get "artifacts.duckdb.${platform}.sha256")"

if [[ -f "${stamp}" ]] && [[ "$(cat "${stamp}")" == "${want}" ]]; then
    exit 0
fi

staging="$(mktemp -d "${TMPDIR:-/tmp}/miniperf-duckdb.XXXXXX")"
trap 'rm -rf "${staging}"' EXIT
bundle="$("$(dirname "${BASH_SOURCE[0]}")/fetch.sh" duckdb "${platform}" "${staging}")"

rm -rf "${destination}"
mkdir -p "$(dirname "${destination}")"
mv "${bundle}" "${destination}"
printf '%s' "${want}" >"${stamp}"
