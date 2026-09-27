//! Windows-specific host readiness checks.

use super::*;

pub(super) fn is_host() -> bool {
    cfg!(target_os = "windows")
}

pub(super) fn windows_checks() -> Vec<Check> {
    use std::path::PathBuf;

    let mut checks = Vec::new();
    let counters = crate::source::scenario_counters(Scenario::Snapshot);
    let probe = libprof::probe_sampling_group(&counters, 250).map_err(|error| error.to_string());
    match &probe {
        Ok(result) if result.samples > 0 && !result.hardware_opened().is_empty() => {
            checks.push(check(
                "ETW hardware profile / PMU",
                format!(
                    "{} samples from {} hardware counters",
                    result.samples,
                    result.hardware_opened().len()
                ),
                Severity::Ok,
                "-",
            ))
        }
        Ok(result) if !result.hardware_opened().is_empty() => checks.push(check(
            "ETW hardware profile / PMU",
            format!(
                "{} hardware counters opened, but no profile samples arrived",
                result.hardware_opened().len()
            ),
            Severity::Blocker,
            "run from an elevated terminal and check that the CPU exposes hardware profile sources",
        )),
        Ok(_) => checks.push(check(
            "ETW hardware profile / PMU",
            "Windows did not expose a usable hardware profile source",
            Severity::Blocker,
            "check BIOS/VM PMU exposure and run from an elevated terminal",
        )),
        Err(error) => checks.push(check(
            "ETW hardware profile / PMU",
            format!("profile probe failed: {error}"),
            Severity::Blocker,
            "run from an elevated terminal and check that the CPU exposes hardware profile sources",
        )),
    }

    checks.push(check(
        "Windows resource counters",
        "process, memory, disk, network and exposed thermal metrics use Windows APIs",
        Severity::Ok,
        "-",
    ));

    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from));
    let bundled_drrun = executable_dir.as_ref().and_then(|dir| {
        [
            dir.join("dynamorio/bin64/drrun.exe"),
            dir.join("../lib/miniperf/dynamorio/bin64/drrun.exe"),
        ]
        .into_iter()
        .find(|path| path.is_file())
    });
    let configured = std::env::var_os("MPERF_DYNAMORIO").map(PathBuf::from);
    let resolve_drrun = |path: &std::path::Path| {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
        if path.is_dir() {
            return ["bin64/drrun.exe", "dynamorio/bin64/drrun.exe"]
                .iter()
                .map(|relative| path.join(relative))
                .find(|candidate| candidate.is_file());
        }
        None
    };
    let drrun = configured
        .as_deref()
        .and_then(resolve_drrun)
        .or(bundled_drrun)
        .or_else(|| which::which("drrun.exe").ok());
    let client = std::env::var_os("MPERF_DR_CLIENT")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            executable_dir
                .as_ref()
                .map(|dir| dir.join("dr_roofline.dll"))
                .filter(|path| path.is_file())
        })
        .or_else(|| {
            executable_dir
                .as_ref()
                .map(|dir| dir.join("../lib/miniperf/dr_roofline.dll"))
                .filter(|path| path.is_file())
        });
    checks.push(match (drrun, client) {
        (Some(_), Some(_)) => check(
            "DynamoRIO accounting",
            "drrun.exe and dr_roofline.dll found",
            Severity::Ok,
            "-",
        ),
        (drrun, client) => check(
            "DynamoRIO accounting",
            format!(
                "{}{}",
                if drrun.is_none() {
                    "drrun.exe missing"
                } else {
                    "drrun.exe found"
                },
                if client.is_none() {
                    "; dr_roofline.dll missing"
                } else {
                    "; dr_roofline.dll found"
                }
            ),
            Severity::Degraded,
            "install/package DynamoRIO and the miniperf client, or set MPERF_DYNAMORIO and MPERF_DR_CLIENT",
        ),
    });

    let disassembler = ["llvm-objdump.exe", "objdump.exe"]
        .iter()
        .find_map(|name| which::which(name).ok());
    checks.push(if disassembler.is_some() {
        check(
            "disassembler",
            "objdump-compatible tool found",
            Severity::Ok,
            "-",
        )
    } else {
        check(
            "disassembler",
            "objdump-compatible tool not found; assembly view is unavailable",
            Severity::Degraded,
            "install LLVM or GNU binutils and add its bin directory to PATH",
        )
    });
    checks
}
