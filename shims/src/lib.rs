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
    #[cfg(unix)]
    {
        next_to_current_exe(shim_name("mperf_libc"))
    }
    #[cfg(windows)]
    {
        None
    }
}

fn shim_name(stem: &str) -> &'static str {
    match stem {
        #[cfg(windows)]
        "mperf_ompt" => "mperf_ompt.dll",
        #[cfg(windows)]
        "mperf_itt" => "mperf_itt.dll",
        #[cfg(windows)]
        "mperf_mpi" => "mperf_mpi.dll",
        #[cfg(windows)]
        "mperf_cupti" => "mperf_cupti.dll",
        #[cfg(not(windows))]
        "mperf_libc" => "libmperf_libc.so",
        #[cfg(not(windows))]
        "mperf_ompt" => "libmperf_ompt.so",
        #[cfg(not(windows))]
        "mperf_itt" => "libmperf_itt.so",
        #[cfg(not(windows))]
        "mperf_mpi" => "libmperf_mpi.so",
        #[cfg(not(windows))]
        "mperf_cupti" => "libmperf_cupti.so",
        _ => "",
    }
}

/// Path of the OMPT tool library (OMP_TOOL_LIBRARIES).
pub fn ompt_shim() -> Option<PathBuf> {
    next_to_current_exe(shim_name("mperf_ompt"))
}

/// Path of the ITT collector (INTEL_LIBITTNOTIFY64).
pub fn itt_shim() -> Option<PathBuf> {
    next_to_current_exe(shim_name("mperf_itt"))
}

/// Path of the MPI proxy (PMPI preload).
pub fn mpi_shim() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        next_to_current_exe(shim_name("mperf_mpi"))
    }
    #[cfg(windows)]
    {
        None
    }
}

/// Path of the CUPTI injection library (CUDA_INJECTION64_PATH).
pub fn cupti_shim() -> Option<PathBuf> {
    next_to_current_exe(shim_name("mperf_cupti"))
}
