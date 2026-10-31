use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parquet::file::reader::{FileReader, SerializedFileReader};

/// Outcome of scanning one session directory for crash-damaged segments.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub healthy: usize,
    pub quarantined: Vec<PathBuf>,
}

/// Validate every Parquet segment in the session directory and quarantine
/// unreadable ones (a crash loses only the segment that was open, which has
/// no footer). Quarantined files get a `.corrupt` suffix so the session
/// opens cleanly; healthy segments are untouched.
pub fn recover_session(dir: &Path) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read session directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "parquet") {
            continue;
        }
        let readable = std::fs::File::open(&path)
            .map_err(anyhow::Error::from)
            .and_then(|file| Ok(SerializedFileReader::new(file)?))
            .and_then(|reader| {
                for group in 0..reader.metadata().num_row_groups() {
                    reader.get_row_group(group)?;
                }
                Ok(())
            });
        match readable {
            Ok(()) => report.healthy += 1,
            Err(_) => {
                let quarantine = path.with_extension("parquet.corrupt");
                std::fs::rename(&path, &quarantine)
                    .with_context(|| format!("failed to quarantine {}", path.display()))?;
                report.quarantined.push(quarantine);
            }
        }
    }
    Ok(report)
}
