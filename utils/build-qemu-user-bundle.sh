#!/usr/bin/env bash
# Builds a plugin-enabled qemu-user bundle for the miniperf roofline backend.
#
# linux-x86_64 and linux-aarch64 build natively. linux-riscv64 cross-compiles
# against the sysroot that utils/deps/setup-riscv64-sysroot.sh assembles.
#
# usage: build-qemu-user-bundle.sh <platform> [output-directory]
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deps/common.sh"

# qemu-user reads its own options from QEMU_* environment variables, so a
# stray QEMU_VERSION or QEMU_PLUGIN in the environment silently changes what
# every qemu invocation below does. Our knobs are MINIPERF_QEMU_* for that
# reason; clear QEMU's before running any of the verification binaries.
unset QEMU_VERSION QEMU_PLUGIN QEMU_CPU QEMU_LD_PREFIX QEMU_STRACE QEMU_LOG

platform="${1:?usage: build-qemu-user-bundle.sh <platform> [output-directory]}"
output_directory="${2:-dist}"
qemu_version="${MINIPERF_QEMU_VERSION:-$(deps_manifest_get upstream.qemu.version)}"
qemu_sha256="${MINIPERF_QEMU_SHA256:-$(deps_manifest_get upstream.qemu.source_sha256)}"
repository_root="${deps_repository_root}"
bundle_name="miniperf-qemu-user-${qemu_version}-${platform}"
targets=x86_64-linux-user,riscv32-linux-user,riscv64-linux-user

case "${platform}" in
    linux-x86_64 | linux-aarch64 | linux-riscv64) ;;
    *)
        printf 'qemu-user targets Linux hosts only; %s is unsupported\n' "${platform}" >&2
        exit 2
        ;;
esac

build_root="$(mktemp -d "${DEPS_BUILD_PARENT:-/tmp}/miniperf-qemu.XXXXXX")"
cleanup() {
    local status=$?
    trap - EXIT
    if [[ "${status}" -ne 0 && "${MINIPERF_QEMU_KEEP_FAILED_BUILD:-0}" == 1 ]]; then
        printf 'Preserving failed QEMU build at %s\n' "${build_root}" >&2
    else
        rm -rf "${build_root}"
    fi
    exit "${status}"
}
trap cleanup EXIT

archive="${build_root}/qemu-${qemu_version}.tar.xz"
source_directory="${build_root}/source"
build_directory="${build_root}/build"
install_directory="${build_root}/install"
bundle_directory="${build_root}/${bundle_name}"

if [[ -n "${MINIPERF_QEMU_SOURCE_ARCHIVE:-}" ]]; then
    cp "${MINIPERF_QEMU_SOURCE_ARCHIVE}" "${archive}"
else
    deps_curl "https://download.qemu.org/qemu-${qemu_version}.tar.xz" \
        --output "${archive}"
fi
actual_sha256="$(deps_sha256 "${archive}")"
if [[ "${actual_sha256}" != "${qemu_sha256}" ]]; then
    printf 'QEMU source checksum mismatch: expected %s, found %s\n' \
        "${qemu_sha256}" "${actual_sha256}" >&2
    exit 1
fi

mkdir -p "${source_directory}" "${build_directory}" "${install_directory}"
tar --extract --file "${archive}" --directory "${source_directory}" --strip-components=1

configure_arguments=(
    --python="${MINIPERF_QEMU_PYTHON:-/usr/bin/python3}"
    # \$ORIGIN is resolved by the dynamic loader, so it must reach the linker
    # unexpanded.
    "--extra-ldflags=-Wl,-rpath,\$ORIGIN/../lib"
    --disable-download
    --prefix=/usr
    "--target-list=${targets}"
    --enable-plugins
    --enable-capstone
    --disable-system
    --disable-bsd-user
    --disable-docs
    --disable-tools
    --disable-guest-agent
    --without-default-features
)
strip_tool="strip"
readelf_tool=readelf
library_directory=/usr/lib

if [[ "${platform}" == linux-riscv64 ]]; then
    sysroot=/usr/riscv64-linux-gnu
    configure_arguments+=(--cross-prefix=riscv64-linux-gnu-)
    export PKG_CONFIG_LIBDIR="${sysroot}/lib/pkgconfig"
    export PKG_CONFIG_SYSROOT_DIR="${sysroot}"
    unset PKG_CONFIG_PATH
    strip_tool=riscv64-linux-gnu-strip
    library_directory="${sysroot}/lib"
else
    configure_arguments+=(--cc="${MINIPERF_QEMU_CC:-cc}" --cxx="${MINIPERF_QEMU_CXX:-c++}")
fi

(
    cd "${build_directory}"
    "${source_directory}/configure" "${configure_arguments[@]}"
    ninja -j "${MINIPERF_QEMU_JOBS:-$(nproc)}"
    DESTDIR="${install_directory}" ninja install
)

mkdir -p \
    "${bundle_directory}/bin" \
    "${bundle_directory}/include" \
    "${bundle_directory}/lib" \
    "${bundle_directory}/share/licenses/qemu"
for binary in qemu-x86_64 qemu-riscv32 qemu-riscv64; do
    install -m 0755 "${install_directory}/usr/bin/${binary}" "${bundle_directory}/bin/${binary}"
    "${strip_tool}" "${bundle_directory}/bin/${binary}"
done
capstone_soname="$(
    "${readelf_tool}" -d "${bundle_directory}/bin/qemu-riscv64" |
        awk '$2 == "(NEEDED)" && $5 ~ /\[libcapstone\.so\./ {
            gsub(/\[|\]/, "", $5)
            print $5
            exit
        }'
)"
if [[ -n "${capstone_soname}" ]]; then
    capstone_library="${library_directory}/${capstone_soname}"
    if [[ ! -e "${capstone_library}" ]]; then
        capstone_library="$(pkg-config --variable=libdir capstone)/${capstone_soname}"
    fi
    if [[ ! -e "${capstone_library}" ]]; then
        printf 'QEMU requires %s, but no such Capstone library was found for %s\n' \
            "${capstone_soname}" "${platform}" >&2
        exit 1
    fi
    install -m 0755 \
        "$(realpath "${capstone_library}")" \
        "${bundle_directory}/lib/${capstone_soname}"
fi
install -m 0644 \
    "${install_directory}/usr/include/qemu-plugin.h" \
    "${bundle_directory}/include/qemu-plugin.h"
for license in COPYING COPYING.LIB; do
    install -m 0644 "${source_directory}/${license}" \
        "${bundle_directory}/share/licenses/qemu/${license}"
done

verify_arguments=()
if [[ "${platform}" == linux-riscv64 ]]; then
    verify_arguments+=(--symbols-only)
elif [[ "${platform}" == linux-aarch64 ]]; then
    # The x86 fixture needs an x86 assembler even though the host is arm64.
    export X86_CC="${X86_CC:-x86_64-linux-gnu-gcc}"
fi
"${repository_root}/utils/verify-qemu-roofline-bundle.sh" \
    "${verify_arguments[@]}" "${bundle_directory}"

{
    printf 'QEMU_VERSION=%s\n' "${qemu_version}"
    printf 'QEMU_SOURCE_SHA256=%s\n' "${qemu_sha256}"
    printf 'PLATFORM=%s\n' "${platform}"
    printf 'TARGETS=%s\n' "${targets}"
    printf 'PLUGIN_API=6\n'
    printf '\nDynamic dependencies (ELF NEEDED):\n'
    "${readelf_tool}" -d "${bundle_directory}/bin/qemu-riscv64" |
        awk '$2 == "(NEEDED)" { gsub(/\[|\]/, "", $5); print $5 }'
} >"${bundle_directory}/MANIFEST.txt"

deps_publish qemu "${platform}" "${qemu_version}" \
    "${build_root}" "${bundle_name}" "${output_directory}"
