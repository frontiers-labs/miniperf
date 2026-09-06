use clap::ValueEnum;
use pmu_data::{ScenarioUi, TmaConstant, TmaGroup, TmaMetric};
use serde::{Deserialize, Serialize};

mod event;
mod manifest;
mod mem;
mod ui;

pub use event::{CallFrame, Event, EventType, IString, Location, ProcMapEntry, UserRegs};
pub use manifest::{
    CounterRender, CounterSpec, MarkerSpec, Severity, TrackSpec, ViewSpec, VisualizationManifest,
};
pub use mem::{MemDataSource, MemLevel, MemOp, MemSample, MemSnoop, MemTlb};

pub use ui::{resources_table_spec, scenario_ui};

/// Version of the on-disk results format written by this build.
///
/// Version 3 is the all-Parquet session directory: every table is a Parquet
/// file in the result directory and identities are XXH3-64 hashes. Older
/// versions are not readable; re-record.
pub const CURRENT_FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scenario {
    Snapshot,
    Mem,
    Roofline,
    TMA,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub pid: i32,
    pub counters: Vec<(EventType, String)>,
    /// How completely the process tree was observed.
    #[serde(default = "default_snapshot_scope")]
    pub scope: String,
    /// Coarse resource sampling period.
    #[serde(default = "default_snapshot_interval_ms")]
    pub interval_ms: u64,
    /// Why collection stopped, when known.
    #[serde(default)]
    pub stop_reason: String,
    /// Collector capability and degradation records.
    #[serde(default)]
    pub collectors: Vec<SnapshotCollectorStatus>,
    /// Limitations which result consumers must keep visible.
    #[serde(default)]
    pub warnings: Vec<String>,
}

fn default_snapshot_scope() -> String {
    "legacy_root_only".to_string()
}

const fn default_snapshot_interval_ms() -> u64 {
    1_000
}

/// Availability and provenance for one snapshot data source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotCollectorStatus {
    pub name: String,
    pub status: String,
    pub source: String,
    pub quality: String,
    #[serde(default)]
    pub message: String,
}

/// One normalized coarse resource observation from a snapshot recording.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotResourceSample {
    pub timestamp_ns: u64,
    pub resource: String,
    #[serde(default)]
    pub resource_id: String,
    pub category: String,
    pub metric: String,
    pub value: f64,
    pub unit: String,
    pub scope: String,
    pub source: String,
    pub quality: String,
}

/// One member of the process tree observed during a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub start_ticks: u64,
    pub first_seen_ns: u64,
    pub last_seen_ns: u64,
    pub command: String,
    pub quality: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// PID of the native, timed process.
    pub perf_pid: i32,
    /// PID of the QEMU accounting process.
    pub accounting_pid: i32,
    pub counters: Vec<(EventType, String)>,
    /// Complete address accounting is process-specific and modeled; hardware
    /// memory-controller counters, when present, are system-scoped.
    pub method: MemoryMethodInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryMethodInfo {
    pub selection: String,
    pub accounting: String,
    pub performance: String,
    pub bandwidth: String,
    pub quality: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RooflineInfo {
    #[serde(default = "default_roofline_backend")]
    pub backend: String,
    pub perf_pid: i32,
    pub counters: Vec<(EventType, String)>,
    pub inst_pid: i32,
    /// How the profiler selected and composed this Roofline measurement.
    /// Absent in recordings produced before automatic method selection.
    #[serde(default)]
    pub method: Option<Box<RooflineMethodInfo>>,
}

fn default_roofline_backend() -> String {
    "compiler".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RooflineMethodInfo {
    /// `auto` for capability-based selection, otherwise `explicit`.
    pub selection: String,
    /// Source of operation and byte accounting, such as `qemu` or `compiler`.
    pub accounting: String,
    /// Source of elapsed time: `native` or `qemu`.
    pub performance: String,
    /// Meaning of the byte denominator, such as `architectural`, `dram`, or
    /// `dram-model` for deterministic last-level-cache modeling.
    ///
    /// A memory-bandwidth roof is compatible only with traffic measured at the
    /// same hierarchy level. Recordings predating this field deserialize as
    /// `unknown` and must not silently use a DRAM roof.
    #[serde(default = "default_roofline_traffic")]
    pub traffic: String,
    /// Short machine-readable quality classification.
    pub quality: String,
    /// Human-readable explanation of why this method was selected.
    pub reason: String,
    /// Limitations which must remain visible to result consumers.
    #[serde(default)]
    pub warnings: Vec<String>,
}

fn default_roofline_traffic() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TMAInfo {
    pub pid: i32,
    pub counters: Vec<(EventType, String)>,
    #[serde(default)]
    pub groups: Vec<TmaGroup>,
    /// True only when the recording used a precise PMU sampling source.
    #[serde(default)]
    pub precise_attribution: bool,
    pub metrics: Vec<TmaMetric>,
    pub constants: Vec<TmaConstant>,
    #[serde(default)]
    pub ui: Option<ScenarioUi>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScenarioInfo {
    Snapshot(SnapshotInfo),
    Mem(MemoryInfo),
    Roofline(RooflineInfo),
    TMA(TMAInfo),
}

/// A cluster of cores on a heterogeneous (big.LITTLE) system, used to attribute
/// samples to a specific core type during post-processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreCluster {
    /// Family id, e.g. `"cortex_a720"`.
    pub family_id: String,
    /// Human readable name, e.g. `"ARM Cortex-A720"`.
    pub name: String,
    /// sysfs cpumask string, e.g. `"0,5-11"`.
    pub cpus: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuInfo {
    /// Sustainable host memory bandwidth, shared by memory and Roofline analyses.
    #[serde(default)]
    pub memory_calibration: Option<Box<MemoryBandwidthCalibration>>,
    /// Host ceilings measured immediately before a Roofline recording.
    #[serde(default)]
    pub roofline_calibration: Option<Box<RooflineCalibration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBandwidthCalibration {
    pub threads: usize,
    #[serde(default)]
    pub cpu_affinity: Option<String>,
    pub samples: usize,
    pub gbytes_per_second: f64,
    #[serde(default)]
    pub gbytes_per_second_samples: Vec<f64>,
    pub working_set_bytes: u64,
    /// `effective_stream` until a supported memory-controller PMU is used.
    pub source: String,
    /// Measured bandwidth roofs and detected capacities for the source host's
    /// cache hierarchy, innermost first. Older recordings contain no levels.
    #[serde(default)]
    pub memory_levels: Vec<MemoryLevelCalibration>,
}

impl From<&RooflineCalibration> for MemoryBandwidthCalibration {
    fn from(value: &RooflineCalibration) -> Self {
        Self {
            threads: value.threads,
            cpu_affinity: value.cpu_affinity.clone(),
            samples: value.samples,
            gbytes_per_second: value.memory_gbytes_per_second,
            gbytes_per_second_samples: value.memory_gbytes_per_second_samples.clone(),
            working_set_bytes: value.memory_working_set_bytes,
            source: "effective_stream".to_string(),
            memory_levels: value.memory_levels.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RooflineCalibration {
    /// Number of Rayon workers used by both calibration kernels.
    pub threads: usize,
    /// Host CPUs on which the process was allowed to run, when available.
    #[serde(default)]
    pub cpu_affinity: Option<String>,
    /// Samples contributing to each reported median.
    pub samples: usize,
    /// Name of the architecture-specific FP64 compute kernel.
    pub compute_kernel: String,
    /// Median aggregate FP64 throughput.
    pub fp64_gflops: f64,
    /// Individual aggregate FP64 throughput samples.
    #[serde(default)]
    pub fp64_gflops_samples: Vec<f64>,
    /// Median aggregate triad bandwidth.
    pub memory_gbytes_per_second: f64,
    /// Individual aggregate triad-bandwidth samples.
    #[serde(default)]
    pub memory_gbytes_per_second_samples: Vec<f64>,
    /// Compute/bandwidth intersection.
    pub ridge_point_flops_per_byte: f64,
    /// Bytes in all three buffers used by the memory calibration.
    pub memory_working_set_bytes: u64,
    /// Bandwidth roofs per memory hierarchy level, innermost first. The
    /// cache-aware roofline plots loops against architectural traffic, so one
    /// DRAM roof is not enough: a cache-resident loop is served far above DRAM
    /// bandwidth and needs the cache-level roofs to be read correctly.
    #[serde(default)]
    pub memory_levels: Vec<MemoryLevelCalibration>,
    /// The same kernels run on one worker, so a loop that ran on one thread
    /// has a ceiling of its own. Absent when the pool had a single worker.
    #[serde(default)]
    pub single_thread: Option<RooflineCeilings>,
}

/// One set of measured ceilings for a given worker count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RooflineCeilings {
    pub threads: usize,
    pub fp64_gflops: f64,
    #[serde(default)]
    pub fp64_gflops_samples: Vec<f64>,
    pub memory_gbytes_per_second: f64,
    #[serde(default)]
    pub memory_gbytes_per_second_samples: Vec<f64>,
    pub ridge_point_flops_per_byte: f64,
    #[serde(default)]
    pub memory_levels: Vec<MemoryLevelCalibration>,
}

/// One measured bandwidth roof of the memory hierarchy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MemoryLevelCalibration {
    /// `L1`, `L2`, `L3` or `DRAM`.
    pub level: String,
    pub gbytes_per_second: f64,
    #[serde(default)]
    pub gbytes_per_second_samples: Vec<f64>,
    /// Total bytes touched across all threads by this level's kernel.
    pub working_set_bytes: u64,
    /// Total detected capacity of this cache level on the source host. DRAM
    /// has no cache capacity and stores zero. Missing in older recordings.
    #[serde(default)]
    pub capacity_bytes: u64,
    /// Number of logical CPUs sharing the detected cache. Zero means unknown.
    #[serde(default)]
    pub shared_by: usize,
}

/// How `os_cpu_clock` observations should be interpreted by time-based views.
///
/// A counter delta is paired with an explicit wall-time interval. Sampled
/// occupancy attributes a CPU-time weight to the logical CPU reported at the
/// sample endpoint: Darwin kperf emits a fixed timer period, while Linux perf
/// emits a task-clock delta from one authoritative sampling group. Endpoint
/// attribution is statistical when a task may have migrated within the delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuClockSource {
    CounterDelta,
    SampledOccupancy,
}

impl CpuClockSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CounterDelta => "counter_delta",
            Self::SampledOccupancy => "sampled_occupancy",
        }
    }
}

/// The capture strategy one scenario ran at, and the better strategies this
/// host could not support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFidelity {
    pub scenario: String,
    /// Identifier of the chosen rung, e.g. `counter_only`.
    pub rung: String,
    pub rejected: Vec<RejectedRung>,
}

/// A capture strategy that was preferred but unavailable here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedRung {
    pub rung: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordInfo {
    /// On-disk results format. Missing means the legacy, pre-versioned format.
    #[serde(default)]
    pub format_version: u32,
    pub scenario: Scenario,
    pub command: Option<Vec<String>>,
    pub cpu_model: String,
    pub cpu_vendor: String,
    /// Requested profiler sampling frequency. Missing on recordings written
    /// before temporal sample metadata was persisted.
    #[serde(default)]
    pub sampling_frequency_hz: Option<u64>,
    /// Semantics of timestamped CPU-clock observations. Missing on recordings
    /// created before CPU-lane data was preserved independently.
    #[serde(default)]
    pub cpu_clock_source: Option<CpuClockSource>,
    /// Number of logical CPUs in the source host topology. Current recordings
    /// use contiguous logical CPU IDs `0..logical_cpu_count`; missing means the
    /// recording predates topology preservation.
    #[serde(default)]
    pub logical_cpu_count: Option<u32>,
    /// Core clusters on a heterogeneous host (empty on homogeneous systems).
    #[serde(default)]
    pub cores: Vec<CoreCluster>,
    /// Extensible host CPU measurements collected with this recording.
    #[serde(default)]
    pub cpu_info: CpuInfo,
    /// Capture fidelity resolved for this recording. Empty in recordings made
    /// before the fidelity ladder existed.
    #[serde(default)]
    pub capture_fidelity: Vec<CaptureFidelity>,
    /// Per-collector outcome for this recording, in any scenario. Empty in
    /// recordings made before collector status was scenario-independent.
    #[serde(default)]
    pub collectors: Vec<SnapshotCollectorStatus>,
    pub scenario_info: ScenarioInfo,
}

impl RecordInfo {
    pub fn ensure_supported_format(&self) -> Result<(), UnsupportedFormatVersion> {
        if self.format_version != CURRENT_FORMAT_VERSION {
            return Err(UnsupportedFormatVersion {
                found: self.format_version,
                supported: CURRENT_FORMAT_VERSION,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedFormatVersion {
    pub found: u32,
    pub supported: u32,
}

impl std::fmt::Display for UnsupportedFormatVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "results use format version {}, but this mperf supports only version {}; re-record with a matching mperf",
            self.found, self.supported
        )
    }
}

impl std::error::Error for UnsupportedFormatVersion {}

impl Scenario {
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::Snapshot => "Snapshot",
            Scenario::Mem => "Memory",
            Scenario::Roofline => "Roofline",
            Scenario::TMA => "Top-Down",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_info_json(format_version: Option<u32>) -> String {
        let version = format_version
            .map(|version| format!(r#""format_version":{version},"#))
            .unwrap_or_default();
        format!(
            r#"{{{version}"scenario":"Snapshot","command":null,"cpu_model":"test","cpu_vendor":"test","scenario_info":{{"Snapshot":{{"pid":1,"counters":[]}}}}}}"#
        )
    }

    #[test]
    fn accepts_only_the_current_format() {
        let info: RecordInfo =
            serde_json::from_str(&record_info_json(Some(CURRENT_FORMAT_VERSION))).unwrap();
        info.ensure_supported_format().unwrap();
        assert!(info.cpu_info.roofline_calibration.is_none());
        assert_eq!(info.cpu_clock_source, None);
        assert_eq!(info.logical_cpu_count, None);

        let legacy: RecordInfo = serde_json::from_str(&record_info_json(None)).unwrap();
        assert!(legacy.ensure_supported_format().is_err());
    }

    #[test]
    fn cpu_clock_source_has_stable_machine_readable_names() {
        assert_eq!(
            serde_json::to_string(&CpuClockSource::CounterDelta).unwrap(),
            "\"counter_delta\""
        );
        assert_eq!(
            serde_json::to_string(&CpuClockSource::SampledOccupancy).unwrap(),
            "\"sampled_occupancy\""
        );
    }

    #[test]
    fn legacy_roofline_method_does_not_assume_a_traffic_level() {
        let method: RooflineMethodInfo = serde_json::from_str(
            r#"{"selection":"auto","accounting":"qemu","performance":"native","quality":"test","reason":"test","warnings":[]}"#,
        )
        .unwrap();

        assert_eq!(method.traffic, "unknown");
    }

    #[test]
    fn rejects_newer_results_with_actionable_message() {
        let info: RecordInfo =
            serde_json::from_str(&record_info_json(Some(CURRENT_FORMAT_VERSION + 1))).unwrap();
        let error = info.ensure_supported_format().unwrap_err().to_string();

        assert!(error.contains("re-record"));
        assert!(error.contains(&(CURRENT_FORMAT_VERSION + 1).to_string()));
    }

    #[test]
    fn round_trips_roofline_calibration_in_cpu_info() {
        let calibration = RooflineCalibration {
            threads: 4,
            cpu_affinity: Some("0-3".to_string()),
            samples: 5,
            compute_kernel: "test-fma".to_string(),
            fp64_gflops: 100.0,
            fp64_gflops_samples: vec![100.0],
            memory_gbytes_per_second: 50.0,
            memory_gbytes_per_second_samples: vec![50.0],
            ridge_point_flops_per_byte: 2.0,
            memory_working_set_bytes: 1024,
            memory_levels: Vec::new(),
            single_thread: None,
        };
        let cpu_info = CpuInfo {
            memory_calibration: None,
            roofline_calibration: Some(Box::new(calibration)),
        };

        let json = serde_json::to_string(&cpu_info).unwrap();
        let decoded: CpuInfo = serde_json::from_str(&json).unwrap();
        let decoded = decoded.roofline_calibration.unwrap();

        assert_eq!(decoded.threads, 4);
        assert_eq!(decoded.cpu_affinity.as_deref(), Some("0-3"));
        assert_eq!(decoded.compute_kernel, "test-fma");
        assert_eq!(decoded.fp64_gflops_samples, vec![100.0]);
        assert_eq!(decoded.memory_gbytes_per_second_samples, vec![50.0]);
        assert_eq!(decoded.ridge_point_flops_per_byte, 2.0);
    }
}
