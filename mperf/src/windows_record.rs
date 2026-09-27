//! Windows-specific recording policy and metadata.

use mperf_data::{CpuClockSource, Scenario, SnapshotCollectorStatus};

/// Windows snapshot and TMA use ETW/WPR paths whose sampling policy is
/// supplied by the Windows collectors rather than the generic PMU probe.
pub(crate) fn needs_sampling_group_probe(scenario: Scenario) -> bool {
    matches!(scenario, Scenario::Snapshot | Scenario::TMA) && !cfg!(target_os = "windows")
}

/// Sampling-frequency and CPU clock metadata for a recording.
pub(crate) fn metadata(scenario: Scenario) -> (Option<u64>, CpuClockSource) {
    let frequency = if cfg!(target_os = "windows") {
        None
    } else if scenario == Scenario::Snapshot {
        Some(crate::source::SNAPSHOT_SAMPLE_FREQUENCY_HZ)
    } else {
        Some(libprof::DEFAULT_SAMPLE_FREQUENCY_HZ)
    };
    let clock = if cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )) {
        CpuClockSource::SampledOccupancy
    } else {
        CpuClockSource::CounterDelta
    };
    (frequency, clock)
}

/// Adds the Windows ETW hardware-PMU collector status to snapshot metadata.
pub(crate) fn add_snapshot_collector(
    collectors: &mut Vec<SnapshotCollectorStatus>,
    counters: &[(mperf_data::EventType, String)],
) {
    if !cfg!(target_os = "windows") {
        return;
    }
    let hardware_pmu = counters
        .iter()
        .any(|(_, name)| name != libprof::Counter::CpuClock.name());
    collectors.push(SnapshotCollectorStatus {
        name: "hardware_pmu".to_owned(),
        status: if hardware_pmu { "available" } else { "unavailable" }.to_owned(),
        source: "windows_etw".to_owned(),
        quality: if hardware_pmu { "sampled" } else { "cpu_time_only" }.to_owned(),
        message: if hardware_pmu {
            "Windows ETW captured hardware PMU overflow samples".to_owned()
        } else {
            "Hardware PMU overflow samples were unavailable; CPU-time instruction attribution may be approximate".to_owned()
        },
    });
}

pub(crate) fn snapshot_scope(tree: bool, attached: bool) -> &'static str {
    if cfg!(target_os = "windows") {
        "root_process_with_tree_discovery"
    } else {
        match (tree, attached) {
            (true, true) => "attached_tree_best_effort",
            (true, false) => "launched_tree_inherited",
            (false, _) => "legacy_root_only",
        }
    }
}

pub(crate) fn publish_initial_process_maps(attached: bool) -> bool {
    cfg!(any(target_os = "macos", target_os = "windows")) || attached
}

pub(crate) fn uses_windows_topdown() -> bool {
    cfg!(target_os = "windows")
}

pub(crate) fn topdown(
    dispatcher: std::sync::Arc<crate::event_dispatcher::EventDispatcher>,
    command: &[String],
    output_directory: &std::path::Path,
) -> anyhow::Result<(
    mperf_data::ScenarioInfo,
    Vec<mperf_data::SnapshotCollectorStatus>,
)> {
    #[cfg(target_os = "windows")]
    {
        crate::windows_tma::topdown(dispatcher, command, output_directory)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (dispatcher, command, output_directory);
        unreachable!("Windows TMA capture is selected only on Windows")
    }
}
