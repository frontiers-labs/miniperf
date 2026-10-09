//! BPF tracepoints are a Linux facility.

use std::path::Path;

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::SourceStatus;

/// Unavailable BPF source on Windows; keeps source selection portable.
#[derive(Default)]
pub struct BpfSource;

impl Source for BpfSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl { name: "bpf" }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        Availability::Unavailable {
            reason: "BPF tracepoints are unavailable on Windows".to_string(),
        }
    }

    fn start(&mut self, _context: &SessionContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        Vec::new()
    }
}
