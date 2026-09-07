#!/usr/bin/env bash
# Verifies and unpacks a miniperf package, printing the unpacked root.
#
# usage: unpack-package.sh <directory-holding-the-tarball> <destination>
#
# The checksum sidecar is checked rather than merely shipped: a truncated
# artifact download would otherwise surface as a confusing failure inside a
# check rather than here.
set -euo pipefail

source_directory="${1:?usage: unpack-package.sh <dist-dir> <dest-dir>}"
destination="${2:?usage: unpack-package.sh <dist-dir> <dest-dir>}"

tarball="$(find "${source_directory}" -name '*.tar.*' ! -name '*.sha256' | head -1)"
[[ -n "${tarball}" ]] || { echo "no package archive in ${source_directory}" >&2; exit 1; }

if [[ -f "${tarball}.sha256" ]]; then
    if command -v sha256sum >/dev/null; then
        (cd "$(dirname "${tarball}")" && sha256sum -c "$(basename "${tarball}").sha256" >/dev/null)
    else
        # macOS ships shasum rather than sha256sum.
        (cd "$(dirname "${tarball}")" && shasum -a 256 -c "$(basename "${tarball}").sha256" >/dev/null)
    fi
fi

mkdir -p "${destination}"
tar -C "${destination}" -xf "${tarball}"
root="$(find "${destination}" -maxdepth 1 -mindepth 1 -type d | head -1)"
[[ -n "${root}" ]] || { echo "the archive contained no package directory" >&2; exit 1; }
(cd "${root}" && pwd)
