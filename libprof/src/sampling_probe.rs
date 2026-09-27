//! Runs a scenario's sampling group against a live workload and reports what
//! actually came back.
//!
//! Opening `cpu-cycles` proves a PMU exists; it does not prove that the group
//! a scenario builds can be scheduled. `perf_event_open` accepts a group the
//! hardware cannot host and the kernel then never runs it, so the recording
//! ends up with no hardware counters and nothing says so. Every check that
//! claims a host is ready to profile goes through here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::{Counter, Error, Process, Record, SamplingDriverBuilder, Sink};

#[cfg(target_os = "windows")]
#[path = "sampling_probe/windows.rs"]
mod windows;

/// What a scenario's sampling group produced on this host.
#[derive(Debug, Clone)]
pub struct SamplingProbe {
    /// Counters the caller asked for.
    pub requested: Vec<Counter>,
    /// Counters the driver opened, after any capability fallback.
    pub opened: Vec<Counter>,
    /// Samples delivered while the workload ran.
    pub samples: usize,
}

impl SamplingProbe {
    /// Hardware counters that survived into the group.
    pub fn hardware_opened(&self) -> Vec<&Counter> {
        self.opened
            .iter()
            .filter(|counter| !counter.is_software())
            .collect()
    }

    /// Hardware counters that were asked for and did not survive.
    pub fn hardware_dropped(&self) -> Vec<&Counter> {
        self.requested
            .iter()
            .filter(|counter| !counter.is_software() && !self.opened.contains(counter))
            .collect()
    }

    /// Whether the group ran as asked: every hardware counter opened, and
    /// samples came back.
    pub fn is_healthy(&self) -> bool {
        self.samples > 0 && self.hardware_dropped().is_empty()
    }

    /// Whether the group collapsed to software events, which yields a
    /// recording with no `pmu_*` columns at all.
    pub fn collapsed_to_software(&self) -> bool {
        self.hardware_opened().is_empty()
            && self.requested.iter().any(|counter| !counter.is_software())
    }
}

struct CountingSink(AtomicUsize);

impl Sink for CountingSink {
    fn record(&self, _record: Record) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Samples a short-lived spinning child with `counters` and reports the result.
///
/// The child is launched through the same process path as a recording and
/// loops inside its shell until `millis` elapses. A loop that forks a helper
/// per iteration would put work in grandchildren, which some sampling groups
/// cannot count, and make a working PMU appear dead.
pub fn probe_sampling_group(counters: &[Counter], millis: u64) -> Result<SamplingProbe, Error> {
    #[cfg(not(target_os = "windows"))]
    let command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "while :; do :; done".to_owned(),
    ];
    #[cfg(target_os = "windows")]
    let command = windows::workload_command();
    let process = Process::new(&command, &[]).map_err(|error| {
        Error::InvalidConfiguration(format!(
            "sampling probe could not start a workload: {error}"
        ))
    })?;

    let mut driver = SamplingDriverBuilder::new()
        .counters(counters)
        .process(&process)
        .build()?;
    let sink = Arc::new(CountingSink(AtomicUsize::new(0)));

    driver.start(sink.clone())?;
    // Some backends can lose a hardware source while starting (for example,
    // Windows falls back to a CPU-time sampler if ETW rejects the session).
    // Report the counters that are actually active, not the planned set.
    let opened = driver.counters();
    process.cont();
    std::thread::sleep(std::time::Duration::from_millis(millis));
    #[cfg(not(target_os = "windows"))]
    unsafe {
        libc::kill(process.pid(), libc::SIGKILL)
    };
    #[cfg(target_os = "windows")]
    windows::stop_workload(&process);
    let _ = process.wait();
    driver.stop()?;

    Ok(SamplingProbe {
        requested: counters.to_vec(),
        opened,
        samples: sink.0.load(Ordering::Relaxed),
    })
}
