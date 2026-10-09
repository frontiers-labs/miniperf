//! Windows feature policy for mechanisms driven by Linux perf PMUs.

use super::Mechanism;

pub(super) fn rejection(mechanism: Mechanism) -> Option<&'static str> {
    match mechanism {
        Mechanism::PebsMem | Mechanism::IbsOp | Mechanism::ArmSpe => Some(
            "Windows ETW does not expose precise memory load/store samples through libprof",
        ),
        Mechanism::FixedTopdown | Mechanism::ArmSlotsTopdown => Some(
            "Windows WPR uses model-specific programmable TMA events instead of Linux fixed topdown/PERF_METRICS events",
        ),
        Mechanism::LbrCallstack => {
            Some("Windows ETW branch-record call stacks are unavailable in libprof")
        }
        Mechanism::UncoreBw => Some(
            "Windows does not expose workload-scoped memory-controller bandwidth through libprof",
        ),
        Mechanism::Baseline => None,
    }
}
