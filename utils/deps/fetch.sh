#!/usr/bin/env bash
# Downloads one dependency bundle pinned in deps/manifest.toml, verifies its
# checksum and unpacks it. Prints the extracted bundle directory.
#
# usage: fetch.sh <dependency> <platform> <destination-directory>
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

dependency="${1:?usage: fetch.sh <dependency> <platform> <destination>}"
platform="${2:?usage: fetch.sh <dependency> <platform> <destination>}"
destination="${3:?usage: fetch.sh <dependency> <platform> <destination>}"

release="$(deps_manifest_get release)"
if ! file="$(deps_manifest_get "artifacts.${dependency}.${platform}.file" 2>/dev/null)"; then
    printf '%s is not published for %s in %s.\n' "${dependency}" "${platform}" "${release}" >&2
    printf 'Run the Dependencies workflow and merge the repin pull request; ' >&2
    # Backticks are literal here: this is prose telling the user what to run.
    # shellcheck disable=SC2016
    printf '`python3 utils/deps/manifest.py check` lists what is missing.\n' >&2
    exit 1
fi
expected_sha256="$(deps_manifest_get "artifacts.${dependency}.${platform}.sha256")"

cache="${MINIPERF_DEPS_CACHE:-${deps_repository_root}/deps/cache/download}"
mkdir -p "${cache}" "${destination}"
archive="${cache}/${file}"

if [[ ! -f "${archive}" ]] || [[ "$(deps_sha256 "${archive}")" != "${expected_sha256}" ]]; then
    deps_curl "https://github.com/alexbatashev/miniperf/releases/download/${release}/${file}" \
        --output "${archive}.partial"
    actual_sha256="$(deps_sha256 "${archive}.partial")"
    if [[ "${actual_sha256}" != "${expected_sha256}" ]]; then
        printf '%s checksum mismatch: expected %s, found %s\n' \
            "${file}" "${expected_sha256}" "${actual_sha256}" >&2
        rm -f "${archive}.partial"
        exit 1
    fi
    mv "${archive}.partial" "${archive}"
fi

staging="$(mktemp -d "${TMPDIR:-/tmp}/miniperf-fetch.XXXXXX")"
trap 'rm -rf "${staging}"' EXIT
if [[ "${file}" == *.zip ]]; then
    "${deps_python}" -c 'import shutil, sys; shutil.unpack_archive(sys.argv[1], sys.argv[2])' \
        "${archive}" "${staging}"
else
    tar --extract --zstd --file "${archive}" --directory "${staging}"
fi

bundle="$(find "${staging}" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
if [[ -z "${bundle}" ]]; then
    printf '%s contains no bundle directory\n' "${file}" >&2
    exit 1
fi
destination="$(cd "${destination}" && pwd)"
extracted="${destination}/$(basename "${bundle}")"
rm -rf "${extracted}"
mv "${bundle}" "${extracted}"
printf '%s\n' "${extracted}"
