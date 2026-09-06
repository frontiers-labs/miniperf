#!/usr/bin/env bash
# Builds a relocatable DynamoRIO bundle for the miniperf roofline backend:
# DynamoRIO (pinned master commit; releases lack the riscv64 port) plus the
# miniperf dr-roofline client linked against roofline-core.
#
# linux-x86_64 and linux-aarch64 build natively. linux-riscv64 cross-compiles
# against the sysroot that utils/deps/setup-riscv64-sysroot.sh assembles.
#
# usage: build-dynamorio-bundle.sh <platform> [output-directory]
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deps/common.sh"

platform="${1:?usage: build-dynamorio-bundle.sh <platform> [output-directory]}"
output_directory="${2:-dist}"
repository="${DYNAMORIO_REPOSITORY:-$(deps_manifest_get upstream.dynamorio.repository)}"
revision="${DYNAMORIO_REVISION:-$(deps_manifest_get upstream.dynamorio.revision)}"
repository_root="${deps_repository_root}"
patch_directory="${repository_root}/deps/patches/dynamorio"
# The bundle is the pinned revision plus the patches in deps/patches, so its
# name has to change when either does. Without the patch fingerprint an
# artifact built from edited patches would reuse the name of one built from
# the old ones.
if compgen -G "${patch_directory}/*.patch" >/dev/null; then
    patch_id="$(cat "${patch_directory}"/*.patch | sha256sum | cut -c1-8)"
    bundle_name="miniperf-dynamorio-${revision:0:12}-p${patch_id}-${platform}"
    bundle_version="${revision:0:12}-p${patch_id}"
else
    bundle_name="miniperf-dynamorio-${revision:0:12}-${platform}"
    bundle_version="${revision:0:12}"
fi

case "${platform}" in
    linux-x86_64) rust_target=x86_64-unknown-linux-gnu ;;
    linux-aarch64) rust_target=aarch64-unknown-linux-gnu ;;
    linux-riscv64) rust_target=riscv64gc-unknown-linux-gnu ;;
    *)
        printf 'DynamoRIO has no port for %s\n' "${platform}" >&2
        exit 2
        ;;
esac

build_root="$(mktemp -d "${DEPS_BUILD_PARENT:-/tmp}/miniperf-dynamorio.XXXXXX")"
cleanup() {
    local status=$?
    trap - EXIT
    if [[ "${status}" -ne 0 && "${DYNAMORIO_KEEP_FAILED_BUILD:-0}" == 1 ]]; then
        printf 'Preserving failed DynamoRIO build at %s\n' "${build_root}" >&2
    else
        rm -rf "${build_root}"
    fi
    exit "${status}"
}
trap cleanup EXIT

source_directory="${build_root}/source"
build_directory="${build_root}/build"
client_build_directory="${build_root}/client"
bundle_directory="${build_root}/${bundle_name}"

git clone "${repository}" "${source_directory}"
git -C "${source_directory}" checkout --quiet "${revision}"
git -C "${source_directory}" submodule update --init

# Patches carried against the pinned revision. Each one is upstreamable on its
# own; they are applied in filename order and the build fails loudly if one
# stops applying, because a silently skipped patch produces a bundle that looks
# right and behaves like the unpatched build.
if compgen -G "${patch_directory}/*.patch" >/dev/null; then
    for patch in "${patch_directory}"/*.patch; do
        printf 'Applying %s\n' "$(basename "${patch}")"
        git -C "${source_directory}" apply --whitespace=nowarn "${patch}"
    done
fi

cmake="${DYNAMORIO_CMAKE:-cmake}"
# ZLIB_ROOT keeps non-system cmake installations (e.g. nix) finding the
# distro zlib that drsyms requires.
cmake_arguments=(
    -DDISABLE_WARNINGS=ON
    -DBUILD_TESTS=OFF
    -DBUILD_SAMPLES=OFF
    -DBUILD_DOCS=OFF
    -DZLIB_ROOT=/usr
)
cargo_arguments=(--release -p miniperf-roofline-core --target "${rust_target}")

if [[ "${platform}" == linux-riscv64 ]]; then
    cmake_arguments=(
        "-DCMAKE_TOOLCHAIN_FILE=${source_directory}/make/toolchain-riscv64.cmake"
        -DDISABLE_WARNINGS=ON
        -DBUILD_TESTS=OFF
        -DBUILD_SAMPLES=OFF
        -DBUILD_DOCS=OFF
        -DCMAKE_FIND_ROOT_PATH=/usr/riscv64-linux-gnu
        -DZLIB_ROOT=/usr/riscv64-linux-gnu
    )
    export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc
    export CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc
    export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=${RISCV_TARGET_FEATURE:-+v,+zba,+zbb}"
fi

"${cmake}" -S "${source_directory}" -B "${build_directory}" "${cmake_arguments[@]}"
"${cmake}" --build "${build_directory}" -j "$(nproc)"

cargo build --manifest-path "${repository_root}/Cargo.toml" "${cargo_arguments[@]}"
roofline_core_library="${repository_root}/target/${rust_target}/release/libroofline_core.a"

client_cmake_arguments=(
    "-DDynamoRIO_DIR=${build_directory}/cmake"
    "-DROOFLINE_CORE_LIB=${roofline_core_library}"
)
if [[ "${platform}" == linux-riscv64 ]]; then
    client_cmake_arguments+=(
        "-DCMAKE_TOOLCHAIN_FILE=${source_directory}/make/toolchain-riscv64.cmake"
        -DCMAKE_FIND_ROOT_PATH=/usr/riscv64-linux-gnu
    )
fi
"${cmake}" -S "${repository_root}/utils/dr-roofline" -B "${client_build_directory}" \
    "${client_cmake_arguments[@]}"
"${cmake}" --build "${client_build_directory}"

mkdir -p "${bundle_directory}/dynamorio"
for directory in bin64 lib64 ext; do
    cp -a "${build_directory}/${directory}" "${bundle_directory}/dynamorio/"
done
cp "${client_build_directory}/libdr_roofline.so" "${bundle_directory}/"
cp "${source_directory}/License.txt" "${bundle_directory}/DYNAMORIO_LICENSE.txt"
{
    printf 'dynamorio_revision=%s\n' "${revision}"
    printf 'platform=%s\n' "${platform}"
    if compgen -G "${patch_directory}/*.patch" >/dev/null; then
        for patch in "${patch_directory}"/*.patch; do
            printf 'patch=%s\n' "$(basename "${patch}")"
        done
    fi
} >"${bundle_directory}/MANIFEST.txt"

if [[ "${platform}" == linux-riscv64 ]]; then
    # DynamoRIO is itself a binary translator, so there is no way to exercise
    # the cross-built bundle under qemu-user. Check the ELF shape instead.
    machine="$(readelf -h "${bundle_directory}/libdr_roofline.so" | awk -F': +' '/Machine:/ { print $2 }')"
    if [[ "${machine}" != *RISC-V* ]]; then
        printf 'dr_roofline client is not a RISC-V object: %s\n' "${machine}" >&2
        exit 1
    fi
    test -x "${bundle_directory}/dynamorio/bin64/drrun"
else
    smoke_output="${build_root}/smoke.counts"
    "${bundle_directory}/dynamorio/bin64/drrun" -disable_traces -max_bb_instrs 32 \
        -c "${bundle_directory}/libdr_roofline.so" \
        "output=${smoke_output}" memory-profile=off -- /bin/true
    grep -q '^instructions=' "${smoke_output}"
fi

deps_publish dynamorio "${platform}" "${bundle_version}" \
    "${build_root}" "${bundle_name}" "${output_directory}"
