cmake_minimum_required(VERSION 3.14)
project(miniperf-dr-roofline C)
if(WIN32)
    # Windows DynamoRIO bundles may include DrMemoryFrameworkConfig.cmake,
    # which reads CMAKE_CXX_SIZEOF_DATA_PTR even for a C-only client.
    enable_language(CXX)
endif()

# Path to the roofline-core Rust staticlib, e.g.
# <repo>/target/release/libroofline_core.a on Unix or
# <repo>/target/release/roofline_core.lib on Windows (build with
# `cargo build --release -p miniperf-roofline-core`).
set(ROOFLINE_CORE_LIB "" CACHE FILEPATH "Path to libroofline_core.a")
if(ROOFLINE_CORE_LIB STREQUAL "")
    get_filename_component(REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." ABSOLUTE)
    if(WIN32)
        set(ROOFLINE_CORE_LIB "${REPO_ROOT}/target/release/roofline_core.lib")
    else()
        set(ROOFLINE_CORE_LIB "${REPO_ROOT}/target/release/libroofline_core.a")
    endif()
endif()
if(NOT EXISTS "${ROOFLINE_CORE_LIB}")
    if(WIN32)
        set(ROOFLINE_CORE_NAME "roofline_core.lib")
    else()
        set(ROOFLINE_CORE_NAME "libroofline_core.a")
    endif()
    message(FATAL_ERROR
        "${ROOFLINE_CORE_NAME} not found at ${ROOFLINE_CORE_LIB}; build it with "
        "'cargo build --release -p miniperf-roofline-core' or pass -DROOFLINE_CORE_LIB=")
endif()

find_package(DynamoRIO REQUIRED)
if(WIN32)
    # The DynamoRIO build-tree package can map Release to RelWithDebInfo even
    # when it was built only as Release. Its imported targets then have real
    # *_RELEASE import libraries but CMake looks for absent *_RELWITHDEBINFO
    # ones. Installed bundles without Release libraries keep their own map.
    get_property(dr_imported_targets DIRECTORY PROPERTY IMPORTED_TARGETS)
    foreach(dr_target IN LISTS dr_imported_targets)
        get_target_property(dr_release_implib ${dr_target} IMPORTED_IMPLIB_RELEASE)
        get_target_property(dr_release_location ${dr_target} IMPORTED_LOCATION_RELEASE)
        if((dr_release_implib AND EXISTS "${dr_release_implib}") OR
           (dr_release_location AND EXISTS "${dr_release_location}"))
            set_property(TARGET ${dr_target} PROPERTY MAP_IMPORTED_CONFIG_RELEASE Release)
        endif()
    endforeach()
endif()

add_library(dr_roofline SHARED client.c)
configure_DynamoRIO_client(dr_roofline)
use_DynamoRIO_extension(dr_roofline drmgr)
use_DynamoRIO_extension(dr_roofline drutil)
use_DynamoRIO_extension(dr_roofline drreg)
use_DynamoRIO_extension(dr_roofline drx)
if(WIN32)
    target_link_libraries(dr_roofline ${ROOFLINE_CORE_LIB} ws2_32 userenv advapi32 bcrypt ntdll)
else()
    target_link_libraries(dr_roofline ${ROOFLINE_CORE_LIB} pthread dl m)
endif()
