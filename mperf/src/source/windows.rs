//! Windows-specific capture fidelity policy.

use mperf_data::{CaptureFidelity, RejectedRung, Scenario, SnapshotCollectorStatus};

pub(super) fn is_host() -> bool {
    cfg!(target_os = "windows")
}

pub(super) fn resolve_fidelity(scenario: Scenario) -> CaptureFidelity {
    if scenario == Scenario::Snapshot {
        // The ETW session can be admitted or denied only when it starts. The
        // actual rung is selected from the collector status after recording.
        return CaptureFidelity {
            scenario: "snapshot".to_owned(),
            rung: "etw_pmu_or_cpu_time".to_owned(),
            rejected: Vec::new(),
        };
    }
    let (rung, reason) = match scenario {
        Scenario::Snapshot => unreachable!(),
        Scenario::Mem => (
            "dynamorio_binary_accounting",
            "Windows does not expose PEBS-style precise memory samples through libprof",
        ),
        Scenario::Roofline => (
            "dynamorio_binary_accounting",
            "Windows does not expose workload-scoped memory-controller bandwidth through libprof",
        ),
        Scenario::TMA => (
            "wpr_etw_sampled_functions",
            "Windows WPR measures coherent TMA intervals and estimates function shares from timer instruction-pointer samples",
        ),
    };
    CaptureFidelity {
        scenario: format!("{scenario:?}").to_lowercase(),
        rung: rung.to_owned(),
        rejected: vec![RejectedRung {
            rung: "hardware_detail".to_owned(),
            reason: reason.to_owned(),
        }],
    }
}

pub(super) fn finalize_fidelity(
    scenario: Scenario,
    mut fidelity: CaptureFidelity,
    collectors: &[SnapshotCollectorStatus],
) -> CaptureFidelity {
    if scenario != Scenario::Snapshot {
        return fidelity;
    }
    if let Some(hardware) = collectors
        .iter()
        .find(|status| status.name == "hardware_pmu")
    {
        if hardware.status == "available" {
            fidelity.rung = "windows_etw_pmu".to_owned();
        } else {
            fidelity.rung = "cpu_time_samples_only".to_owned();
            fidelity.rejected.push(RejectedRung {
                rung: "windows_etw_pmu".to_owned(),
                reason: hardware.message.clone(),
            });
        }
    }
    fidelity
}
