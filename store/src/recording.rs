use std::path::{Path, PathBuf};

use mperf_data::{RecordInfo, UnsupportedFormatVersion};

use crate::Session;

/// A completed recording with supported metadata and its query session.
pub struct Recording {
    info: RecordInfo,
    session: Session,
}

/// Failure to read recording metadata or open its query session.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("failed to read {}", path.display())]
    ReadInfo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {}", path.display())]
    ParseInfo {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    UnsupportedFormat(#[from] UnsupportedFormatVersion),
    #[error("failed to open {}", path.display())]
    OpenSession {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
}

impl Recording {
    /// Read and validate metadata before opening the recording's tables.
    pub fn open(dir: &Path) -> Result<Self, RecordingError> {
        let info_path = dir.join("info.json");
        let data =
            std::fs::read_to_string(&info_path).map_err(|source| RecordingError::ReadInfo {
                path: info_path.clone(),
                source,
            })?;
        let info: RecordInfo =
            serde_json::from_str(&data).map_err(|source| RecordingError::ParseInfo {
                path: info_path,
                source,
            })?;
        info.ensure_supported_format()?;
        let session = Session::open(dir).map_err(|source| RecordingError::OpenSession {
            path: dir.to_owned(),
            source,
        })?;
        Ok(Self { info, session })
    }

    /// Consume the recording and return its validated metadata and session.
    pub fn into_parts(self) -> (RecordInfo, Session) {
        (self.info, self.session)
    }
}
