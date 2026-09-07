#![deny(missing_docs)]
//! Low-overhead hardware-counter timing and in-memory perf sampling.
//!
//! [`EventTimer`] measures focused scopes, while [`QuickSampler`] records a
//! closure into memory without the profiler's file or dispatcher machinery.

mod capabilities;
mod cpu_family;
#[cfg(feature = "criterion")]
mod criterion_measurement;
mod driver;
mod event_timer;
mod features;
mod host_telemetry;
mod platform;
mod platform_memory;
mod process;
mod quick;
mod sampling_probe;
mod sink;
mod source;
mod topdown;
mod unwind;

pub use capabilities::{capabilities, Capabilities, PmuDevice};
pub use cpu_family::{host_cpu_description, host_metrics};
#[cfg(feature = "criterion")]
pub use criterion_measurement::CriterionCounter;
pub use driver::{inherited_sampling_supported, mem_sampling_driver};
pub use driver::{
    list_supported_counters, CoreId, CounterEntry, CounterResult, CounterValue, CountingDriver,
    CountingDriverBuilder, DriverKind, MeasurementQuality, SamplingDriver, SamplingDriverBuilder,
    UnwindMode,
};
#[cfg(feature = "criterion")]
pub use event_timer::CounterCheckpoint;
pub use event_timer::{
    CounterStatistics, EventTimer, Measurement, MeasurementSpan, MeasurementStatistics,
    Measurements, ReadCost, ReadMethod,
};
pub use features::{resolve, Feature, Mechanism, Resolution, Satisfied};
pub use host_telemetry::{
    ClusterClocks, DeviceClocks, HostTelemetry, HostTelemetrySample, ThermalZone,
};
pub use platform::{current_thread_id, process_alive, process_modules, process_tree, ProcessStat};
pub use platform_memory::{
    bandwidth_counters_present, MemoryControllerMonitor, MemoryControllerSample,
};
pub use pmu_data::{Metric, MetricError, MetricExpression};
pub use process::Process;
#[cfg(feature = "symbolize")]
pub use quick::{top_symbols, SymbolCount};
pub use quick::{QuickSampler, SampleBatch};
pub use sampling_probe::{probe_sampling_group, SamplingProbe};
pub use sink::{
    MemSample, ProcAddr, ProcessInfo, Record, ResourceSample, Sample, Sink, SourceStatus, UserRegs,
};
pub use source::{
    Availability, BpfSource, HostTelemetrySource, InternalEventsSource, PmuSamplingSource,
    PreciseMemorySource, ProcfsSource, SessionContext, Source, SourceDecl,
};
pub use topdown::{is_topdown_event, sysfs_alias, GROUP_LEADER};
pub use unwind::{Frames, PostHocUnwinder, StackSample};

/// Default sampling frequency used by [`SamplingDriverBuilder`].
pub const DEFAULT_SAMPLE_FREQUENCY_HZ: u64 = 1_000;

/// Returns the top-down analysis scenario for the detected host CPU, if one is defined.
pub fn host_tma_scenario() -> Option<pmu_data::TmaScenario> {
    cpu_family::host_tma_scenario()
}

/// The top-down scenario a recording will actually use: the hardware's fixed
/// topdown counters when it has them, otherwise the event arithmetic defined
/// for the detected CPU family.
pub fn tma_scenario() -> Option<pmu_data::TmaScenario> {
    topdown::scenario(&capabilities()).or_else(host_tma_scenario)
}

/// The counter a top-down scenario event name maps to.
pub fn tma_counter(event: &str) -> Counter {
    match event {
        "cycles" => Counter::Cycles,
        "instructions" => Counter::Instructions,
        "stalled_cycles_frontend" => Counter::StalledCyclesFrontend,
        "stalled_cycles_backend" => Counter::StalledCyclesBackend,
        _ => Counter::Custom(event.to_owned()),
    }
}

/// Maximum number of events in one coherent group on the host PMU, when known.
pub fn host_max_counters() -> Option<usize> {
    cpu_family::host_max_counters()
}

/// The core clusters present on the host, on a heterogeneous (big.LITTLE)
/// system. Returns an empty vector on homogeneous systems (a single cluster),
/// where per-core attribution is meaningless.
pub fn host_core_clusters() -> Vec<CoreId> {
    let pmus = cpu_family::host_core_pmus();
    if pmus.len() <= 1 {
        return Vec::new();
    }

    pmus.iter()
        .map(|pmu| {
            let name = cpu_family::find_cpu_family(pmu.family_id)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| pmu.family_id.to_string());
            CoreId {
                family_id: pmu.family_id.to_string(),
                name,
                cpus: pmu.cpus.clone(),
            }
        })
        .collect()
}

use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
/// A hardware, software, or platform-specific performance counter.
pub enum Counter {
    /// CPU cycles.
    Cycles,
    /// Retired instructions.
    Instructions,
    /// Last-level-cache references.
    LLCReferences,
    /// Last-level-cache misses.
    LLCMisses,
    /// Retired branch instructions.
    BranchInstructions,
    /// Mispredicted branch instructions.
    BranchMisses,
    /// Cycles stalled by the processor frontend.
    StalledCyclesFrontend,
    /// Cycles stalled by the processor backend.
    StalledCyclesBackend,
    /// Software CPU-clock time.
    CpuClock,
    /// Page faults.
    PageFaults,
    /// Context switches.
    ContextSwitches,
    /// CPU migrations.
    CpuMigrations,
    /// A named event resolved through the active platform event table.
    Custom(String),
    /// A resolved raw event used internally and by advanced callers.
    Internal {
        /// Event name.
        name: String,
        /// Event description.
        desc: String,
        /// Raw perf event encoding.
        code: u64,
    },
}

#[derive(Error, Debug)]
/// Errors produced while configuring or reading performance events.
pub enum Error {
    /// The kernel rejected `perf_event_open` for a counter.
    #[error(
        "perf_event_open failed for counter '{counter}' ({scope}): errno {errno} ({source}); {hint}"
    )]
    PerfEventOpen {
        /// Counter that failed to open.
        counter: String,
        /// Thread or CPU scope used for the event.
        scope: String,
        /// Operating-system error number.
        errno: i32,
        /// Actionable diagnostic guidance.
        hint: String,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A perf control ioctl failed.
    #[error("perf ioctl {operation} failed for counter '{counter}': {source}")]
    PerfIoctl {
        /// Perf ioctl operation name.
        operation: &'static str,
        /// Counter affected by the ioctl.
        counter: String,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A perf metadata or sampling-ring mapping failed.
    #[error(
        "failed to mmap the sampling buffer for counter '{counter}' ({length} bytes): {source}"
    )]
    PerfMmap {
        /// Counter whose buffer could not be mapped.
        counter: String,
        /// Requested mapping length.
        length: usize,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    #[error("counter '{counter}' is not available for CPU family '{family}'")]
    /// A counter absent from the selected CPU-family table.
    UnsupportedCounter {
        /// Requested counter name.
        counter: String,
        /// Detected CPU family.
        family: String,
    },
    #[error("driver '{driver}' is not available on this platform")]
    /// A driver unavailable on the current operating system.
    UnsupportedDriver {
        /// Requested driver name.
        driver: String,
    },
    /// The requested counter or sampler configuration is invalid.
    #[error("invalid counter configuration: {0}")]
    InvalidConfiguration(String),
    /// A sampling reader thread panicked.
    #[error("sampling worker thread panicked")]
    WorkerPanicked,
    /// The kernel dropped sampling records because a perf ring overflowed.
    #[error("perf sampling lost {count} records; reduce sampling frequency or increase the perf ring allowance")]
    SamplesLost {
        /// Number of records reported lost by the kernel.
        count: u64,
    },
    /// Every sampling group the target reached reported enabled time and no
    /// running time: the kernel accepted groups the hardware cannot schedule.
    #[error(
        "the sampling groups asked for more hardware counters than this host has free, so the kernel never scheduled them and the recording has no samples ({groups})"
    )]
    SamplingGroupNeverScheduled {
        /// Enabled and running time of every group leader, for the report.
        groups: String,
    },
    /// A grouped counter read failed.
    #[error("failed to read perf counter group: {source}")]
    PerfRead {
        /// Underlying read error.
        #[source]
        source: std::io::Error,
    },
    /// A closure passed to quick sampling panicked.
    #[error("sampled workload panicked")]
    WorkloadPanicked,
    /// The native backend could not configure its requested counters.
    #[error("failed to create native performance counters")]
    CounterCreationFail,
    /// The native backend could not enable or stop its counters.
    #[error("failed to enable native performance counters")]
    EnableFailed,
    /// macOS denied access to the private kperf interfaces.
    #[error("macOS KPC/kperf access was denied; try running this command with sudo")]
    PermissionDenied,
}

impl Error {
    #[cfg(target_os = "linux")]
    pub(crate) fn perf_event_open(counter: &Counter, cpu: Option<i32>) -> Self {
        let source = std::io::Error::last_os_error();
        let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok());
        Self::perf_event_open_with(counter, cpu, source, paranoid)
    }

    #[cfg(any(target_os = "linux", test))]
    fn perf_event_open_with(
        counter: &Counter,
        cpu: Option<i32>,
        source: std::io::Error,
        paranoid: Option<i32>,
    ) -> Self {
        let errno = source.raw_os_error().unwrap_or(0);
        let hint = perf_error_hint(errno, paranoid);
        Self::PerfEventOpen {
            counter: counter.name().to_owned(),
            scope: cpu.map_or_else(|| "this thread".to_owned(), |cpu| format!("CPU {cpu}")),
            errno,
            hint,
            source,
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn perf_ioctl(operation: &'static str, counter: &Counter) -> Self {
        Self::PerfIoctl {
            operation,
            counter: counter.name().to_owned(),
            source: std::io::Error::last_os_error(),
        }
    }

    /// Returns whether the kernel reported that an event does not exist.
    pub fn is_event_unsupported(&self) -> bool {
        matches!(self, Self::PerfEventOpen { errno, .. } if *errno == libc::ENOENT)
    }

    /// Returns the affected counter name when the error carries one.
    pub fn counter_name(&self) -> Option<&str> {
        match self {
            Self::PerfEventOpen { counter, .. } | Self::UnsupportedCounter { counter, .. } => {
                Some(counter)
            }
            _ => None,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn perf_error_hint(errno: i32, paranoid: Option<i32>) -> String {
    match errno {
        libc::EACCES | libc::EPERM => {
            let paranoid = paranoid.map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            format!(
                "check /proc/sys/kernel/perf_event_paranoid (currently {paranoid}) or grant CAP_PERFMON"
            )
        }
        libc::ENOENT => "event is not supported by this PMU".to_owned(),
        libc::E2BIG | libc::EINVAL => {
            "the kernel rejected the perf_event_attr; the event or attribute combination may be unsupported"
                .to_owned()
        }
        libc::EMFILE | libc::ENFILE => {
            "file descriptor limit reached; raise ulimit -n or close other descriptors".to_owned()
        }
        _ => "perf_event_open failed; inspect the kernel log and PMU availability".to_owned(),
    }
}

impl Counter {
    /// Returns the stable perf-style counter name.
    pub fn name(&self) -> &str {
        match self {
            Counter::Cycles => "cycles",
            Counter::Instructions => "instructions",
            Counter::LLCReferences => "llc_references",
            Counter::LLCMisses => "llc_misses",
            Counter::BranchInstructions => "branches",
            Counter::BranchMisses => "branch_misses",
            Counter::StalledCyclesFrontend => "stalled_cycles_frontend",
            Counter::StalledCyclesBackend => "stalled_cycles_backend",
            Counter::CpuClock => "cpu_clock",
            Counter::PageFaults => "page_faults",
            Counter::ContextSwitches => "context_switches",
            Counter::CpuMigrations => "cpu_migrations",
            Counter::Custom(name) => name,
            Counter::Internal {
                name,
                desc: _,
                code: _,
            } => name,
        }
    }

    /// Returns a human-readable counter description when available.
    pub fn description(&self) -> &str {
        match self {
            Counter::Cycles => "Number of CPU cycles",
            Counter::Instructions => "Number of instructions retired",
            Counter::LLCReferences => "Last level cache references",
            Counter::LLCMisses => "Last level cache misses",
            Counter::BranchInstructions => "Branch instructions retired",
            Counter::BranchMisses => "Branch instruction missess",
            Counter::StalledCyclesFrontend => {
                "Number of cycles stalled due to frontend bottlenecks"
            }
            Counter::StalledCyclesBackend => "Number of cycles stalled due to backend bottlenecks",
            Counter::CpuClock => "A high-resolution per-CPU timer",
            Counter::PageFaults => "Number of page faults",
            Counter::ContextSwitches => "Number of context switches",
            Counter::CpuMigrations => "Number of the times the process has migrated to a new CPU",
            Counter::Custom(_) => "",
            Counter::Internal {
                name: _,
                desc,
                code: _,
            } => desc,
        }
    }

    /// Returns whether the counter is implemented by perf's software PMU.
    pub fn is_software(&self) -> bool {
        matches!(
            self,
            Counter::CpuClock
                | Counter::PageFaults
                | Counter::ContextSwitches
                | Counter::CpuMigrations
        )
    }
}
