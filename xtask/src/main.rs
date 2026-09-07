//! Repository automation. `cargo xtask check` runs locally exactly what CI
//! runs; `cargo xtask matrix` emits the job matrices CI consumes.

mod registry;

use anyhow::{bail, Context, Result};
use registry::{checks_for, Needs, Os, Pmu, CHECKS, PACKAGE_TARGETS, RUNNERS};
use std::{path::PathBuf, process::Command};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") => check(&args[1..]),
        Some("matrix") => matrix(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 cargo xtask check <all|lint|NAME> [--package DIR] [--recordings DIR]\n  \
                 cargo xtask matrix <test|gui|package>"
            );
            std::process::exit(2);
        }
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repository root")
        .to_path_buf()
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|ix| args.get(ix + 1))
        .cloned()
}

fn check(args: &[String]) -> Result<()> {
    let root = repository_root();
    let selector = args.first().map(String::as_str).unwrap_or("all");
    let package = flag(args, "--package");
    let recordings = flag(args, "--recordings");
    let os = Os::host();

    // A host with counters is treated as Reliable so PMU checks actually run
    // locally; checks/run still consults the oracle and skips if it is wrong.
    let selected: Vec<&registry::Check> = match selector {
        "all" => CHECKS
            .iter()
            .filter(|check| check.os.contains(&os))
            .collect(),
        "lint" => checks_for(Needs::Source, os, Pmu::Absent),
        name => {
            let found = CHECKS.iter().find(|check| check.name == name);
            match found {
                Some(check) => vec![check],
                None => bail!(
                    "unknown check `{name}`; known: {}",
                    CHECKS
                        .iter()
                        .map(|check| check.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };

    // Ordered by what they read, so record checks run before anything that
    // consumes their output.
    let mut ordered = selected;
    ordered.sort_by_key(|check| match check.needs {
        Needs::Source => 0,
        Needs::Package => 1,
        Needs::Recordings => 2,
    });

    let mut failed = Vec::new();
    for check in ordered {
        let mut command = Command::new(root.join("checks/run"));
        command.current_dir(&root);
        if let Some(package) = &package {
            command.env("MPERF_PACKAGE", package);
        }
        if let Some(recordings) = &recordings {
            command.env("MPERF_RECORDINGS", recordings);
        }
        command.arg(check.name);
        let status = command
            .status()
            .with_context(|| format!("running checks/run {}", check.name))?;
        if !status.success() {
            failed.push(check.name);
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        bail!("failed: {}", failed.join(", "))
    }
}

fn matrix(args: &[String]) -> Result<()> {
    let which = args.first().map(String::as_str).unwrap_or("test");
    let entries = match which {
        "package" => PACKAGE_TARGETS
            .iter()
            .map(|(platform, target, runner)| {
                serde_json::json!({
                    "platform": platform,
                    "target": target,
                    "runner": runner,
                })
            })
            .collect::<Vec<_>>(),
        "test" | "gui" => {
            let needs = if which == "test" {
                Needs::Package
            } else {
                Needs::Recordings
            };
            RUNNERS
                .iter()
                .filter_map(|runner| {
                    let checks = checks_for(needs, runner.os, runner.pmu);
                    if checks.is_empty() {
                        return None;
                    }
                    let mut apt: Vec<&str> = checks
                        .iter()
                        .flat_map(|check| check.apt.iter().copied())
                        .collect();
                    apt.sort_unstable();
                    apt.dedup();
                    Some(serde_json::json!({
                        "label": runner.label,
                        "platform": runner.platform,
                        "os": runner.os.as_str(),
                        "pmu": match runner.pmu {
                            Pmu::Absent => "absent",
                            Pmu::Flaky => "flaky",
                            Pmu::Reliable => "reliable",
                        },
                        "reruns": runner.reruns,
                        "checks": checks.iter().map(|check| check.name).collect::<Vec<_>>(),
                        "records": checks.iter().any(|check| check.records),
                        "apt": apt,
                    }))
                })
                .collect()
        }
        other => bail!("unknown matrix `{other}`; use test, gui or package"),
    };
    println!("{}", serde_json::Value::Array(entries));
    Ok(())
}
