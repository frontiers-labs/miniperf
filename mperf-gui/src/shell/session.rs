//! L1 session model for the Tracks shell: one loaded recording plus the
//! derived inventories (threads, modules, activity lanes, counter tracks)
//! every view scopes against.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use mperf_data::{RecordInfo, Scenario};

use crate::{
    memory::MemoryData,
    model::SummaryStats,
    profile::{ProfileData, TimeRange},
    profile_analysis::{CounterTracks, SampleFilter, instructions_metric},
    roofline::RooflineData,
    snapshot::SnapshotData,
    ui::ModuleKind,
};

use super::tma::TmaData;

pub const LANE_BUCKETS: usize = 480;
pub const TRACK_BINS: usize = 240;
pub const MAX_PINNED_TRACKS: usize = 3;

pub struct ThreadInfo {
    pub thread_id: u32,
    pub label: String,
    pub samples: u64,
}

pub struct ModuleInfo {
    pub label: String,
    pub kind: ModuleKind,
    pub frame_ids: BTreeSet<usize>,
    pub samples: u64,
}

pub struct ThreadLane {
    pub counts: Vec<u64>,
}

/// Unfiltered per-thread activity for the master timeline, aligned with
/// [`ShellSession::threads`]. Buckets span the full recording range.
pub struct ThreadLanes {
    pub buckets: usize,
    pub lanes: Vec<ThreadLane>,
    pub max_count: u64,
}

pub struct ShellSession {
    pub result_directory: PathBuf,
    pub name: String,
    pub scenario: Scenario,
    pub cpu_model: String,
    pub sampling_frequency_hz: Option<u64>,
    pub summary: SummaryStats,
    pub tma: Option<TmaData>,
    /// USE-method resource snapshot, when the recording carries one.
    pub snapshot: Option<SnapshotData>,
    /// Address-trace memory analysis, when the recording carries one.
    pub memory: Option<MemoryData>,
    /// Calibrated roofline with per-loop throughput, when available.
    pub roofline: Option<RooflineData>,
    pub profile: ProfileData,
    pub full_range: Option<TimeRange>,
    pub total_samples: u64,
    /// Whether any sample carries a call stack — gates every stack view.
    pub has_stacks: bool,
    /// Column of the per-sample instruction counter, when recorded.
    pub instructions_metric: Option<usize>,
    /// Whether the recording carries disassembly for source/asm tabs.
    pub has_assembly: bool,
    pub threads: Vec<ThreadInfo>,
    pub modules: Vec<ModuleInfo>,
    pub lanes: Option<ThreadLanes>,
    /// Full-run counter tracks pinned under the thread lanes.
    pub tracks: Option<CounterTracks>,
    pub pinned_tracks: Vec<usize>,
}

/// The one global filter that scopes every view. `modules` and `threads`
/// hold *deselected* inventory indices so a freshly loaded session is
/// unfiltered with empty sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobalFilter {
    pub time: Option<TimeRange>,
    pub disabled_threads: BTreeSet<u32>,
    pub disabled_modules: BTreeSet<usize>,
    pub symbol: String,
}

impl GlobalFilter {
    pub fn is_active(&self) -> bool {
        self.time.is_some()
            || !self.disabled_threads.is_empty()
            || !self.disabled_modules.is_empty()
            || !self.symbol.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The same filter without its time range, for views that always span the
    /// full run and paint the time selection instead of being scoped by it.
    pub fn spatial(&self) -> Self {
        Self {
            time: None,
            ..self.clone()
        }
    }

    /// Identity of everything except the time range, so those views recompute
    /// on thread/module/symbol changes but not while brushing.
    pub fn spatial_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.disabled_threads.hash(&mut hasher);
        self.disabled_modules.hash(&mut hasher);
        self.symbol.hash(&mut hasher);
        hasher.finish()
    }

    pub fn resolve(&self, session: &ShellSession) -> SampleFilter {
        let threads = (!self.disabled_threads.is_empty()).then(|| {
            session
                .threads
                .iter()
                .map(|thread| thread.thread_id)
                .filter(|thread_id| !self.disabled_threads.contains(thread_id))
                .collect()
        });
        let leaf_frames = (!self.disabled_modules.is_empty()).then(|| {
            session
                .modules
                .iter()
                .enumerate()
                .filter(|(index, _)| !self.disabled_modules.contains(index))
                .flat_map(|(_, module)| module.frame_ids.iter().copied())
                .collect()
        });
        let query = self.symbol.trim().to_lowercase();
        let frame_ids = (!query.is_empty()).then(|| {
            session
                .profile
                .frames
                .iter()
                .filter(|frame| frame.name.to_lowercase().contains(&query))
                .map(|frame| frame.id)
                .collect()
        });
        SampleFilter {
            range: self.time,
            threads,
            frame_ids,
            leaf_frames,
            ..SampleFilter::default()
        }
    }
}

impl ShellSession {
    pub fn load(result_directory: &Path) -> Result<Self> {
        let result_directory = fs::canonicalize(result_directory)
            .with_context(|| format!("failed to resolve {}", result_directory.display()))?;
        let info_path = result_directory.join("info.json");
        let info_data = fs::read_to_string(&info_path)
            .with_context(|| format!("failed to read {}", info_path.display()))?;
        let record_info: RecordInfo = serde_json::from_str(&info_data)
            .with_context(|| format!("failed to parse {}", info_path.display()))?;
        record_info.ensure_supported_format()?;

        let session = store::Session::open(&result_directory)
            .with_context(|| format!("failed to open {}", result_directory.display()))?;
        let connection = session.connection();
        let summary = SummaryStats::load(connection)?;
        let tma = TmaData::load(&record_info.scenario_info, connection);
        let mut profile = ProfileData::load(connection);
        profile.logical_cpu_count = record_info.logical_cpu_count;
        let has_assembly = super::asm::has_assembly(connection);
        let snapshot = SnapshotData::load(connection, record_info.logical_cpu_count);
        let memory = load_memory(connection, &record_info);
        let roofline = load_roofline(connection, &record_info);
        drop(session);

        let name = result_directory
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| result_directory.display().to_string());
        let command_name = record_info.command.as_ref().and_then(|command| {
            let binary = command.first()?;
            Path::new(binary)
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        });

        let full_range = profile.full_range();
        let total_samples = profile.samples.len() as u64;
        let has_stacks = profile
            .samples
            .iter()
            .any(|sample| !sample.stack.is_empty());
        let instructions_metric = instructions_metric(&profile.counter_metrics);
        let threads = thread_inventory(&profile, command_name.as_deref());
        let modules = module_inventory(&profile);
        let lanes = full_range.and_then(|range| thread_lanes(&profile, &threads, range));
        let tracks = CounterTracks::build(&profile, &SampleFilter::default(), TRACK_BINS);
        let pinned_tracks = tracks.as_ref().map(pin_tracks).unwrap_or_default();

        Ok(Self {
            result_directory,
            name,
            scenario: record_info.scenario,
            cpu_model: record_info.cpu_model,
            sampling_frequency_hz: record_info.sampling_frequency_hz,
            summary,
            tma,
            snapshot,
            memory,
            roofline,
            profile,
            full_range,
            total_samples,
            has_stacks,
            instructions_metric,
            has_assembly,
            threads,
            modules,
            lanes,
            tracks,
            pinned_tracks,
        })
    }

    pub fn scenario_label(&self) -> &'static str {
        match self.scenario {
            Scenario::Snapshot => "Snapshot",
            Scenario::Mem => "Memory",
            Scenario::Roofline => "Roofline",
            Scenario::TMA => "Top-Down",
        }
    }

    pub fn duration_seconds(&self) -> f64 {
        self.full_range
            .map(|range| range.end_ns.saturating_sub(range.start_ns) as f64 / 1e9)
            .unwrap_or(0.0)
    }

    pub fn module_label(&self, frame_id: usize) -> Option<(&str, ModuleKind)> {
        self.modules
            .iter()
            .find(|module| module.frame_ids.contains(&frame_id))
            .map(|module| (module.label.as_str(), module.kind))
    }
}

/// Memory analysis is kept only when the recording actually produced one;
/// availability is data-presence driven, never scenario-driven.
fn load_memory(
    connection: &crate::sql::Connection,
    record_info: &RecordInfo,
) -> Option<MemoryData> {
    let calibration = record_info
        .cpu_info
        .memory_calibration
        .as_deref()
        .map(|calibration| calibration.memory_levels.clone())
        .unwrap_or_default();
    let data = MemoryData::load(connection, calibration);
    let usable = data.summary.is_some() || !data.miss_ratio.is_empty() || !data.timeline.is_empty();
    (data.error.is_none() && usable).then_some(data)
}

fn load_roofline(
    connection: &crate::sql::Connection,
    record_info: &RecordInfo,
) -> Option<RooflineData> {
    let data = RooflineData::load(
        connection,
        record_info
            .cpu_info
            .roofline_calibration
            .as_deref()
            .cloned(),
        match &record_info.scenario_info {
            mperf_data::ScenarioInfo::Roofline(info) => info.method.as_deref().cloned(),
            _ => None,
        },
    );
    (!data.loops.is_empty()).then_some(data)
}

fn thread_inventory(profile: &ProfileData, command_name: Option<&str>) -> Vec<ThreadInfo> {
    let mut counts = BTreeMap::<u32, u64>::new();
    let mut process_ids = BTreeMap::<u32, u32>::new();
    for sample in &profile.samples {
        *counts.entry(sample.thread_id).or_default() += 1;
        process_ids.insert(sample.thread_id, sample.process_id);
    }
    let mut threads = counts
        .into_iter()
        .map(|(thread_id, samples)| {
            let is_main = process_ids.get(&thread_id) == Some(&thread_id);
            let label = match (is_main, command_name) {
                (true, Some(name)) => name.to_owned(),
                (true, None) => format!("main {thread_id}"),
                (false, _) => format!("tid {thread_id}"),
            };
            ThreadInfo {
                thread_id,
                label,
                samples,
            }
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| {
        let main =
            |thread: &ThreadInfo| process_ids.get(&thread.thread_id) == Some(&thread.thread_id);
        main(right)
            .cmp(&main(left))
            .then(right.samples.cmp(&left.samples))
            .then(left.thread_id.cmp(&right.thread_id))
    });
    threads
}

fn module_kind(module: &str) -> ModuleKind {
    let lower = module.to_lowercase();
    if lower.contains("kernel") || lower.contains("vmlinux") || lower.starts_with('[') {
        ModuleKind::Kernel
    } else if lower.contains("libc")
        || lower.contains("libm.")
        || lower.contains("libstdc++")
        || lower.contains("libgcc")
        || lower.contains("libdyld")
        || lower.contains("dyld")
        || lower.contains("libsystem")
        || lower.contains("ld-linux")
    {
        ModuleKind::Runtime
    } else if lower.contains(".so") || lower.contains(".dylib") || lower.starts_with("lib") {
        ModuleKind::Library
    } else {
        ModuleKind::User
    }
}

fn module_inventory(profile: &ProfileData) -> Vec<ModuleInfo> {
    let mut by_module = BTreeMap::<String, BTreeSet<usize>>::new();
    for frame in &profile.frames {
        let label = frame
            .module
            .as_deref()
            .map(|module| {
                Path::new(module)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(module)
                    .to_owned()
            })
            .unwrap_or_else(|| "unknown".to_owned());
        by_module.entry(label).or_default().insert(frame.id);
    }

    let mut leaf_counts = BTreeMap::<usize, u64>::new();
    for sample in &profile.samples {
        if let Some(frame_id) = sample.stack.last() {
            *leaf_counts.entry(*frame_id).or_default() += 1;
        }
    }

    let mut modules = by_module
        .into_iter()
        .map(|(label, frame_ids)| {
            let samples = frame_ids
                .iter()
                .filter_map(|frame_id| leaf_counts.get(frame_id))
                .sum();
            ModuleInfo {
                kind: module_kind(&label),
                label,
                frame_ids,
                samples,
            }
        })
        .collect::<Vec<_>>();
    modules.sort_by(|left, right| {
        right
            .samples
            .cmp(&left.samples)
            .then_with(|| left.label.cmp(&right.label))
    });
    modules
}

fn thread_lanes(
    profile: &ProfileData,
    threads: &[ThreadInfo],
    range: TimeRange,
) -> Option<ThreadLanes> {
    let duration = range.end_ns.saturating_sub(range.start_ns);
    if duration == 0 || threads.is_empty() {
        return None;
    }
    let buckets = LANE_BUCKETS.min(duration as usize).max(1);
    let bucket_width = duration.div_ceil(buckets as u64).max(1);
    let lane_index: BTreeMap<u32, usize> = threads
        .iter()
        .enumerate()
        .map(|(index, thread)| (thread.thread_id, index))
        .collect();
    let mut lanes: Vec<ThreadLane> = threads
        .iter()
        .map(|_| ThreadLane {
            counts: vec![0; buckets],
        })
        .collect();
    let mut max_count = 0u64;
    for sample in &profile.samples {
        let Some(lane) = lane_index.get(&sample.thread_id) else {
            continue;
        };
        let offset = sample.timestamp_ns.saturating_sub(range.start_ns);
        let bucket = ((offset / bucket_width) as usize).min(buckets - 1);
        let count = &mut lanes[*lane].counts[bucket];
        *count += 1;
        max_count = max_count.max(*count);
    }
    (max_count > 0).then_some(ThreadLanes {
        buckets,
        lanes,
        max_count,
    })
}

/// Picks up to [`MAX_PINNED_TRACKS`] tracks for the master timeline: the
/// derived IPC ratio first, then the most diagnostic raw counters.
fn pin_tracks(tracks: &CounterTracks) -> Vec<usize> {
    const PREFERRED: [&str; 5] = [
        "derived.ipc",
        "llc_misses",
        "stalled_cycles_backend",
        "branch_misses",
        "os_cpu_clock",
    ];
    let normalized = |key: &str| -> String {
        key.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    };
    let mut pinned = Vec::new();
    for preferred in PREFERRED {
        if pinned.len() >= MAX_PINNED_TRACKS {
            break;
        }
        if let Some(index) = tracks.tracks.iter().position(|track| {
            normalized(&track.key).contains(&normalized(preferred))
                && track.values.iter().any(Option::is_some)
        }) && !pinned.contains(&index)
        {
            pinned.push(index);
        }
    }
    for (index, track) in tracks.tracks.iter().enumerate() {
        if pinned.len() >= MAX_PINNED_TRACKS {
            break;
        }
        if !pinned.contains(&index) && track.values.iter().any(Option::is_some) {
            pinned.push(index);
        }
    }
    pinned
}

pub fn format_count(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

pub fn format_duration_seconds(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.2}s")
    } else {
        format!("{:.0}ms", seconds * 1e3)
    }
}
