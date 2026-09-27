//! The host facilities every source is written against.
//!
//! This module selects the host implementation for shared process and clock
//! facilities. Specialized drivers, timers, and telemetry collectors have
//! their own platform modules. Callers receive `None`, an empty result, or an
//! `Unsupported` error where a facility is unavailable.

use crate::sink::ProcAddr;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// One process observed in `/proc`-style process accounting.
#[derive(Clone, Debug, Default)]
pub struct ProcessStat {
    /// Process identifier.
    pub pid: u32,
    /// Parent process identifier.
    pub ppid: u32,
    /// Single-letter process state; `Z` for a zombie.
    pub state: u8,
    /// Command name.
    pub command: String,
    /// Start time in clock ticks, which disambiguates recycled PIDs.
    pub start_ticks: u64,
    /// User CPU time in clock ticks.
    pub user_ticks: u64,
    /// System CPU time in clock ticks.
    pub system_ticks: u64,
    /// Minor page faults taken so far.
    pub minor_faults: u64,
    /// Major page faults taken so far.
    pub major_faults: u64,
    /// Resident set size in pages.
    pub rss_pages: i64,
}

/// Byte and call counts from a process's I/O accounting.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessIo {
    /// Bytes read, including from page cache.
    pub read_bytes: u64,
    /// Bytes written.
    pub write_bytes: u64,
    /// Read syscalls issued.
    pub read_calls: u64,
    /// Write syscalls issued.
    pub write_calls: u64,
}

/// Context switches a process has taken.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessContext {
    /// Switches the process asked for.
    pub voluntary: u64,
    /// Switches the scheduler forced.
    pub involuntary: u64,
}

/// Every process descended from `root_pid`, `root_pid` included.
///
/// `None` where the host exposes no process tree; callers then measure the
/// root process alone and say so in their status.
pub fn process_tree(root_pid: u32) -> Option<Vec<ProcessStat>> {
    imp::process_tree(root_pid)
}

/// Whether this host exposes a process tree at all, without walking it.
pub fn process_tree_supported() -> bool {
    imp::process_tree_supported()
}

/// I/O accounting for one process, all zero where the host does not report it.
pub fn process_io(pid: u32) -> ProcessIo {
    imp::process_io(pid)
}

/// Context-switch counts for one process, all zero where unreported.
pub fn process_context(pid: u32) -> ProcessContext {
    imp::process_context(pid)
}

/// Resident memory of a live process in bytes, where the host exposes it.
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        return imp::process_rss_bytes(pid);
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kbytes| kbytes.saturating_mul(1024))
    }
}

/// Configured logical CPU count, including CPUs outside the current affinity.
pub fn configured_cpu_count() -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        imp::configured_cpu_count()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let configured = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        (configured > 0)
            .then(|| u32::try_from(configured).ok())
            .flatten()
    }
}

/// Timestamp in nanoseconds on the host's monotonic clock.
pub fn monotonic_timestamp_ns() -> std::io::Result<u64> {
    #[cfg(target_os = "windows")]
    {
        imp::monotonic_timestamp_ns()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut timestamp = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((timestamp.tv_sec as u64) * 1_000_000_000 + timestamp.tv_nsec as u64)
    }
}

/// Whether the process is still alive.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        return imp::process_alive(pid);
    }
    #[cfg(not(target_os = "windows"))]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
}

/// Executable mappings of a live process, for symbolization.
///
/// Empty when the process is gone or the host exposes no mapping interface.
pub fn process_modules(pid: u32) -> Vec<ProcAddr> {
    imp::process_modules(pid)
}

/// Identifier of the calling thread as the kernel reports it.
pub fn current_thread_id() -> u64 {
    imp::current_thread_id()
}

/// Clock ticks per second, for turning `ProcessStat` times into seconds.
pub fn ticks_per_second() -> f64 {
    #[cfg(target_os = "windows")]
    {
        imp::ticks_per_second()
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64
    }
}

/// Page size in bytes, for turning `rss_pages` into bytes.
pub fn page_size() -> f64 {
    #[cfg(target_os = "windows")]
    {
        return imp::page_size();
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as f64
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported {
    use super::{ProcessContext, ProcessIo, ProcessStat};
    use crate::sink::ProcAddr;

    pub(super) fn process_tree(_root_pid: u32) -> Option<Vec<ProcessStat>> {
        None
    }

    pub(super) fn process_tree_supported() -> bool {
        false
    }

    pub(super) fn process_io(_pid: u32) -> ProcessIo {
        ProcessIo::default()
    }

    pub(super) fn process_context(_pid: u32) -> ProcessContext {
        ProcessContext::default()
    }

    pub(super) fn process_modules(_pid: u32) -> Vec<ProcAddr> {
        Vec::new()
    }

    pub(super) fn current_thread_id() -> u64 {
        0
    }
}
