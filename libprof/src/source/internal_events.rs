use std::path::{Path, PathBuf};

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::SourceStatus;

/// The self-contained collector inside the profiled process: activated purely
/// through the child environment, it writes its own event tables into the
/// session directory. Nothing runs on this side.
pub struct InternalEventsSource {
    /// Whether the target was compiled with roofline loop instrumentation, in
    /// which case the collector reports loop boundaries as well.
    pub roofline_instrumented: bool,
}

impl Source for InternalEventsSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "internal_events",
        }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        Availability::Available
    }

    fn child_environment(&self, directory: &Path) -> Vec<(String, String)> {
        let directory = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_owned());
        let mut env = vec![(
            "MPERF_SESSION_DIR".to_string(),
            directory.to_string_lossy().into_owned(),
        )];
        if let Some(library) = collector_library_path() {
            env.push((
                "MPERF_COLLECTOR_LIBRARY".to_string(),
                library.to_string_lossy().into_owned(),
            ));
        }
        if self.roofline_instrumented {
            env.push((
                "MPERF_COLLECTOR_ROOFLINE_INSTRUMENTED".to_string(),
                "1".to_string(),
            ));
        }
        env
    }

    fn start(&mut self, _context: &SessionContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        Vec::new()
    }
}

/// The collector core shipped next to the running executable, when present.
fn collector_library_path() -> Option<PathBuf> {
    let library = if cfg!(target_os = "macos") {
        "libmperf_collector.dylib"
    } else {
        "libmperf_collector.so"
    };
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    // `../lib/miniperf` is the released package layout; without it a packaged
    // mperf found no collector and every shim silently recorded nothing.
    [
        path.join(library),
        path.join("../lib/miniperf").join(library),
        path.join("../lib").join(library),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}
