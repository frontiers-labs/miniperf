# Shared helpers for the dependency build scripts. Source, do not execute.
# shellcheck shell=bash

deps_repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Windows runners ship `python`, not `python3`.
if command -v python3 >/dev/null 2>&1; then
    deps_python=python3
else
    deps_python=python
fi

# Downloads a pinned artifact. Every download in this repository goes through
# here, because the obvious spelling is wrong in a way that only shows up as a
# red build weeks later: `--retry N` covers transient HTTP responses and
# timeouts, and nothing else. A TLS handshake that dies mid-connection exits 35
# and is not retried at all, so one hiccup against a release CDN fails a
# packaging job outright. `--retry-all-errors` is the flag that covers it.
deps_curl() {
    curl --fail --location --silent --show-error \
        --connect-timeout 30 --retry 5 --retry-delay 2 --retry-all-errors "$@"
}

deps_manifest_get() {
    "${deps_python}" "${deps_repository_root}/utils/deps/manifest.py" get "$1"
}

# Prints a path that native Windows tools (CMake, MSVC, Python) can open. Under
# Git Bash the POSIX form is a fiction those binaries cannot resolve.
deps_native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1"
    else
        printf '%s\n' "$1"
    fi
}

deps_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

# deps_publish <dependency> <platform> <version> <staging-parent> <bundle-name> <output-directory>
#
# Archives <staging-parent>/<bundle-name> and writes the archive, its .sha256,
# and the .meta sidecar that utils/deps/manifest.py reads back.
deps_publish() {
    local dependency="$1" platform="$2" version="$3"
    local staging_parent="$4" bundle_name="$5" output_directory="$6"

    mkdir -p "${output_directory}"
    output_directory="$(cd "${output_directory}" && pwd)"

    local archive_name
    if [[ "${platform}" == windows-* ]]; then
        archive_name="${bundle_name}.zip"
        "${deps_python}" -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2], sys.argv[3])' \
            "$(deps_native_path "${output_directory}/${bundle_name}")" \
            "$(deps_native_path "${staging_parent}")" \
            "${bundle_name}"
    else
        archive_name="${bundle_name}.tar.zst"
        tar --create --zstd --file "${output_directory}/${archive_name}" \
            --directory "${staging_parent}" "${bundle_name}"
    fi

    local checksum
    checksum="$(deps_sha256 "${output_directory}/${archive_name}")"
    printf '%s  %s\n' "${checksum}" "${archive_name}" \
        >"${output_directory}/${archive_name}.sha256"
    {
        printf 'dependency=%s\n' "${dependency}"
        printf 'platform=%s\n' "${platform}"
        printf 'version=%s\n' "${version}"
        printf 'file=%s\n' "${archive_name}"
    } >"${output_directory}/${archive_name}.meta"

    printf '%s\n' "${output_directory}/${archive_name}"
}
