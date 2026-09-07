//! Assembling libprof sources into the passes a scenario runs.
//!
//! Everything here is about *when and what* to measure: which counters a
//! scenario wants, which sources it needs versus merely likes, and what to
//! record about the ones that could not run. How to measure is libprof's.

use std::path::Path;

use anyhow::Result;
use libprof::{Availability, Counter, Feature, PmuSamplingSource, SessionContext, Source};
use mperf_data::{Scenario, SnapshotCollectorStatus};

/// Interrupt rate for the snapshot scenario, in hertz. A snapshot watches a
/// whole process tree for as long as the user leaves it running, so it trades
/// resolution for an overhead the user does not notice.
pub const SNAPSHOT_SAMPLE_FREQUENCY_HZ: u64 = 99;

/// One app execution with its own source set. A scenario is an ordered list
/// of passes; single-pass scenarios are the one-element case. Passes share
/// the session directory: per-process file names make merge concatenation
/// and hash-based identities give cross-pass correlation for free.
pub struct Pass {
    pub name: &'static str,
    pub required: Vec<Box<dyn Source>>,
    pub optional: Vec<Box<dyn Source>>,
}

/// Sources selected for one pass, plus statuses of everything declared but
/// not running.
pub struct ResolvedPass {
    pub name: &'static str,
    pub sources: Vec<Box<dyn Source>>,
    pub statuses: Vec<SnapshotCollectorStatus>,
}

impl Pass {
    /// Resolve declarations against probed availability: every required
    /// source must be available; optional sources degrade to a status entry.
    pub fn resolve(self, directory: &Path) -> Result<ResolvedPass> {
        let mut sources = Vec::new();
        let mut statuses = Vec::new();
        for source in self.required {
            match source.probe(directory) {
                Availability::Available => sources.push(source),
                Availability::Unavailable { reason } => anyhow::bail!(
                    "required source '{}' is unavailable: {reason}",
                    source.declare().name
                ),
            }
        }
        for source in self.optional {
            match source.probe(directory) {
                Availability::Available => sources.push(source),
                Availability::Unavailable { reason } => statuses.push(SnapshotCollectorStatus {
                    name: source.declare().name.to_string(),
                    status: "unavailable".to_string(),
                    source: "probe".to_string(),
                    quality: "missing".to_string(),
                    message: reason,
                }),
            }
        }
        Ok(ResolvedPass {
            name: self.name,
            sources,
            statuses,
        })
    }
}

impl ResolvedPass {
    pub fn child_environment(&self, directory: &Path) -> Vec<(String, String)> {
        self.sources
            .iter()
            .flat_map(|source| source.child_environment(directory))
            .collect()
    }

    pub fn start(&mut self, context: &SessionContext) -> Result<()> {
        for source in &mut self.sources {
            source.start(context).map_err(|error| {
                error.context(format!(
                    "failed to start source '{}' in pass '{}'",
                    source.declare().name,
                    self.name
                ))
            })?;
        }
        Ok(())
    }

    pub fn stop(&mut self, context: &SessionContext) -> Vec<SnapshotCollectorStatus> {
        let mut statuses = std::mem::take(&mut self.statuses);
        for source in &mut self.sources {
            statuses.extend(source.stop(context).into_iter().map(collector_status));
        }
        statuses
    }

    /// The sampling rate the host ceiling forced the pass down to, and the one
    /// it asked for. `None` when the pass sampled at the rate it wanted.
    pub fn lowered_sample_rate(&self) -> Option<(u64, u64)> {
        self.sources
            .iter()
            .find_map(|source| source.as_any().downcast_ref::<PmuSamplingSource>())
            .and_then(|source| source.lowered_sample_rate())
    }

    /// Counters the pass's PMU sampling source actually opened.
    pub fn recorded_counters(&self) -> Vec<(mperf_data::EventType, String)> {
        self.sources
            .iter()
            .find_map(|source| source.as_any().downcast_ref::<PmuSamplingSource>())
            .map(|source| {
                source
                    .recorded_counters()
                    .iter()
                    .map(|counter| {
                        (
                            crate::utils::counter_to_event_ty(counter),
                            counter.name().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn collector_status(status: libprof::SourceStatus) -> SnapshotCollectorStatus {
    SnapshotCollectorStatus {
        name: status.name,
        status: status.status,
        source: status.source,
        quality: status.quality,
        message: status.message,
    }
}

/// The hardware capability each scenario's headline analysis needs. Which
/// mechanism provides it is libprof's business, not the scenario's.
pub fn scenario_feature(scenario: Scenario) -> Feature {
    match scenario {
        Scenario::Snapshot => Feature::HwCallstack,
        Scenario::TMA => Feature::Topdown,
        Scenario::Mem => Feature::PreciseMem,
        Scenario::Roofline => Feature::DramBw,
    }
}

/// Whether the Roofline scenario gets measured memory-controller bandwidth on
/// this host, rather than the core-side baseline.
pub fn roofline_uncore_bandwidth() -> bool {
    libprof::resolve(Feature::DramBw, &libprof::capabilities()).is_satisfied()
}

/// Resolve a scenario's headline feature against the probed host.
pub fn resolve_fidelity(scenario: Scenario) -> mperf_data::CaptureFidelity {
    let resolution = libprof::resolve(scenario_feature(scenario), &libprof::capabilities());
    mperf_data::CaptureFidelity {
        scenario: format!("{scenario:?}").to_lowercase(),
        rung: resolution.mechanism().name().to_string(),
        rejected: resolution
            .rejected
            .into_iter()
            .map(|(mechanism, reason)| mperf_data::RejectedRung {
                rung: mechanism.name().to_string(),
                reason,
            })
            .collect(),
    }
}

/// The PMU sampling source a scenario records with.
pub fn pmu_sampling_source(scenario: Scenario) -> PmuSamplingSource {
    let source = PmuSamplingSource::new(scenario_counters(scenario));
    match scenario {
        Scenario::Snapshot => source
            .sample_freq(SNAPSHOT_SAMPLE_FREQUENCY_HZ)
            .stack_dump_size(2 * 1024),
        _ => source,
    }
}

pub fn scenario_counters(scenario: Scenario) -> Vec<Counter> {
    match scenario {
        Scenario::Snapshot | Scenario::Mem | Scenario::Roofline => vec![
            Counter::Cycles,
            Counter::Instructions,
            Counter::LLCReferences,
            Counter::LLCMisses,
            Counter::BranchMisses,
            Counter::BranchInstructions,
            Counter::StalledCyclesBackend,
            Counter::StalledCyclesFrontend,
            Counter::CpuClock,
            Counter::CpuMigrations,
            Counter::PageFaults,
            Counter::ContextSwitches,
        ],
        Scenario::TMA => libprof::tma_scenario()
            .expect("TMA counter selection requires a supported host CPU")
            .events
            .iter()
            .map(|event| libprof::tma_counter(event))
            .chain([
                Counter::CpuClock,
                Counter::CpuMigrations,
                Counter::PageFaults,
                Counter::ContextSwitches,
            ])
            .collect(),
    }
}
