//! Platform selection for ETW-specific TMA postprocessing.

use std::path::Path;

use anyhow::Result;
use mperf_data::TMAInfo;

use crate::postprocess::tables::Tables;

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod windows;

#[cfg(target_os = "windows")]
pub(super) fn process_if_windows(
    tables: &Tables,
    info: &TMAInfo,
    session_dir: &Path,
) -> Result<bool> {
    let file = session_dir.join("windows-tma-intervals.json");
    if !file.exists() {
        return Ok(false);
    }
    windows::process_windows(tables, info, &file)?;
    Ok(true)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn process_if_windows(
    _tables: &Tables,
    _info: &TMAInfo,
    _session_dir: &Path,
) -> Result<bool> {
    Ok(false)
}
