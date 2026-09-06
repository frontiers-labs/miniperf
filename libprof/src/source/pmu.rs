use std::path::Path;

use anyhow::Result;

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::{
    capabilities, platform, resolve, Counter, Feature, SamplingDriver, SamplingDriverBuilder,
    SourceStatus,
};

/// Counter-based sampling. The counter list and rate come from the caller:
/// which events are worth sampling is a question about the analysis, not about
/// the hardware.
pub struct PmuSamplingSource {
    counters: Vec<Counter>,
    sample_freq: Option<u64>,
    stack_dump_size: Option<u32>,
    drivers: Vec<Box<dyn SamplingDriver>>,
    recorded: Vec<Counter>,
    sample_rate: Option<(u64, u64)>,
}

impl PmuSamplingSource {
    /// Samples `counters` at the driver's default rate.
    pub fn new(counters: Vec<Counter>) -> Self {
        PmuSamplingSource {
            counters,
            sample_freq: None,
            stack_dump_size: None,
            drivers: Vec::new(),
            recorded: Vec::new(),
            sample_rate: None,
        }
    }

    /// Overrides the interrupt frequency in hertz.
    pub fn sample_freq(mut self, hz: u64) -> Self {
        self.sample_freq = Some(hz);
        self
    }

    /// Captures this many user stack bytes per sample for post-hoc unwinding.
    pub fn stack_dump_size(mut self, bytes: u32) -> Self {
        self.stack_dump_size = Some(bytes);
        self
    }

    /// Counters the drivers actually opened, after capability fallbacks.
    pub fn recorded_counters(&self) -> &[Counter] {
        &self.recorded
    }

    /// The sampling frequency in use and the one requested, when the host
    /// ceiling forced them apart.
    pub fn lowered_sample_rate(&self) -> Option<(u64, u64)> {
        self.sample_rate
            .filter(|(effective, requested)| effective < requested)
    }
}

impl Source for PmuSamplingSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "pmu_sampling",
        }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        Availability::Available
    }

    fn start(&mut self, context: &SessionContext) -> Result<()> {
        // An attached target is a whole tree; a launched one is a single
        // suspended root whose descendants the kernel inherits sampling into.
        let target_pids = match context.attached_pid {
            Some(pid) => platform::process_tree(pid)
                .filter(|tree| !tree.is_empty())
                .map_or_else(|| vec![pid], |tree| tree.iter().map(|m| m.pid).collect()),
            None => vec![context.root_pid()],
        };
        // Call-stack LBR is a per-task feature; every group opened here is
        // attached to a target PID, so it is always valid when the hardware
        // has it.
        let lbr = resolve(Feature::HwCallstack, &capabilities()).is_satisfied();
        for target_pid in target_pids {
            let mut builder = SamplingDriverBuilder::new().counters(&self.counters);
            if let Some(freq) = self.sample_freq {
                builder = builder.sample_freq(freq);
            }
            if let Some(size) = self.stack_dump_size {
                builder = builder.stack_dump_size(size);
            }
            builder = builder.lbr_callstack(lbr);
            if let Some(process) = &context.process {
                builder = builder.process(process);
            } else {
                builder = builder.pid(target_pid as i32);
            }
            self.drivers.push(builder.build()?);
        }
        if let Some(driver) = self.drivers.first() {
            self.recorded = driver.counters();
            self.sample_rate = driver.sample_rate();
        }
        for driver in &mut self.drivers {
            driver.start(context.sink.clone())?;
        }
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        let mut message = String::new();
        let mut status = "available";
        let mut quality = "exact";
        for driver in &mut self.drivers {
            if let Err(error) = driver.stop() {
                status = "degraded";
                quality = "lossy";
                message = error.to_string();
            }
        }
        self.drivers.clear();
        if let (true, Some((effective, requested))) =
            (status == "available", self.lowered_sample_rate())
        {
            status = "degraded";
            quality = "best_effort";
            message = format!(
                "sampled at {effective} Hz instead of {requested} Hz: this host's \
                 perf_event_max_sample_rate is shared between the groups this scenario opens, \
                 and asking for more makes the kernel throttle the groups, which guts the \
                 counters rather than only thinning the samples"
            );
        }
        if status == "available" && crate::inherited_sampling_supported() == Some(false) {
            status = "degraded";
            quality = "best_effort";
            message = "this kernel rejects inherited sampling groups with PERF_SAMPLE_READ \
                       (Linux < 6.12); threads created after exec are not sampled"
                .to_string();
        }
        vec![SourceStatus::new(
            "pmu_sampling",
            status,
            "perf_events",
            quality,
            &message,
        )]
    }
}

/// Instruction-level memory access sampling: [`Feature::PreciseMem`], on
/// whichever mechanism this host provides it. Runs alongside counter-based
/// sampling and contributes the `mem_samples` family; where no mechanism
/// exists it probes unavailable and nothing else about the recording changes.
#[derive(Default)]
pub struct PreciseMemorySource {
    driver: Option<Box<dyn SamplingDriver>>,
    satisfied: Option<crate::Satisfied>,
}

impl Source for PreciseMemorySource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "precise_memory",
        }
    }

    /// Allocation-site attribution needs the in-process collector's stack
    /// capture around `malloc`/`free`; the unthrottled allocation stream alone
    /// carries no call stacks.
    fn child_environment(&self, directory: &Path) -> Vec<(String, String)> {
        super::InternalEventsSource {
            roofline_instrumented: false,
        }
        .child_environment(directory)
    }

    fn probe(&self, _directory: &Path) -> Availability {
        let resolution = resolve(Feature::PreciseMem, &capabilities());
        match resolution.satisfied {
            Some(_) => Availability::Available,
            None => Availability::Unavailable {
                reason: resolution
                    .rejected
                    .first()
                    .map(|(_, reason)| reason.clone())
                    .unwrap_or_else(|| "no precise memory sampling on this host".to_string()),
            },
        }
    }

    fn start(&mut self, context: &SessionContext) -> Result<()> {
        let resolution = resolve(Feature::PreciseMem, &capabilities());
        let Some(satisfied) = resolution.satisfied else {
            return Ok(());
        };
        let lbr = resolve(Feature::HwCallstack, &capabilities()).is_satisfied();
        let mut driver = crate::mem_sampling_driver(
            context.root_pid() as i32,
            crate::DEFAULT_SAMPLE_FREQUENCY_HZ,
            8 * 1024,
            lbr,
        )?;
        driver.start(context.sink.clone())?;
        self.driver = Some(driver);
        self.satisfied = Some(satisfied);
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        let Some(satisfied) = self.satisfied.take() else {
            // Probed available, then opened nothing: the recording has no
            // memory samples and must not be read as though it does.
            self.driver = None;
            return vec![SourceStatus::new(
                "precise_memory",
                "unavailable",
                "none",
                "unavailable",
                "no precise memory sampling facility was opened",
            )];
        };
        let mut status = "available";
        let mut message = String::new();
        if let Some(Err(error)) = self.driver.as_mut().map(|driver| driver.stop()) {
            status = "degraded";
            message = error.to_string();
        }
        self.driver = None;
        vec![SourceStatus::new(
            "precise_memory",
            status,
            satisfied.mechanism.name(),
            quality_name(satisfied.quality),
            &message,
        )]
    }
}

/// How a mechanism's provenance is written into the session.
fn quality_name(quality: crate::MeasurementQuality) -> &'static str {
    match quality {
        crate::MeasurementQuality::Exact => "precise",
        crate::MeasurementQuality::Scaled => "scaled",
        // A mechanism that samples every op and tags the memory ones answers a
        // different question than "sample these loads"; its rate is not a load
        // rate, and the session has to say so.
        crate::MeasurementQuality::Estimated => "best_effort",
    }
}
