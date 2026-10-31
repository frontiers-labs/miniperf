//! The two wall clocks a recording is anchored to.
//!
//! `monotonic_ns` must return the same `CLOCK_MONOTONIC` a perf sample's
//! timestamp comes from, so it is a raw syscall rather than an `Instant`
//! delta. That syscall is the one thing in this crate the platform has to
//! choose, which is why it lives alone in this file: everything around it
//! stays under the platform-cfg guard.

/// Nanoseconds on `CLOCK_MONOTONIC`, the clock perf timestamps use.
#[cfg(unix)]
pub(crate) fn monotonic_ns() -> i64 {
    // tv_sec and tv_nsec are already i64 on 64-bit Linux and narrower
    // elsewhere, so the conversion is a no-op on the platform clippy happens
    // to be running on.
    #[allow(clippy::useless_conversion)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        i64::from(ts.tv_sec) * 1_000_000_000 + i64::from(ts.tv_nsec)
    }
}

/// Windows has no `clock_gettime`. It only ever reads recordings — the
/// collector is Unix-only — but the table builders still have to compile
/// there, and a process-relative monotonic clock is enough to read with.
#[cfg(not(unix))]
pub(crate) fn monotonic_ns() -> i64 {
    use std::{sync::OnceLock, time::Instant};

    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    i64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(i64::MAX)
}

/// Nanoseconds since the Unix epoch. `SystemTime` is `CLOCK_REALTIME`
/// everywhere it exists, so this needs no platform of its own.
pub(crate) fn realtime_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or_default()
}
