#!/usr/bin/env bash
# Build a relocatable miniperf package for one Rust target.
#
# The package embeds the external dependencies pinned in deps/manifest.toml —
# DynamoRIO and qemu-user — under lib/miniperf, where mperf discovers them
# relative to its own executable. Windows ships the GUI only.
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deps/common.sh"

target="${1:?usage: package-miniperf.sh <rust-target> [output-directory]}"
output_directory="${2:-dist}"
repository_root="${deps_repository_root}"
version="$(awk -F '"' '/^version = / { print $2; exit }' "${repository_root}/mperf/Cargo.toml")"
package_name="miniperf-${version}-${target}"

case "${target}" in
    x86_64-unknown-linux-gnu) platform=linux-x86_64 ;;
    aarch64-unknown-linux-gnu) platform=linux-aarch64 ;;
    riscv64gc-unknown-linux-gnu) platform=linux-riscv64 ;;
    aarch64-apple-darwin) platform=macos-aarch64 ;;
    x86_64-pc-windows-msvc) platform=windows-x86_64 ;;
    *)
        printf 'unsupported target: %s\n' "${target}" >&2
        exit 2
        ;;
esac

mkdir -p "${output_directory}"
output_directory="$(cd "${output_directory}" && pwd)"
target_directory="${CARGO_TARGET_DIR:-${repository_root}/target}"
mkdir -p "${target_directory}"
target_directory="$(cd "${target_directory}" && pwd)"
staging_root="$(mktemp -d "${TMPDIR:-/tmp}/miniperf-package.XXXXXX")"
trap 'rm -rf "${staging_root}"' EXIT
package_root="${staging_root}/${package_name}"
release_directory="${target_directory}/${target}/release"

# The riscv64 packages target rv64gcv_zba_zbb, matching the pinned
# dependencies; plain riscv64gc would leave the vector paths unbuilt.
if [[ "${platform}" == linux-riscv64 ]]; then
    export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+v,+zba,+zbb"
fi

if [[ "${platform}" == windows-* ]]; then
    package_crates=(-p mperf-gui)
else
    package_crates=(-p mperf -p miniperf-collector-core)
    if [[ "${platform}" != linux-riscv64 ]]; then
        package_crates+=(-p mperf-gui)
    fi
    if [[ "${platform}" == linux-* ]]; then
        # Every shim ships. They are pure-Rust cdylibs with one libc
        # dependency, and a shim the user cannot find is a feature they cannot
        # use: the tracing and roofline guides both hand out LD_PRELOAD and
        # OMP_TOOL_LIBRARIES paths that only resolve inside the package.
        package_crates+=(
            -p miniperf-shim-libc
            -p miniperf-shim-ompt
            -p miniperf-shim-itt
            -p miniperf-shim-mpi
            -p miniperf-shim-cupti
            -p miniperf-qemu-roofline
        )
    fi
fi

cargo build \
    --locked \
    --release \
    --target "${target}" \
    --manifest-path "${repository_root}/Cargo.toml" \
    "${package_crates[@]}"

mkdir -p "${package_root}/share/doc/miniperf"
install -m 0644 "${repository_root}/README.md" "${package_root}/share/doc/miniperf/README.md"
install -m 0644 "${repository_root}/LICENSE" "${package_root}/share/doc/miniperf/LICENSE"

# The manual tracing API. docs/src/guide/tracing.md tells users to compile
# against these, and until now they existed only in a source checkout.
if [[ "${platform}" != windows-* ]]; then
    mkdir -p "${package_root}/include" "${package_root}/share/miniperf"
    install -m 0644 \
        "${repository_root}/collector-core/include/mperf_trace.h" \
        "${package_root}/include/mperf_trace.h"
    install -m 0644 \
        "${repository_root}/collector-core/stub/mperf_trace_stub.c" \
        "${package_root}/share/miniperf/mperf_trace_stub.c"
fi

if [[ "${platform}" == windows-* ]]; then
    install -m 0755 "${release_directory}/mperf-gui.exe" "${package_root}/mperf-gui.exe"
else
    mkdir -p "${package_root}/bin" "${package_root}/lib/miniperf"
    install -m 0755 "${release_directory}/mperf" "${package_root}/bin/mperf"
fi

if [[ "${platform}" == linux-* ]]; then
    for library in \
        libmperf_collector.so \
        libmperf_libc.so \
        libmperf_ompt.so \
        libmperf_itt.so \
        libmperf_mpi.so \
        libmperf_cupti.so \
        libminiperf_qemu_roofline.so; do
        install -m 0755 "${release_directory}/${library}" "${package_root}/lib/miniperf/${library}"
    done
elif [[ "${platform}" == macos-* ]]; then
    install -m 0755 \
        "${release_directory}/libmperf_collector.dylib" \
        "${package_root}/lib/miniperf/libmperf_collector.dylib"
fi

# Embed the pinned instrumentation backends. mperf resolves drrun at
# lib/miniperf/dynamorio/bin64/drrun and qemu-user at lib/miniperf/qemu/bin,
# both relative to its own executable.
embedded_dependencies=()
if [[ "${platform}" == linux-* ]]; then
    dynamorio_bundle="$("${repository_root}/utils/deps/fetch.sh" dynamorio "${platform}" "${staging_root}")"
    cp -a "${dynamorio_bundle}/dynamorio" "${package_root}/lib/miniperf/dynamorio"
    install -m 0755 \
        "${dynamorio_bundle}/libdr_roofline.so" \
        "${package_root}/lib/miniperf/libdr_roofline.so"
    install -m 0644 \
        "${dynamorio_bundle}/DYNAMORIO_LICENSE.txt" \
        "${package_root}/share/doc/miniperf/DYNAMORIO_LICENSE.txt"
    embedded_dependencies+=(dynamorio)

    qemu_bundle="$("${repository_root}/utils/deps/fetch.sh" qemu "${platform}" "${staging_root}")"
    mkdir -p "${package_root}/lib/miniperf/qemu"
    cp -a "${qemu_bundle}/bin" "${qemu_bundle}/lib" "${package_root}/lib/miniperf/qemu/"
    chmod -R a+rX "${package_root}/lib/miniperf/qemu" "${package_root}/lib/miniperf/dynamorio"
    cp -a "${qemu_bundle}/share/licenses/qemu" "${package_root}/share/doc/miniperf/qemu-licenses"
    embedded_dependencies+=(qemu)
fi

if [[ "${platform}" == macos-* ]]; then
    # macOS only treats an executable as a GUI application when it lives inside
    # a bundle: a loose binary gets no dock icon, no menu bar and therefore no
    # way to quit it. Ship the .app as the real artifact and make bin/mperf-gui
    # a wrapper that execs into it, so command-line use keeps full app
    # behaviour.
    bundle_root="${package_root}/mperf-gui.app"
    mkdir -p "${bundle_root}/Contents/MacOS" "${bundle_root}/Contents/Resources"
    install -m 0755 "${release_directory}/mperf-gui" "${bundle_root}/Contents/MacOS/mperf-gui"
    printf 'APPL????' >"${bundle_root}/Contents/PkgInfo"
    cat >"${bundle_root}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>miniperf</string>
    <key>CFBundleExecutable</key>
    <string>mperf-gui</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.alexbatashev.miniperf</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>miniperf</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${version}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST
    cat >"${package_root}/bin/mperf-gui" <<'WRAPPER'
#!/bin/sh
exec "$(cd "$(dirname "$0")/.." && pwd)/mperf-gui.app/Contents/MacOS/mperf-gui" "$@"
WRAPPER
    chmod 0755 "${package_root}/bin/mperf-gui"
elif [[ "${platform}" == linux-* && "${platform}" != linux-riscv64 ]]; then
    install -m 0755 "${release_directory}/mperf-gui" "${package_root}/bin/mperf-gui"
fi

{
    printf 'name=%s\n' "${package_name}"
    printf 'version=%s\n' "${version}"
    printf 'target=%s\n' "${target}"
    printf 'platform=%s\n' "${platform}"
    printf 'commit=%s\n' "$(git -C "${repository_root}" rev-parse HEAD)"
    printf 'deps_release=%s\n' "$(deps_manifest_get release)"
    printf 'embedded=%s\n' "${embedded_dependencies[*]-}"
} >"${package_root}/MANIFEST.txt"

"${repository_root}/utils/verify-miniperf-package.sh" "${platform}" "${package_root}"

if [[ "${platform}" == windows-* ]]; then
    archive="${output_directory}/${package_name}.zip"
    "${deps_python}" -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2], sys.argv[3])' \
        "${output_directory}/${package_name}" "${staging_root}" "${package_name}"
else
    archive="${output_directory}/${package_name}.tar.gz"
    COPYFILE_DISABLE=1 tar --create --gzip --file "${archive}" \
        --directory "${staging_root}" "${package_name}"
fi
printf '%s  %s\n' "$(deps_sha256 "${archive}")" "$(basename "${archive}")" >"${archive}.sha256"
printf '%s\n' "${archive}"
