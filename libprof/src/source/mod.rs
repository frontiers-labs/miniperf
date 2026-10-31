//! Data producers: everything this host can be measured through.
//!
//! A source is probed, given the child environment it needs, started against a
//! [`SessionContext`], and stopped with a status describing what it actually
//! delivered. Sources know how to measure; deciding when and what to measure is
//! the caller's job.
//!
//! Every source compiles on every target. A host that cannot run one says so
//! from [`Source::probe`], so a scenario degrades at runtime instead of a build
//! failing on a platform nobody tested.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::{Process, ResourceSample, Sink, SourceStatus};

mod allocations;
mod bpf;
mod internal_events;
mod pmu;
mod procfs;
mod telemetry;

pub use allocations::AllocationSource;
pub use bpf::BpfSource;
pub use internal_events::InternalEventsSource;
pub use pmu::{PmuSamplingSource, PreciseMemorySource};
pub use procfs::ProcfsSource;
pub use telemetry::HostTelemetrySource;

/// What a source is, declared before probing.
pub struct SourceDecl {
    /// Stable identifier, also used as the status name.
    pub name: &'static str,
}

/// Result of probing whether a source can run here.
pub enum Availability {
    /// The host provides everything this source needs.
    Available,
    /// The source cannot run, with the reason to record and show.
    Unavailable {
        /// Why the source cannot run here.
        reason: String,
    },
}

/// Everything a source needs to know about the run it takes part in. The
/// target process is the launched (still suspended) root, or an attach PID.
pub struct SessionContext {
    /// Directory the session is being written into. Sources that hand a path
    /// to an external tool need it; sources that only produce records do not.
    pub directory: PathBuf,
    /// Where this source writes what it measures.
    pub sink: Arc<dyn Sink>,
    /// The launched target, still suspended, when the caller launched one.
    pub process: Option<Rc<Process>>,
    /// The target PID, when the caller attached to a running process.
    pub attached_pid: Option<u32>,
}

impl SessionContext {
    /// PID of the process at the root of the measured tree.
    pub fn root_pid(&self) -> u32 {
        self.attached_pid
            .unwrap_or_else(|| self.process.as_ref().expect("no target process").pid() as u32)
    }

    /// Whether the target was launched by this session rather than attached to.
    pub fn launched(&self) -> bool {
        self.process.is_some()
    }
}

/// A data producer contributing tables to a recording session.
pub trait Source {
    /// What this source is and what it writes.
    fn declare(&self) -> SourceDecl;

    /// Downcast hook, for callers that need a concrete source back after a run.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Whether this host can run the source, and why not when it cannot.
    fn probe(&self, directory: &Path) -> Availability;

    /// Environment the profiled child needs for this source, injected before
    /// the process is created.
    fn child_environment(&self, _directory: &Path) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Begins measuring.
    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()>;

    /// Stops measuring and reports what was delivered.
    fn stop(&mut self, context: &SessionContext) -> Vec<SourceStatus>;
}

/// One resource observation, with every field owned.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resource_sample(
    timestamp_ns: u64,
    resource: &str,
    resource_id: &str,
    category: &str,
    metric: &str,
    value: f64,
    unit: &str,
    scope: &str,
    source: &str,
    quality: &str,
) -> ResourceSample {
    ResourceSample {
        timestamp_ns,
        resource: resource.to_string(),
        resource_id: resource_id.to_string(),
        category: category.to_string(),
        metric: metric.to_string(),
        value,
        unit: unit.to_string(),
        scope: scope.to_string(),
        source: source.to_string(),
        quality: quality.to_string(),
    }
}
