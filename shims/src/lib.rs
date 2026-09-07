//! Locator for the miniperf proxy shims (§4 of the event-collection
//! redesign). Each shim is a pure-Rust cdylib crate under `shims/`, built by
//! cargo like every other workspace member — no external C toolchain.

use std::path::PathBuf;

fn next_to_current_exe(name: &str) -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    // `../lib/miniperf` is where utils/package-miniperf.sh installs these, and
    // is the only one of the three that a released package uses; the other two
    // serve a cargo target directory and a plain `lib` layout. Omitting it
    // meant every shipped package resolved no shim at all and recorded nothing
    // from them, silently.
    [
        path.join(name),
        path.join("../lib/miniperf").join(name),
        path.join("../lib").join(name),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

/// Path of the libc LD_PRELOAD shim, when built for this target.
pub fn libc_shim() -> Option<PathBuf> {
    next_to_current_exe("libmperf_libc.so")
}

/// Path of the OMPT tool library (OMP_TOOL_LIBRARIES).
pub fn ompt_shim() -> Option<PathBuf> {
    next_to_current_exe("libmperf_ompt.so")
}

/// Path of the ITT collector (INTEL_LIBITTNOTIFY64).
pub fn itt_shim() -> Option<PathBuf> {
    next_to_current_exe("libmperf_itt.so")
}

/// Path of the MPI proxy (PMPI preload).
pub fn mpi_shim() -> Option<PathBuf> {
    next_to_current_exe("libmperf_mpi.so")
}

/// Path of the CUPTI injection library (CUDA_INJECTION64_PATH).
pub fn cupti_shim() -> Option<PathBuf> {
    next_to_current_exe("libmperf_cupti.so")
}
