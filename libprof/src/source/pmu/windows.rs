//! Windows PMU sampling provenance and fallback status.

use crate::Counter;

pub(super) fn source_name() -> &'static str {
    "windows_etw"
}

pub(super) fn sampling_fallback(recorded: &[Counter]) -> Option<(&'static str, &'static str)> {
    (!recorded.iter().any(|counter| *counter != Counter::CpuClock)).then_some((
        "cpu_time_samples_only",
        "CPU-time samples were captured without hardware PMU counters; instruction attribution is approximate when ETW is unavailable",
    ))
}
