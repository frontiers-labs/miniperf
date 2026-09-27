//! PMU sampling provenance for non-Windows hosts.

use crate::Counter;

pub(super) fn source_name() -> &'static str {
    "perf_events"
}

pub(super) fn sampling_fallback(_recorded: &[Counter]) -> Option<(&'static str, &'static str)> {
    None
}
