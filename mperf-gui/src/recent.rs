use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const MAX_RECENT_RESULTS: usize = 12;
const STATE_DIRECTORY_OVERRIDE: &str = "MPERF_GUI_STATE_DIR";

pub fn load() -> Vec<PathBuf> {
    let Some(path) = state_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };

    let recent_results: Vec<PathBuf> = serde_json::from_str(&contents).unwrap_or_default();
    recent_results
        .into_iter()
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

pub fn remember(recent_results: &mut Vec<PathBuf>, result_directory: &Path) -> Result<()> {
    let result_directory =
        fs::canonicalize(result_directory).unwrap_or_else(|_| result_directory.to_path_buf());
    insert(recent_results, result_directory);

    let Some(path) = state_file() else {
        return Ok(());
    };
    let parent = path.parent().context("recent-results file has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temporary_path = path.with_extension("json.tmp");
    let contents =
        serde_json::to_vec_pretty(recent_results).context("failed to serialize recent results")?;
    fs::write(&temporary_path, contents)
        .with_context(|| format!("failed to write {}", temporary_path.display()))?;
    fs::rename(&temporary_path, &path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn insert(recent_results: &mut Vec<PathBuf>, result_directory: PathBuf) {
    recent_results.retain(|path| path != &result_directory);
    recent_results.insert(0, result_directory);
    recent_results.truncate(MAX_RECENT_RESULTS);
}

fn state_file() -> Option<PathBuf> {
    if let Some(path) = env::var_os(STATE_DIRECTORY_OVERRIDE) {
        return Some(PathBuf::from(path).join("recent-results.json"));
    }

    let home = env::var_os("HOME").map(PathBuf::from);
    let state_directory = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".local/state")))?
    };
    Some(state_directory.join("mperf-gui/recent-results.json"))
}
