use anyhow::Result;
use comfy_table::{Cell, Color, ContentArrangement, Table};
use libprof::{Capabilities, Mechanism, SamplingProbe, probe_sampling_group};
use mperf_data::Scenario;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    Blocker,
    Degraded,
    Info,
    Ok,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Degraded => "degraded",
            Severity::Info => "info",
            Severity::Ok => "ok",
        }
    }

    fn color(self) -> Color {
        match self {
            Severity::Blocker => Color::Red,
            Severity::Degraded => Color::Yellow,
            Severity::Info => Color::Blue,
            Severity::Ok => Color::Green,
        }
    }
}

#[derive(Clone, Debug)]
struct Check {
    feature: String,
    status: String,
    severity: Severity,
    action: String,
}

fn check(
    feature: &str,
    status: impl Into<String>,
    severity: Severity,
    action: impl Into<String>,
) -> Check {
    // Em dashes render two columns wide in many terminals but count as one in
    // the table layout, which skews every border line.
    Check {
        feature: feature.to_owned(),
        status: status.into().replace('\u{2014}', "-"),
        severity,
        action: action.into(),
    }
}

/// External programs the profiler shells out to at runtime.
#[derive(Clone, Copy, Debug, Default)]
struct Tooling {
    bpftrace: bool,
    objdump: bool,
    debuginfod_find: bool,
    debuginfod_requested: bool,
}

impl Tooling {
    fn probe() -> Self {
        Self {
            bpftrace: which::which("bpftrace").is_ok(),
            objdump: which::which("objdump").is_ok(),
            debuginfod_find: which::which("debuginfod-find").is_ok(),
            debuginfod_requested: std::env::var_os("DEBUGINFOD_URLS").is_some(),
        }
    }
}

fn sysctl(key: &str, value: &str) -> String {
    format!(
        "sudo sysctl -w {key}={value} (persist by adding `{key} = {value}` to /etc/sysctl.d/99-mperf.conf)"
    )
}

fn paranoid_check(caps: &Capabilities) -> Check {
    match caps.perf_event_paranoid {
        None => check(
            "perf_event_paranoid",
            "unreadable",
            Severity::Info,
            "no /proc/sys/kernel/perf_event_paranoid on this host",
        ),
        Some(level) if level > 2 => check(
            "perf_event_paranoid",
            format!("level {level}: perf_event_open is denied without root"),
            Severity::Blocker,
            sysctl("kernel.perf_event_paranoid", "1"),
        ),
        Some(level @ 2) => check(
            "perf_event_paranoid",
            format!("level {level}: no kernel samples, no system-wide (uncore) events"),
            Severity::Degraded,
            sysctl("kernel.perf_event_paranoid", "0"),
        ),
        Some(level @ 1) => check(
            "perf_event_paranoid",
            format!("level {level}: no kernel samples"),
            Severity::Degraded,
            sysctl("kernel.perf_event_paranoid", "0"),
        ),
        Some(level) => check(
            "perf_event_paranoid",
            format!("level {level}: unrestricted"),
            Severity::Ok,
            "-",
        ),
    }
}

/// The host's ceiling on samples per second, which every sampling group on a
/// CPU shares.
///
/// A scenario whose counters need more than one group splits that ceiling
/// between them, and asking for more than it allows makes the kernel throttle:
/// throttling stops a group while its running time keeps accruing, so the
/// counters come back orders of magnitude low rather than merely sparse. The
/// profiler lowers its own rate to stay under the ceiling, so this reports what
/// that costs rather than a failure.
fn sample_rate_ceiling_check(rate: Option<u64>) -> Check {
    let Some(rate) = rate else {
        return check(
            "sampling rate ceiling",
            "perf_event_max_sample_rate is unreadable",
            Severity::Info,
            "-",
        );
    };

    // What a two-group scenario (TMA on a small PMU) would be held to.
    let per_group = (rate * 4 / 5 / 2).max(1);
    if rate >= 10_000 {
        return check(
            "sampling rate ceiling",
            format!("{rate} Hz, enough for every scenario"),
            Severity::Ok,
            "-",
        );
    }
    check(
        "sampling rate ceiling",
        format!(
            "{rate} Hz shared between a scenario's groups, so a multi-group scenario samples at about {per_group} Hz"
        ),
        Severity::Degraded,
        "sudo sysctl -w kernel.perf_event_max_sample_rate=10000 (persist by adding `kernel.perf_event_max_sample_rate = 10000` to /etc/sysctl.d/99-mperf.conf)",
    )
}

/// Whether a scenario's sampling group is schedulable here, tested the way a
/// recording does it: build the group, sample a live child, count what comes
/// back.
///
/// `cpu-cycles opens` is not that test. A group the PMU cannot host is still
/// accepted by `perf_event_open` and then never scheduled, which produces a
/// recording with no `pmu_*` columns and no error, so this check has to run
/// the real thing and insist on real samples.
fn sampling_group_check(scenario: Scenario, probe: &Result<SamplingProbe, String>) -> Check {
    let label = match scenario {
        Scenario::Snapshot => "snapshot",
        Scenario::Mem => "mem",
        Scenario::Roofline => "roofline",
        Scenario::TMA => "tma",
    };
    let name = format!("sampling group ({label})");
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            return check(
                &name,
                format!("group could not be opened: {error}"),
                Severity::Blocker,
                "run `mperf doctor` with the workload's permissions, or report this host",
            );
        }
    };

    let opened = probe.hardware_opened().len();
    let dropped = probe
        .hardware_dropped()
        .iter()
        .map(|counter| counter.name().to_owned())
        .collect::<Vec<_>>();

    if probe.collapsed_to_software() {
        return check(
            &name,
            "no hardware counter survived; samples would carry software events only",
            Severity::Blocker,
            "recordings on this host have no pmu_* data; report the host and its event table",
        );
    }
    if probe.samples == 0 {
        return check(
            &name,
            format!("{opened} hardware counters opened but no samples arrived"),
            Severity::Blocker,
            "the group is accepted and never scheduled; report the host and its event table",
        );
    }
    if !dropped.is_empty() {
        return check(
            &name,
            format!(
                "{} samples, {opened} hardware counters; dropped {}",
                probe.samples,
                dropped.join(", ")
            ),
            Severity::Degraded,
            "these counters are missing from recordings on this host",
        );
    }
    check(
        &name,
        format!(
            "{} samples across {opened} hardware counters",
            probe.samples
        ),
        Severity::Ok,
        "-",
    )
}

fn hardware_counter_check(caps: &Capabilities) -> Check {
    if caps.hardware_counters {
        check("hardware counters", "cpu-cycles opens", Severity::Ok, "-")
    } else {
        check(
            "hardware counters",
            "cpu-cycles cannot be opened - no PMU access (virtualized host or paranoid level)",
            Severity::Blocker,
            "lower perf_event_paranoid, or run on hardware that exposes a PMU",
        )
    }
}

fn kernel_symbol_check(caps: &Capabilities) -> Check {
    if caps.kernel_symbols {
        check(
            "kernel symbols (kptr_restrict)",
            "kernel addresses are readable",
            Severity::Ok,
            "-",
        )
    } else {
        check(
            "kernel symbols (kptr_restrict)",
            format!(
                "kptr_restrict={} - kernel frames stay unsymbolized",
                caps.kptr_restrict
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
            ),
            Severity::Degraded,
            sysctl("kernel.kptr_restrict", "0"),
        )
    }
}

fn nmi_watchdog_check(caps: &Capabilities) -> Check {
    match caps.nmi_watchdog {
        Some(true) => check(
            "NMI watchdog",
            "enabled - holds one hardware counter, shrinking sampling groups",
            Severity::Degraded,
            sysctl("kernel.nmi_watchdog", "0"),
        ),
        Some(false) => check("NMI watchdog", "disabled", Severity::Ok, "-"),
        None => check("NMI watchdog", "unknown", Severity::Info, "-"),
    }
}

fn bpf_checks(caps: &Capabilities, tooling: &Tooling) -> Vec<Check> {
    let mut checks = vec![if tooling.bpftrace {
        check("bpftrace", "installed", Severity::Ok, "-")
    } else {
        check(
            "bpftrace",
            "not installed - snapshot loses scheduler, block-IO and TCP metrics",
            Severity::Blocker,
            "install bpftrace (pacman -S bpftrace / apt install bpftrace / dnf install bpftrace)",
        )
    }];

    checks.push(if !caps.kernel_btf {
        check(
            "eBPF collection (snapshot)",
            "kernel BTF missing at /sys/kernel/btf/vmlinux",
            Severity::Blocker,
            "boot a kernel built with CONFIG_DEBUG_INFO_BTF",
        )
    } else if caps.is_root {
        check(
            "eBPF collection (snapshot)",
            "running as root",
            Severity::Ok,
            "-",
        )
    } else {
        check(
            "eBPF collection (snapshot)",
            "not root - the BPF collector will be skipped",
            Severity::Blocker,
            "run snapshot under sudo: sudo mperf record -s snapshot -o OUT -- CMD",
        )
    });

    checks
}

fn tooling_checks(tooling: &Tooling) -> Vec<Check> {
    let mut checks = vec![if tooling.objdump {
        check("objdump (disassembly)", "installed", Severity::Ok, "-")
    } else {
        check(
            "objdump (disassembly)",
            "not installed - the assembly view in `mperf show` is unavailable",
            Severity::Degraded,
            "install binutils (pacman -S binutils / apt install binutils)",
        )
    }];

    if tooling.debuginfod_requested && !tooling.debuginfod_find {
        checks.push(check(
            "debuginfod-find",
            "DEBUGINFOD_URLS is set but debuginfod-find is missing",
            Severity::Degraded,
            "install debuginfod (pacman -S debuginfod / apt install debuginfod)",
        ));
    }

    checks
}

fn mechanism_feature(mechanism: Mechanism) -> &'static str {
    match mechanism {
        Mechanism::PebsMem => "precise sampling (Intel PEBS)",
        Mechanism::IbsOp => "precise sampling (AMD IBS)",
        Mechanism::ArmSpe => "precise sampling (Arm SPE)",
        Mechanism::FixedTopdown => "fixed topdown (PERF_METRICS)",
        Mechanism::ArmSlotsTopdown => "topdown (Arm pmuv3 slots)",
        Mechanism::LbrCallstack => "branch records (LBR call stacks)",
        Mechanism::UncoreBw => "uncore memory bandwidth",
        Mechanism::Baseline => "baseline counters",
    }
}

/// Mechanisms worth reporting on this host: no Arm SPE row on x86, no PEBS row
/// on AMD, nothing vendor-specific on architectures that cannot have it.
fn applicable_mechanisms(caps: &Capabilities) -> Vec<Mechanism> {
    let mut mechanisms = Vec::new();
    match caps.arch.as_str() {
        "x86_64" | "x86" => {
            if !caps.is_amd() {
                mechanisms.push(Mechanism::PebsMem);
                mechanisms.push(Mechanism::FixedTopdown);
            }
            if !caps.is_intel() {
                mechanisms.push(Mechanism::IbsOp);
            }
            mechanisms.push(Mechanism::LbrCallstack);
        }
        "aarch64" => {
            mechanisms.push(Mechanism::ArmSpe);
            mechanisms.push(Mechanism::ArmSlotsTopdown);
        }
        _ => {}
    }
    mechanisms.push(Mechanism::UncoreBw);
    mechanisms
}

fn mechanism_check(mechanism: Mechanism, caps: &Capabilities) -> Check {
    let feature = mechanism_feature(mechanism);
    let Some(reason) = mechanism.rejection(caps) else {
        return check(feature, "available", Severity::Ok, "-");
    };
    let status = reason
        .split_once(": ")
        .map_or(reason.as_str(), |(_, rest)| rest)
        .to_owned();

    let (severity, action) = match mechanism {
        Mechanism::IbsOp if !caps.has_cpu_flag("ibs") => (
            Severity::Degraded,
            "check BIOS for an IBS / 'Instruction Based Sampling' toggle",
        ),
        Mechanism::IbsOp => (
            Severity::Degraded,
            "kernel needs CONFIG_PERF_EVENTS_AMD_IBS to expose the ibs_op PMU",
        ),
        Mechanism::ArmSpe => (
            Severity::Degraded,
            "kernel needs CONFIG_ARM_SPE_PMU and firmware must expose SPE",
        ),
        Mechanism::UncoreBw if !caps.system_wide_allowed() => {
            return check(
                feature,
                status,
                Severity::Degraded,
                sysctl("kernel.perf_event_paranoid", "0"),
            );
        }
        Mechanism::UncoreBw => (Severity::Info, "no memory-controller PMU on this platform"),
        _ => (Severity::Info, "not available on this CPU"),
    };

    check(feature, status, severity, action)
}

/// What only a live run can tell us: the host's sampling ceiling, and what a
/// scenario's real group produced. Measured once at the boundary so the check
/// table stays a function of its inputs.
struct HostProbe {
    max_sample_rate: Option<u64>,
    groups: Vec<(Scenario, Result<SamplingProbe, String>)>,
}

impl HostProbe {
    fn measure() -> Self {
        HostProbe {
            max_sample_rate: std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
                .ok()
                .and_then(|value| value.trim().parse().ok()),
            groups: [Scenario::Snapshot, Scenario::TMA]
                .into_iter()
                .map(|scenario| {
                    let counters = crate::source::scenario_counters(scenario);
                    let probe =
                        probe_sampling_group(&counters, 200).map_err(|error| error.to_string());
                    (scenario, probe)
                })
                .collect(),
        }
    }
}

fn checks(caps: &Capabilities, tooling: &Tooling, host: &HostProbe) -> Vec<Check> {
    let mut checks = vec![
        paranoid_check(caps),
        hardware_counter_check(caps),
        sample_rate_ceiling_check(host.max_sample_rate),
    ];
    checks.extend(
        host.groups
            .iter()
            .map(|(scenario, probe)| sampling_group_check(*scenario, probe)),
    );
    checks.push(kernel_symbol_check(caps));
    checks.push(nmi_watchdog_check(caps));
    checks.extend(bpf_checks(caps, tooling));
    checks.extend(
        applicable_mechanisms(caps)
            .into_iter()
            .map(|mechanism| mechanism_check(mechanism, caps)),
    );
    checks.extend(tooling_checks(tooling));
    checks
}

fn render(checks: &[Check]) -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Feature", "Status", "Severity", "Action"]);
    for check in checks {
        table.add_row(vec![
            Cell::new(&check.feature),
            Cell::new(&check.status),
            Cell::new(check.severity.label()).fg(check.severity.color()),
            Cell::new(&check.action),
        ]);
    }
    table
}

/// Diagnose this host's profiling readiness. Exits with a nonzero status when
/// any check is a blocker.
pub fn do_doctor() -> Result<()> {
    let caps = libprof::capabilities();
    let (vendor, model) = libprof::host_cpu_description();
    let cpu = if model.starts_with(&vendor) {
        model
    } else {
        format!("{vendor} {model}")
    };
    let checks = checks(&caps, &Tooling::probe(), &HostProbe::measure());

    println!(
        "mperf doctor - {cpu} ({}), kernel {}\n",
        caps.arch,
        caps.kernel_version.as_deref().unwrap_or("unknown")
    );
    println!("{}", render(&checks));

    let blockers = checks
        .iter()
        .filter(|check| check.severity == Severity::Blocker)
        .count();
    if blockers > 0 {
        println!("\n{blockers} blocker(s) found");
        std::process::exit(1);
    }
    println!("\nno blockers found");
    Ok(())
}
