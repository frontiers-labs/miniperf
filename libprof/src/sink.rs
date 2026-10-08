//! What a source produces and where it puts it.
//!
//! Every source in this crate writes through [`Sink`]. The sink is the only
//! thing a source knows about its consumer, which is what lets the same source
//! feed the profiler's Parquet session, a test harness, or an application that
//! embeds libprof to watch itself.
//!
//! ```
//! use libprof::{Record, Sink};
//!
//! #[derive(Default)]
//! struct Counted(std::sync::atomic::AtomicUsize);
//!
//! impl Sink for Counted {
//!     fn record(&self, record: Record) {
//!         if let Record::Sample(_) = record {
//!             self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
//!         }
//!     }
//! }
//! ```

use smallvec::SmallVec;

use crate::Counter;

/// One measurement, on its way from a source to its consumer.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Keep samples inline on the sampling hot path.
pub enum Record {
    /// A performance-counter sample.
    Sample(Sample),
    /// A precise memory-access sample (PEBS/IBS/SPE).
    MemSample(MemSample),
    /// A process address-space mapping.
    ProcAddr(ProcAddr),
    /// One coarse resource observation: a clock, a temperature, a byte count.
    Resource(ResourceSample),
    /// A process observed in the target's tree.
    Process(ProcessInfo),
    /// A scalar summary produced once, at stop time.
    Metric {
        /// Table the metric belongs to, e.g. `"bpf"`.
        group: &'static str,
        /// Metric name within the group.
        name: String,
        /// Measured value.
        value: f64,
    },
}

/// Receives records from a source.
///
/// Sampling drivers call this from their reader threads, so the cost of an
/// implementation is paid on the sampling path: a sink that blocks for longer
/// than the ring takes to fill shows up as records the kernel dropped.
pub trait Sink: Send + Sync {
    /// Handles one record.
    fn record(&self, record: Record);
}

impl<F: Fn(Record) + Send + Sync> Sink for F {
    fn record(&self, record: Record) {
        self(record)
    }
}

/// Register state captured by `PERF_SAMPLE_REGS_USER`.
#[derive(Debug, Clone)]
pub struct UserRegs {
    /// Perf register ABI tag.
    pub abi: u64,
    /// Bit mask identifying captured architecture registers.
    pub mask: u64,
    /// Values are ordered by increasing set-bit index in `mask`.
    pub values: Vec<u64>,
}

/// A structure that represents a single sample
#[derive(Debug)]
pub struct Sample {
    /// Unique ID shared by all samples of the event
    pub event_id: u128,
    /// Instruction pointer
    pub ip: u64,
    /// Process ID
    pub pid: u32,
    /// Thread ID
    pub tid: u32,
    /// CPU ID that the event occured on
    pub cpu: u32,
    /// Family id of the core cluster this sample came from (e.g.
    /// `"cortex_a720"`), on a heterogeneous system. `None` on homogeneous hosts.
    pub core: Option<String>,
    /// Timestamp
    pub time: u64,
    /// Time for which the event was enabled.
    pub time_enabled: u64,
    /// Time for which the event was scheduled on hardware.
    pub time_running: u64,
    /// Counter represented by this sample.
    pub counter: Counter,
    /// Counter delta since the preceding sample.
    pub value: u64,
    /// Kernel-provided instruction-pointer callchain.
    pub callstack: SmallVec<[u64; 8]>,
    /// Call stack reconstructed from the hardware branch records (Intel LBR
    /// call-stack mode, AMD BRS / LbrV2 replay). Empty when branch records
    /// were not requested.
    pub lbr_callstack: SmallVec<[u64; 8]>,
    /// Raw user register state for post-hoc unwinding.
    pub user_regs: Option<UserRegs>,
    /// User stack bytes beginning at the sampled stack pointer.
    pub user_stack: Vec<u8>,
}

/// One precise memory access: skid-free IP plus the data address, vendor data
/// source encoding and access latency the hardware reported for it.
#[derive(Debug)]
pub struct MemSample {
    /// Instruction pointer of the accessing instruction.
    pub ip: u64,
    /// Process ID.
    pub pid: u32,
    /// Thread ID.
    pub tid: u32,
    /// CPU the access executed on.
    pub cpu: u32,
    /// Timestamp on `CLOCK_MONOTONIC`.
    pub time: u64,
    /// Virtual address the access targeted.
    pub data_addr: u64,
    /// Access latency in core cycles, zero when unreported.
    pub latency: u64,
    /// Raw vendor data-source encoding, normalized downstream.
    pub data_src: u64,
    /// Kernel-provided instruction-pointer callchain.
    pub callstack: SmallVec<[u64; 8]>,
    /// Call stack reconstructed from the hardware branch records.
    pub lbr_callstack: SmallVec<[u64; 8]>,
    /// Raw user register state for post-hoc unwinding.
    pub user_regs: Option<UserRegs>,
    /// User stack bytes beginning at the sampled stack pointer.
    pub user_stack: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// One process memory mapping.
pub struct ProcAddr {
    /// Process identifier.
    pub pid: u32,
    /// Mapping start address.
    pub addr: u64,
    /// Mapping length in bytes.
    pub len: u64,
    /// File offset backing the mapping.
    pub pgoff: u64,
    /// Path of the mapped file.
    pub filename: String,
}

/// One normalized coarse resource observation.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSample {
    /// Nanoseconds since the source started.
    pub timestamp_ns: u64,
    /// Resource kind, e.g. `"cpu"`, `"memory"`, `"gpu"`.
    pub resource: String,
    /// Instance within the kind, e.g. a cluster or device id.
    pub resource_id: String,
    /// USE category: `"utilization"`, `"saturation"` or `"errors"`.
    pub category: String,
    /// Metric name, e.g. `"frequency"`.
    pub metric: String,
    /// Measured value.
    pub value: f64,
    /// Unit of `value`, e.g. `"hertz"`.
    pub unit: String,
    /// What the value covers, e.g. `"system_during_target"`.
    pub scope: String,
    /// Where the value was read from, e.g. `"cpufreq"`.
    pub source: String,
    /// How faithful the value is, e.g. `"exact_system"`.
    pub quality: String,
}

/// One member of the process tree observed during a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process identifier.
    pub pid: u32,
    /// Parent process identifier.
    pub ppid: u32,
    /// Process start time in clock ticks, which disambiguates recycled PIDs.
    pub start_ticks: u64,
    /// Nanoseconds since the source started, at first observation.
    pub first_seen_ns: u64,
    /// Nanoseconds since the source started, at last observation.
    pub last_seen_ns: u64,
    /// Command name.
    pub command: String,
    /// How faithful the observation is.
    pub quality: String,
}

/// Availability and provenance for one source, recorded into the session so a
/// missing or degraded measurement is visible instead of silently absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    /// Source or signal this describes.
    pub name: String,
    /// One of `available`, `degraded`, `unavailable`, `permission_denied`,
    /// `error`.
    pub status: String,
    /// Facility the data came from, e.g. `"perf_events"`.
    pub source: String,
    /// How faithful the data is, e.g. `"exact"`, `"best_effort"`.
    pub quality: String,
    /// Human-readable detail, empty when there is nothing to add.
    pub message: String,
}

impl SourceStatus {
    /// A status entry, with every field owned.
    pub fn new(name: &str, status: &str, source: &str, quality: &str, message: &str) -> Self {
        SourceStatus {
            name: name.to_string(),
            status: status.to_string(),
            source: source.to_string(),
            quality: quality.to_string(),
            message: message.to_string(),
        }
    }

    /// Whether the source ran as intended.
    pub fn is_available(&self) -> bool {
        self.status == "available"
    }
}
