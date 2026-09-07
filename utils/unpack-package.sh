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

# Windows packages are a .zip; every other platform is a .tar.gz.
archive="$(find "${source_directory}" \( -name '*.tar.*' -o -name '*.zip' \) ! -name '*.sha256' | head -1)"
[[ -n "${archive}" ]] || {
    echo "no package archive in ${source_directory}; it holds:" >&2
    ls -la "${source_directory}" >&2
    exit 1
}

if [[ -f "${archive}.sha256" ]]; then
    if command -v sha256sum >/dev/null; then
        (cd "$(dirname "${archive}")" && sha256sum -c "$(basename "${archive}").sha256" >/dev/null)
    else
        # macOS ships shasum rather than sha256sum.
        (cd "$(dirname "${archive}")" && shasum -a 256 -c "$(basename "${archive}").sha256" >/dev/null)
    fi
fi

mkdir -p "${destination}"
case "${archive}" in
    *.zip) unzip -q -o "${archive}" -d "${destination}" ;;
    *)     tar -C "${destination}" -xf "${archive}" ;;
esac

root="$(find "${destination}" -maxdepth 1 -mindepth 1 -type d | head -1)"
if [[ -z "${root}" ]]; then
    # shutil.make_archive writes the Windows package's contents at the archive
    # root rather than under a directory.
    root="${destination}"
fi
(cd "${root}" && pwd)
