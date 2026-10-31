use std::path::Path;

use anyhow::{Context, Result};
use mperf_data::RecordInfo;

use crate::Session;

/// A recording with supported metadata and its query session.
pub struct Recording {
    info: RecordInfo,
    session: Session,
}

impl Recording {
    /// Read and validate metadata before opening the recording's tables.
    pub fn open(dir: &Path) -> Result<Self> {
        let info_path = dir.join("info.json");
        let data = std::fs::read_to_string(&info_path)
            .with_context(|| format!("failed to read {}", info_path.display()))?;
        let info: RecordInfo = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", info_path.display()))?;
        info.ensure_supported_format()?;
        let session =
            Session::open(dir).with_context(|| format!("failed to open {}", dir.display()))?;
        Ok(Self { info, session })
    }

    /// Consume the recording and return its validated metadata and session.
    pub fn into_parts(self) -> (RecordInfo, Session) {
        (self.info, self.session)
    }
}
