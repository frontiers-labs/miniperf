//! `cargo xtask check [all|lint|gui|NAME]` runs locally what CI runs.
//!
//! There is no logic here beyond finding the repository and forwarding to
//! `checks/run`: which checks apply to a host is decided by the checks
//! themselves, at the moment they run, not predicted in advance.

use anyhow::{Context, Result};
use std::{path::PathBuf, process::Command};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("check") {
        eprintln!(
            "usage: cargo xtask check [all|lint|gui|NAME] [--package DIR] [--recordings DIR]"
        );
        std::process::exit(2);
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repository root")
        .to_path_buf();

    let mut command = Command::new(root.join("checks/run"));
    command.current_dir(&root).args(&args[1..]);
    let status = command.status().context("running checks/run")?;
    std::process::exit(status.code().unwrap_or(1));
}
