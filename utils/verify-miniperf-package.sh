#!/usr/bin/env bash
# Asserts that a staged miniperf package contains everything the platform
# promises, so a package that would fail at the user's machine fails the build
# instead.
#
# usage: verify-miniperf-package.sh <platform> <package-root>
set -euo pipefail

platform="${1:?usage: verify-miniperf-package.sh <platform> <package-root>}"
package_root="${2:?usage: verify-miniperf-package.sh <platform> <package-root>}"

required_files=(MANIFEST.txt share/doc/miniperf/README.md share/doc/miniperf/LICENSE)

case "${platform}" in
    linux-riscv64)
        required_files+=(
            bin/mperf
            lib/miniperf/libmperf_collector.so
            lib/miniperf/libmperf_libc.so
            lib/miniperf/libmperf_ompt.so
            lib/miniperf/libmperf_itt.so
            lib/miniperf/libmperf_mpi.so
            lib/miniperf/libmperf_cupti.so
            lib/miniperf/libminiperf_qemu_roofline.so
            lib/miniperf/libdr_roofline.so
            lib/miniperf/dynamorio/bin64/drrun
            lib/miniperf/qemu/bin/qemu-riscv64
            include/mperf_trace.h
            share/miniperf/mperf_trace_stub.c
        )
        ;;
    linux-*)
        required_files+=(
            bin/mperf
            bin/mperf-gui
            lib/miniperf/libmperf_collector.so
            lib/miniperf/libmperf_libc.so
            lib/miniperf/libmperf_ompt.so
            lib/miniperf/libmperf_itt.so
            lib/miniperf/libmperf_mpi.so
            lib/miniperf/libmperf_cupti.so
            lib/miniperf/libminiperf_qemu_roofline.so
            lib/miniperf/libdr_roofline.so
            lib/miniperf/dynamorio/bin64/drrun
            lib/miniperf/qemu/bin/qemu-riscv64
            include/mperf_trace.h
            share/miniperf/mperf_trace_stub.c
        )
        ;;
    macos-*)
        # A loose GUI binary on macOS has no menu bar and cannot be quit; the
        # .app layout is the artifact, not a nicety.
        required_files+=(
            bin/mperf
            bin/mperf-gui
            lib/miniperf/libmperf_collector.dylib
            include/mperf_trace.h
            share/miniperf/mperf_trace_stub.c
            mperf-gui.app/Contents/Info.plist
            mperf-gui.app/Contents/MacOS/mperf-gui
            mperf-gui.app/Contents/PkgInfo
        )
        ;;
    windows-*)
        required_files+=(
            bin/mperf.exe
            mperf-gui.exe
            lib/miniperf/mperf_collector.dll
            lib/miniperf/mperf_ompt.dll
            lib/miniperf/mperf_itt.dll
            lib/miniperf/mperf_cupti.dll
            lib/miniperf/dr_roofline.dll
            lib/miniperf/dynamorio/bin64/drrun.exe
            include/mperf_trace.h
            share/miniperf/mperf_trace_stub.c
            share/doc/miniperf/DYNAMORIO_LICENSE.txt
        )
        ;;
    *)
        printf 'unknown platform: %s\n' "${platform}" >&2
        exit 2
        ;;
esac

status=0
for relative in "${required_files[@]}"; do
    if [[ ! -f "${package_root}/${relative}" ]]; then
        printf 'package is missing file %s\n' "${relative}" >&2
        status=1
    fi
done
if [[ "${platform}" == windows-* && -f "${package_root}/bin/mperf.exe" ]]; then
    # Doctor may exit nonzero when this packaging runner has no ETW privilege.
    # Its DynamoRIO row still verifies that the shipped binary finds the
    # bundled launcher and client from the package layout.
    doctor_output="$("${package_root}/bin/mperf.exe" doctor 2>&1)" || true
    if ! grep -q 'drrun.exe and dr_roofline.dll found' <<<"${doctor_output}"; then
        printf 'packaged mperf cannot discover its DynamoRIO launcher and client\n' >&2
        status=1
    fi
fi
if [[ "${platform}" == macos-* ]] && command -v plutil >/dev/null 2>&1; then
    plutil -lint "${package_root}/mperf-gui.app/Contents/Info.plist" >/dev/null || status=1
fi

if [[ "${status}" -ne 0 ]]; then
    printf '\n%s package verification failed\n' "${platform}" >&2
    exit 1
fi
printf '%s package verification passed (%d files)\n' "${platform}" "${#required_files[@]}"
