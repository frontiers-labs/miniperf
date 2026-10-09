use std::path::Path;

use anyhow::Result;
use mperf_data::ProcMapEntry;
use object::Object;
#[cfg(not(target_os = "windows"))]
use object::ObjectSegment;

use super::{ModuleMapping, Tables};

pub(super) fn load_modules(tables: &Tables) -> Result<Vec<ProcMapEntry>> {
    #[cfg(target_os = "windows")]
    if !tables.has_table("modules") {
        return Ok(Vec::new());
    }
    crate::utils::load_modules(tables.connection())
}

#[cfg(target_os = "windows")]
pub(super) fn missing_mapping(executable: &Path) -> Result<()> {
    eprintln!(
        "Warning: native Roofline samples have no executable mapping for '{}'; per-loop timing is unavailable",
        executable.display()
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub(super) fn missing_mapping(executable: &Path) -> Result<()> {
    anyhow::bail!(
        "native Roofline samples have no executable mapping for '{}'",
        executable.display()
    )
}

#[cfg(target_os = "windows")]
pub(super) fn executable_mappings(
    modules: &[ProcMapEntry],
    pid: u32,
    executable: &Path,
    object: &object::File<'_>,
) -> Vec<ModuleMapping> {
    // Toolhelp reports a PE module as one image-base mapping with file
    // offset zero. PE loop addresses use the preferred image base, so a
    // section file offset must not be subtracted from this mapping.
    modules
        .iter()
        .filter(|mapping| mapping.pid == pid)
        .filter(|mapping| paths_refer_to_same_file(Path::new(&mapping.filename), executable))
        .map(|mapping| ModuleMapping {
            runtime_start: mapping.address as u64,
            runtime_end: mapping.address.saturating_add(mapping.size) as u64,
            svma_start: object.relative_address_base(),
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
pub(super) fn executable_mappings(
    modules: &[ProcMapEntry],
    pid: u32,
    executable: &Path,
    object: &object::File<'_>,
) -> Vec<ModuleMapping> {
    modules
        .iter()
        .filter(|mapping| mapping.pid == pid)
        .filter(|mapping| paths_refer_to_same_file(Path::new(&mapping.filename), executable))
        .filter_map(|mapping| {
            let mapping_offset = mapping.offset as u64;
            let segment = object
                .segments()
                .filter(|segment| {
                    let (file_offset, file_size) = segment.file_range();
                    mapping_offset >= (file_offset & !0xfff)
                        && mapping_offset < file_offset.saturating_add(file_size)
                })
                .min_by_key(|segment| segment.file_range().0.abs_diff(mapping_offset))?;
            let (file_offset, _) = segment.file_range();
            Some(ModuleMapping {
                runtime_start: mapping.address as u64,
                runtime_end: mapping.address.saturating_add(mapping.size) as u64,
                svma_start: segment
                    .address()
                    .saturating_add(mapping_offset)
                    .saturating_sub(file_offset),
            })
        })
        .collect()
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    let left_text = left.to_string_lossy();
    let left = Path::new(
        left_text
            .strip_suffix(" (deleted)")
            .unwrap_or(left_text.as_ref()),
    );
    left == right
        || windows_case_insensitive_match(left, right)
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(target_os = "windows")]
fn windows_case_insensitive_match(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(target_os = "windows"))]
fn windows_case_insensitive_match(_left: &Path, _right: &Path) -> bool {
    false
}
