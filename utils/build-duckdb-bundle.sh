#!/usr/bin/env bash
# Builds a prebuilt DuckDB for miniperf-store to link against, replacing the
# duckdb crate's `bundled` feature (which recompiles DuckDB from source on
# every clean build).
#
# The archives CMake produces — the extension loader, core_functions, parquet,
# jemalloc where enabled, and duckdb_static itself — are merged into a single
# libduckdb_static.a so that `DUCKDB_STATIC=1 DUCKDB_LIB_DIR=<bundle>/lib` is
# all libduckdb-sys needs. Link order stops mattering once there is one archive.
#
# usage: build-duckdb-bundle.sh <platform> [output-directory]
set -euo pipefail

# shellcheck source=utils/deps/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deps/common.sh"

platform="${1:?usage: build-duckdb-bundle.sh <platform> [output-directory]}"
output_directory="${2:-dist}"
version="${DUCKDB_VERSION:-$(deps_manifest_get upstream.duckdb.version)}"
repository="${DUCKDB_REPOSITORY:-$(deps_manifest_get upstream.duckdb.repository)}"
bundle_name="miniperf-duckdb-${version}-${platform}"

# CMake and MSVC are native binaries: handed an MSYS path like /tmp/... they
# report the source directory as missing. Stage under the Windows temp
# directory in mixed form (C:/...), which both bash and they understand.
if [[ "${platform}" == windows-* && -z "${DEPS_BUILD_PARENT:-}" ]]; then
    DEPS_BUILD_PARENT="$(deps_native_path "$(cd "${TEMP:-${TMP:-/tmp}}" && pwd)")"
fi
build_root="$(mktemp -d "${DEPS_BUILD_PARENT:-/tmp}/miniperf-duckdb.XXXXXX")"
trap 'rm -rf "${build_root}"' EXIT
source_directory="${build_root}/source"
build_directory="${build_root}/build"
install_directory="${build_root}/install"
bundle_directory="${build_root}/${bundle_name}"

git clone --depth 1 --branch "v${version}" "${repository}" "${source_directory}"

cmake_arguments=(
    -S "${source_directory}"
    -B "${build_directory}"
    -G Ninja
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_LIBDIR=lib
    -DBUILD_UNITTESTS=0
    -DBUILD_SHELL=0
    -DBUILD_SHARED_LIBS=0
    -DDISABLE_UNITY=0
    -DDISABLE_EXTENSION_LOAD=0
    -DENABLE_EXTENSION_AUTOLOADING=1
    -DENABLE_EXTENSION_AUTOINSTALL=1
    -DBUILD_EXTENSIONS=parquet
    -DSKIP_EXTENSIONS=
)

# Keep MSYS from rewriting MSVC's '-FOO:bar' arguments as Windows paths.
export MSYS2_ARG_CONV_EXCL='*'
archiver="ar"
warning_flag=-w
case "${platform}" in
    linux-x86_64 | linux-aarch64)
        # Matches what the duckdb crate's bundled build enables.
        cmake_arguments+=(-DENABLE_JEMALLOC=ON)
        ;;
    linux-riscv64)
        toolchain="${build_root}/riscv64.cmake"
        cat >"${toolchain}" <<'EOF'
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR riscv64)
set(CMAKE_C_COMPILER riscv64-linux-gnu-gcc)
set(CMAKE_CXX_COMPILER riscv64-linux-gnu-g++)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
EOF
        cmake_arguments+=(
            # DuckDB otherwise builds duckdb_platform_binary for the target and
            # executes it to name the platform, which binfmt cannot run without
            # a riscv64 loader on the host. The build's own message suggests
            # DUCKDB_PLATFORM, but CMakeLists.txt gates on this one.
            -DDUCKDB_EXPLICIT_PLATFORM=linux_riscv64
            "-DCMAKE_TOOLCHAIN_FILE=${toolchain}"
            "-DCMAKE_C_FLAGS=-march=${RISCV_MARCH:-rv64gcv_zba_zbb} -mabi=lp64d"
            "-DCMAKE_CXX_FLAGS=-march=${RISCV_MARCH:-rv64gcv_zba_zbb} -mabi=lp64d"
            -DENABLE_JEMALLOC=OFF
        )
        archiver=riscv64-linux-gnu-ar
        ;;
    macos-aarch64)
        cmake_arguments+=(
            -DCMAKE_OSX_ARCHITECTURES=arm64
            "-DCMAKE_OSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}"
        )
        ;;
    windows-x86_64)
        warning_flag=-w
        ;;
    *)
        printf 'unsupported platform for DuckDB: %s\n' "${platform}" >&2
        exit 2
        ;;
esac

cmake_arguments+=(
    "-DCMAKE_C_FLAGS_INIT=${warning_flag}"
    "-DCMAKE_CXX_FLAGS_INIT=${warning_flag}"
)

cmake "${cmake_arguments[@]}"
cmake --build "${build_directory}"
cmake --install "${build_directory}" --prefix "${install_directory}"

mkdir -p "${bundle_directory}/lib" "${bundle_directory}/include"
cp "${install_directory}/include/duckdb.h" "${bundle_directory}/include/duckdb.h"
cp "${install_directory}/include/duckdb.hpp" "${bundle_directory}/include/duckdb.hpp"
cp "${source_directory}/LICENSE" "${bundle_directory}/DUCKDB_LICENSE.txt"

# DuckDB installs both the generated extension loader and a do-nothing
# `dummy_static_extension_loader` for builds without static extensions. Merging
# both leaves it to the linker which one it pulls, and picking the dummy
# silently unregisters parquet, so keep only the generated one.
# macOS ships bash 3.2, which has no `mapfile`.
component_libraries=()
if [[ "${platform}" == windows-* ]]; then
    library_glob='*.lib'
    dummy_loader='dummy_static_extension_loader.lib'
else
    library_glob='lib*.a'
    dummy_loader='libdummy_static_extension_loader.a'
fi
excluded_libraries=("${dummy_loader}")
if [[ "${platform}" == windows-* ]]; then
    # DuckDB builds the shared library too, and duckdb.lib is its import
    # library: stubs that define the entire C API. lib.exe keeps the first
    # definition it sees, so leaving it in the merge replaces the static
    # implementation with thunks into duckdb.dll. The link still succeeds and
    # the result is a few hundred KB that fails at runtime. The Unix glob never
    # matched the equivalent libduckdb.so, which is why only Windows broke.
    excluded_libraries+=(duckdb.lib)
fi
find_arguments=("${install_directory}/lib" -name "${library_glob}")
for excluded_library in "${excluded_libraries[@]}"; do
    find_arguments+=(-not -name "${excluded_library}")
done
while IFS= read -r library; do
    component_libraries+=("${library}")
done < <(find "${find_arguments[@]}" | sort)
if [[ "${#component_libraries[@]}" -eq 0 ]]; then
    printf 'DuckDB install produced no static libraries in %s\n' "${install_directory}/lib" >&2
    exit 1
fi
printf 'Merging %d DuckDB archives\n' "${#component_libraries[@]}"
for component_library in "${component_libraries[@]}"; do
    printf '  %8s  %s\n' \
        "$(du -k "${component_library}" | cut -f1)K" "$(basename "${component_library}")"
done

case "${platform}" in
    windows-*)
        merged_library="${bundle_directory}/lib/duckdb_static.lib"
        lib.exe "-OUT:${merged_library}" "${component_libraries[@]}"
        ;;
    macos-*)
        merged_library="${bundle_directory}/lib/libduckdb_static.a"
        libtool -static -o "${merged_library}" "${component_libraries[@]}"
        ;;
    *)
        merged_library="${bundle_directory}/lib/libduckdb_static.a"
        {
            printf 'create %s\n' "${merged_library}"
            printf 'addlib %s\n' "${component_libraries[@]}"
            printf 'save\nend\n'
        } | "${archiver}" -M
        ;;
esac

{
    printf 'duckdb_version=%s\n' "${version}"
    if [[ "${platform}" == windows-* ]]; then
        printf 'required_define=DUCKDB_STATIC_BUILD\n'
    fi
    printf 'platform=%s\n' "${platform}"
    printf 'extensions=parquet,core_functions\n'
    printf 'merged_archives=%s\n' "$(basename -a "${component_libraries[@]}" | paste -sd, -)"
} >"${bundle_directory}/MANIFEST.txt"

# Prove the merged archive is self-contained and that parquet is compiled in
# rather than autoloaded, which is the whole reason miniperf-store links it.
merged_kilobytes="$(du -k "${merged_library}" | cut -f1)"
printf 'Merged library: %s (%sK)\n' "${merged_library}" "${merged_kilobytes}"
if [[ "${merged_kilobytes}" -lt 20480 ]]; then
    printf 'merged DuckDB library is only %sK; it holds import thunks rather than\n' \
        "${merged_kilobytes}" >&2
    printf 'the static implementation, which links cleanly and fails at runtime\n' >&2
    exit 1
fi

smoke_source="${build_root}/duckdb-smoke.c"
cat >"${smoke_source}" <<'EOF'
#include <duckdb.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    duckdb_config config;
    duckdb_database database;
    duckdb_connection connection;
    duckdb_result result;

    // Without these, a parquet query could be satisfied by downloading the
    // extension at runtime, which would make this test pass on a build where
    // parquet was never linked in.
    char *open_error = NULL;
    if (duckdb_create_config(&config) != DuckDBSuccess) {
        fprintf(stderr, "duckdb_create_config failed\n");
        return 1;
    }
    if (duckdb_set_config(config, "autoinstall_known_extensions", "false") != DuckDBSuccess) {
        fprintf(stderr, "duckdb_set_config(autoinstall_known_extensions) failed\n");
        return 7;
    }
    if (duckdb_set_config(config, "autoload_known_extensions", "false") != DuckDBSuccess) {
        fprintf(stderr, "duckdb_set_config(autoload_known_extensions) failed\n");
        return 8;
    }
    if (duckdb_open_ext(NULL, &database, config, &open_error) != DuckDBSuccess) {
        fprintf(stderr, "duckdb_open_ext failed: %s\n",
                open_error ? open_error : "(no error reported)");
        return 9;
    }
    duckdb_destroy_config(&config);
    if (duckdb_connect(database, &connection) != DuckDBSuccess) {
        fprintf(stderr, "duckdb_connect failed\n");
        return 2;
    }
    duckdb_result write_result;
    if (duckdb_query(connection,
                     "COPY (SELECT 42::BIGINT AS answer) TO 'smoke.parquet' (FORMAT PARQUET)",
                     &write_result) != DuckDBSuccess) {
        fprintf(stderr, "parquet write failed: %s\n", duckdb_result_error(&write_result));
        return 3;
    }
    if (duckdb_query(connection, "SELECT answer FROM read_parquet('smoke.parquet')", &result) !=
        DuckDBSuccess) {
        fprintf(stderr, "parquet read failed: %s\n", duckdb_result_error(&result));
        return 4;
    }

    duckdb_data_chunk chunk = duckdb_fetch_chunk(result);
    if (chunk == NULL) return 5;
    int64_t *values = (int64_t *)duckdb_vector_get_data(duckdb_data_chunk_get_vector(chunk, 0));
    if (values[0] != 42) return 6;

    printf("duckdb %s parquet round-trip ok\n", duckdb_library_version());
    return 0;
}
EOF

smoke_binary="${build_root}/duckdb-smoke"
smoke_name=duckdb-smoke
case "${platform}" in
    windows-*)
        smoke_name=duckdb-smoke.exe
        smoke_binary="${build_root}/${smoke_name}"
        compile_status=0
        (
            cd "${build_root}"
            # Without DUCKDB_STATIC_BUILD, duckdb.h declares the C API
            # __declspec(dllimport) on Windows and the link asks for __imp_*
            # symbols that a static library does not carry.
            cl.exe -nologo -DDUCKDB_STATIC_BUILD "-I${bundle_directory}/include" \
                "${smoke_source}" "-Fe${smoke_name}" "${merged_library}" \
                ws2_32.lib rstrtmgr.lib bcrypt.lib
        ) || compile_status=$?
        if [[ "${compile_status}" -ne 0 ]]; then
            printf 'cl.exe exited %d building the DuckDB smoke test\n' "${compile_status}" >&2
            exit 1
        fi
        if [[ ! -f "${smoke_binary}" ]]; then
            printf 'cl.exe reported success but %s is absent; build root holds:\n' \
                "${smoke_binary}" >&2
            ls -la "${build_root}" >&2
            exit 1
        fi
        ;;
    macos-*)
        cc -I"${bundle_directory}/include" "${smoke_source}" -o "${smoke_binary}" \
            "${merged_library}" -lc++
        ;;
    linux-riscv64)
        riscv64-linux-gnu-gcc -static -I"${bundle_directory}/include" "${smoke_source}" \
            -o "${smoke_binary}" "${merged_library}" -lstdc++ -lpthread -ldl -lm
        ;;
    *)
        cc -I"${bundle_directory}/include" "${smoke_source}" -o "${smoke_binary}" \
            "${merged_library}" -lstdc++ -lpthread -ldl -lm
        ;;
esac

# macOS ships bash 3.2, which treats "${empty[@]}" as an unbound variable under
# `set -u`; keep the binary itself in the array so it is never empty.
smoke_command=("./${smoke_name}")
run_smoke=1
if [[ "${platform}" == linux-riscv64 ]]; then
    # Cross-built on x86; run the check under qemu-user when the host has it.
    if command -v qemu-riscv64 >/dev/null 2>&1; then
        # The bundle is compiled for rv64gcv_zba_zbb and GCC autovectorizes at
        # -O3, so the default qemu CPU (no V) would SIGILL.
        smoke_command=(
            qemu-riscv64 -cpu "rv64,v=true,vlen=256,zba=true,zbb=true" "./${smoke_name}"
        )
    else
        printf 'qemu-riscv64 is unavailable; DuckDB bundle verified by linking only\n' >&2
        run_smoke=0
    fi
fi
if [[ "${run_smoke}" -eq 1 ]]; then
    smoke_status=0
    if [[ "${platform}" == windows-* ]]; then
        # Git Bash reports any failed image load as a bare 127 with no further
        # detail. Hand the binary to the Windows loader instead, inheriting the
        # working directory from bash rather than quoting a path through
        # cmd.exe, which is fussy enough to fail on its own.
        (cd "${build_root}" && cmd.exe /c "${smoke_name}") || smoke_status=$?
    else
        (cd "${build_root}" && "${smoke_command[@]}") || smoke_status=$?
    fi
    if [[ "${smoke_status}" -ne 0 ]]; then
        printf 'DuckDB smoke test exited %d\n' "${smoke_status}" >&2
        ls -la "${build_root}" >&2
        exit 1
    fi
fi

deps_publish duckdb "${platform}" "${version}" "${build_root}" "${bundle_name}" "${output_directory}"
