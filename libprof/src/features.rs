//! What a caller asks for, and what this host can actually do about it.
//!
//! Callers request a [`Feature`] — an intent, like "precise memory samples".
//! libprof answers with the best [`Mechanism`] the host supports for it, or
//! with the reason every mechanism was rejected. PEBS, IBS and SPE are three
//! mechanisms for one feature, not three things a scenario has to know about;
//! adding a fourth is one variant and one resolver arm, invisible above.

use crate::{Capabilities, MeasurementQuality};

/// A capability a caller needs, independent of the hardware that provides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Instruction-level memory access samples: address, data source, latency.
    PreciseMem,
    /// Hardware top-down pipeline slot breakdown.
    Topdown,
    /// Call stacks from hardware branch records, without frame pointers.
    HwCallstack,
    /// Measured memory-controller bandwidth, rather than a core-side estimate.
    DramBw,
}

impl Feature {
    /// Stable identifier written to the session and shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Feature::PreciseMem => "precise_mem",
            Feature::Topdown => "topdown",
            Feature::HwCallstack => "hw_callstack",
            Feature::DramBw => "dram_bw",
        }
    }

    /// The mechanisms that can provide this feature, best first.
    pub fn mechanisms(self) -> &'static [Mechanism] {
        match self {
            Feature::PreciseMem => &[Mechanism::PebsMem, Mechanism::IbsOp, Mechanism::ArmSpe],
            Feature::Topdown => &[
                Mechanism::FixedTopdown,
                Mechanism::ArmSlotsTopdown,
                Mechanism::AmdDispatchSlots,
            ],
            Feature::HwCallstack => &[Mechanism::LbrCallstack],
            Feature::DramBw => &[Mechanism::UncoreBw],
        }
    }
}

/// A hardware facility that provides a [`Feature`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    /// Intel PEBS precise memory sampling (address, data source, latency).
    PebsMem,
    /// AMD IBS op sampling through the `ibs_op` PMU.
    IbsOp,
    /// Arm Statistical Profiling Extension through an `arm_spe_*` PMU.
    ArmSpe,
    /// Intel fixed topdown (`slots` + `topdown-*` PERF_METRICS).
    FixedTopdown,
    /// Arm pmuv3 slots-based L1 topdown.
    ArmSlotsTopdown,
    /// AMD Zen 4+ dispatch-slot events (`de_no_dispatch_per_slot`), counted on
    /// programmable counters.
    AmdDispatchSlots,
    /// Branch-record call stacks: Intel LBR call-stack mode, or AMD BRS / LbrV2
    /// history replayed into a stack, as a frame-pointer-free stack source.
    LbrCallstack,
    /// Memory-controller counters for measured DRAM bandwidth.
    UncoreBw,
    /// Plain counters and frame-pointer/DWARF stacks: what a host that
    /// provides none of the above still measures.
    Baseline,
}

impl Mechanism {
    /// Stable identifier written to the session and shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Mechanism::PebsMem => "pebs_mem",
            Mechanism::IbsOp => "ibs_op",
            Mechanism::ArmSpe => "arm_spe",
            Mechanism::FixedTopdown => "fixed_topdown",
            Mechanism::ArmSlotsTopdown => "arm_slots_topdown",
            Mechanism::AmdDispatchSlots => "amd_dispatch_slots",
            Mechanism::LbrCallstack => "lbr_callstack",
            Mechanism::UncoreBw => "uncore_bw",
            Mechanism::Baseline => "counter_only",
        }
    }

    /// How faithfully this mechanism delivers its feature.
    ///
    /// PEBS periods count the load events that were asked for; IBS and SPE
    /// sample every op and tag the memory ones, so their periods mean ops and
    /// their rate cannot be read as a load rate.
    pub fn quality(self) -> MeasurementQuality {
        match self {
            Mechanism::PebsMem
            | Mechanism::FixedTopdown
            | Mechanism::ArmSlotsTopdown
            | Mechanism::LbrCallstack
            | Mechanism::UncoreBw => MeasurementQuality::Exact,
            // The hardware counts slots directly, but the events outnumber
            // the counters and each group only sees its share of the run.
            Mechanism::AmdDispatchSlots => MeasurementQuality::Scaled,
            Mechanism::IbsOp | Mechanism::ArmSpe | Mechanism::Baseline => {
                MeasurementQuality::Estimated
            }
        }
    }

    /// Why this mechanism cannot run on this host, or `None` when it can.
    pub fn rejection(self, caps: &Capabilities) -> Option<String> {
        match self {
            Mechanism::PebsMem => {
                if caps.core_pmus().next().is_none() {
                    return Some("PEBS: no core PMU exposed in sysfs".to_string());
                }
                if !caps.max_precise().is_some_and(|precise| precise >= 2) {
                    return Some(format!(
                        "PEBS: core PMU advertises max_precise={} — precise sampling needs 2",
                        caps.max_precise().unwrap_or(0)
                    ));
                }
                let missing: Vec<&str> = ["mem-loads", "mem-stores"]
                    .into_iter()
                    .filter(|event| !caps.core_pmus().any(|pmu| pmu.has_event(event)))
                    .collect();
                if !missing.is_empty() {
                    return Some(format!(
                        "PEBS: core PMU exposes no {} event alias",
                        missing.join("/")
                    ));
                }
                // PERF_SAMPLE_WEIGHT_STRUCT, which carries the access latency,
                // landed in 5.12; without it a sample says where but not how bad.
                let release = caps.kernel_version.as_deref().unwrap_or_default();
                if kernel_at_least(release, 5, 12) == Some(false) {
                    return Some(format!(
                        "PEBS: kernel {release} predates 5.12 — PERF_SAMPLE_WEIGHT_STRUCT is unavailable"
                    ));
                }
                None
            }
            Mechanism::IbsOp => {
                let Some(pmu) = caps.pmu("ibs_op") else {
                    if cfg!(target_arch = "x86_64") && !caps.has_cpu_flag("ibs") {
                        return Some(
                            "IBS: `ibs` CPUID flag absent — possibly disabled in BIOS".to_string(),
                        );
                    }
                    return Some("IBS: no `ibs_op` PMU exposed by the kernel".to_string());
                };
                // IBS has no hardware privilege filter. The kernel's software
                // filter is what lets a recording sample user space only;
                // without it the open is refused.
                (!pmu.formats.contains("swfilt")).then(|| {
                    "IBS: `ibs_op` has no `swfilt` format — this kernel cannot restrict IBS samples to user space"
                        .to_string()
                })
            }
            Mechanism::ArmSpe => (caps.pmus_with_prefix("arm_spe").next().is_none()).then(|| {
                "Arm SPE: no `arm_spe_*` PMU exposed — needs CONFIG_ARM_SPE_PMU and firmware support"
                    .to_string()
            }),
            // A hybrid host needs one topdown group per core type, opened only
            // on that type's CPUs; until that exists, degrade rather than open
            // a P-core group on an E-core and fail the recording.
            Mechanism::FixedTopdown if caps.pmu("cpu_atom").is_some() => Some(
                "fixed topdown: hybrid Intel cores (cpu_core/cpu_atom) are not supported yet"
                    .to_string(),
            ),
            Mechanism::FixedTopdown => {
                let Some(pmu) = caps.core_pmus().find(|pmu| {
                    crate::topdown::INTEL_EVENTS
                        .iter()
                        .all(|event| pmu.has_event(&crate::sysfs_alias(event)))
                }) else {
                    return Some(
                        "fixed topdown: core PMU exposes no `slots` + `topdown-*` events (PERF_METRICS is Icelake and newer)"
                            .to_string(),
                    );
                };
                group_is_schedulable(pmu)
            }
            Mechanism::ArmSlotsTopdown => {
                let pmuv3: Vec<_> = caps.pmus_with_prefix("armv8_pmuv3").collect();
                if pmuv3.is_empty() {
                    return Some("Arm topdown: no `armv8_pmuv3*` PMU exposed".to_string());
                }
                // Every cluster must be measurable: a per-core-type breakdown
                // that silently omits one core type is worse than none.
                if pmuv3.iter().any(|pmu| pmu.cap_number("slots").is_none()) {
                    return Some(
                        "Arm topdown: pmuv3 advertises no `slots` capability".to_string(),
                    );
                }
                if pmuv3.iter().any(|pmu| {
                    !crate::topdown::ARM_EVENTS
                        .iter()
                        .all(|event| pmu.has_event(event))
                }) {
                    return Some(format!(
                        "Arm topdown: a pmuv3 instance is missing the architected slots events ({})",
                        crate::topdown::ARM_EVENTS.join("/")
                    ));
                }
                pmuv3.iter().find_map(|pmu| group_is_schedulable(pmu))
            }
            Mechanism::AmdDispatchSlots => {
                if !caps.hardware_counters {
                    return Some("AMD topdown: no hardware PMU exposed".to_string());
                }
                (!crate::cpu_family::host_has_event("de_no_dispatch_per_slot.backend_stalls"))
                    .then(|| {
                        "AMD topdown: dispatch-slot events (`de_no_dispatch_per_slot`) are Zen 4 and newer"
                            .to_string()
                    })
            }
            Mechanism::LbrCallstack => {
                let depth = caps
                    .core_pmus()
                    .filter_map(|pmu| pmu.cap_number("branches"))
                    .max();
                (!depth.is_some_and(|branches| branches > 0)).then(|| {
                    "LBR: core PMU advertises no branch-record depth in `caps/branches`".to_string()
                })
            }
            Mechanism::UncoreBw => {
                if !caps.system_wide_allowed() {
                    return Some(format!(
                        "uncore bandwidth: system-wide events need CAP_PERFMON or perf_event_paranoid <= 0 (currently {})",
                        caps.perf_event_paranoid
                            .map_or_else(|| "unknown".to_string(), |level| level.to_string())
                    ));
                }
                (!crate::platform_memory::bandwidth_counters_present(caps)).then(|| {
                    "uncore bandwidth: no memory-controller PMU exposed (uncore_imc*/amd_df/amd_umc/arm_cmn)"
                        .to_string()
                })
            }
            Mechanism::Baseline => None,
        }
    }
}

/// Reject a topdown mechanism whose group the PMU is too narrow to schedule.
/// The event set is fixed by the methodology, so a PMU that cannot hold it has
/// to degrade to the arithmetic baseline rather than fail the recording.
fn group_is_schedulable(pmu: &crate::PmuDevice) -> Option<String> {
    let events = crate::topdown::group_events(pmu);
    let events = events.iter().map(String::as_str).collect::<Vec<_>>();
    (!crate::topdown::group_opens(pmu, &events)).then(|| {
        format!(
            "topdown: {} cannot schedule the whole group ({}) in one counter set",
            pmu.name,
            events.join("/")
        )
    })
}

/// Whether a `uname -r` release is at least `major.minor`. `None` when the
/// string cannot be parsed, so an unreadable version never blocks a mechanism.
fn kernel_at_least(release: &str, major: u32, minor: u32) -> Option<bool> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let found_major = parts.next()?.parse::<u32>().ok()?;
    let found_minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    Some((found_major, found_minor) >= (major, minor))
}

/// The mechanism a feature runs on, with its provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Satisfied {
    /// Hardware facility that provides the feature here.
    pub mechanism: Mechanism,
    /// How faithful that facility's data is.
    pub quality: MeasurementQuality,
}

/// What libprof can do for one requested feature on this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The feature that was requested.
    pub feature: Feature,
    /// The mechanism chosen, or `None` when every mechanism was rejected.
    pub satisfied: Option<Satisfied>,
    /// Mechanisms rejected before the chosen one, best first.
    pub rejected: Vec<(Mechanism, String)>,
}

impl Resolution {
    /// Whether the host can provide the feature at all.
    pub fn is_satisfied(&self) -> bool {
        self.satisfied.is_some()
    }

    /// The mechanism in use, or [`Mechanism::Baseline`] when the feature was
    /// rejected and the caller falls back to plain counters.
    pub fn mechanism(&self) -> Mechanism {
        self.satisfied
            .map_or(Mechanism::Baseline, |satisfied| satisfied.mechanism)
    }
}

/// Resolve one feature against a probed host: the best mechanism it supports,
/// plus why every better one was rejected.
pub fn resolve(feature: Feature, caps: &Capabilities) -> Resolution {
    let mut rejected = Vec::new();
    for mechanism in feature.mechanisms() {
        match mechanism.rejection(caps) {
            None => {
                return Resolution {
                    feature,
                    satisfied: Some(Satisfied {
                        mechanism: *mechanism,
                        quality: mechanism.quality(),
                    }),
                    rejected,
                };
            }
            Some(reason) => rejected.push((*mechanism, reason)),
        }
    }
    Resolution {
        feature,
        satisfied: None,
        rejected,
    }
}
