use std::collections::HashMap;

use lazy_static::lazy_static;
use pmu_data::{EventDesc, Metric, TmaScenario};

#[allow(dead_code)]
pub struct CPUFamily {
    pub name: String,
    pub vendor: String,
    pub id: String,
    pub max_counters: Option<usize>,
    pub leader_event: Option<String>,
    pub events: HashMap<String, EventDesc>,
    pub aliases: HashMap<String, String>,
    pub metrics: Vec<Metric>,
    pub scenarios: HashMap<String, TmaScenario>,
}

include!(concat!(env!("OUT_DIR"), "/events.rs"));

pub fn find_cpu_family(id: &str) -> Option<&CPUFamily> {
    CPU_FAMILIES.get(id)
}

/// Returns derived PMU metrics defined for the detected host family.
pub fn host_metrics() -> Vec<Metric> {
    find_cpu_family(get_host_cpu_family())
        .map(|family| family.metrics.clone())
        .unwrap_or_default()
}

pub fn host_tma_scenario() -> Option<TmaScenario> {
    find_cpu_family(get_host_cpu_family())
        .and_then(|family| family.scenarios.get("tma"))
        .cloned()
        .or_else(|| Some(architectural_tma_fallback()))
}

/// Conservative level-one fallback for CPUs without a vendor TMA definition.
/// It uses only architectural perf events and labels its estimates accordingly.
fn architectural_tma_fallback() -> TmaScenario {
    TmaScenario {
        name: "tma".to_owned(),
        events: vec![
            "cycles".to_owned(),
            "instructions".to_owned(),
            "stalled_cycles_frontend".to_owned(),
            "stalled_cycles_backend".to_owned(),
        ],
        groups: vec![
            pmu_data::TmaGroup {
                name: "retiring".to_owned(),
                events: vec!["cycles".to_owned(), "instructions".to_owned()],
            },
            pmu_data::TmaGroup {
                name: "fe_bound".to_owned(),
                events: vec![
                    "cycles".to_owned(),
                    "instructions".to_owned(),
                    "stalled_cycles_frontend".to_owned(),
                ],
            },
            pmu_data::TmaGroup {
                name: "be_bound".to_owned(),
                events: vec![
                    "cycles".to_owned(),
                    "instructions".to_owned(),
                    "stalled_cycles_backend".to_owned(),
                ],
            },
        ],
        precise_attribution: false,
        constants: vec![pmu_data::TmaConstant {
            name: "assumed_retire_width".to_owned(),
            value: 4,
        }],
        metrics: vec![
            pmu_data::TmaMetric {
                name: "retiring".to_owned(),
                desc: "Estimated retiring fraction (architectural fallback)".to_owned(),
                formula: "instructions / ($assumed_retire_width * cycles)".to_owned(),
                group: Some("retiring".to_owned()),
                cpus: None,
            },
            pmu_data::TmaMetric {
                name: "fe_bound".to_owned(),
                desc: "Estimated frontend-bound fraction (architectural fallback)".to_owned(),
                formula: "stalled_cycles_frontend / cycles".to_owned(),
                group: Some("fe_bound".to_owned()),
                cpus: None,
            },
            pmu_data::TmaMetric {
                name: "be_bound".to_owned(),
                desc: "Estimated backend-bound fraction (architectural fallback)".to_owned(),
                formula: "stalled_cycles_backend / cycles".to_owned(),
                group: Some("be_bound".to_owned()),
                cpus: None,
            },
        ],
        ui: None,
    }
}

/// Maximum number of events the host PMU can schedule in one coherent group.
pub fn host_max_counters() -> Option<usize> {
    find_cpu_family(get_host_cpu_family()).and_then(|family| family.max_counters)
}

#[cfg(target_arch = "x86_64")]
pub fn get_host_cpu_family() -> &'static str {
    const EAX_VENDOR_INFO: u32 = 0x1;

    let result = core::arch::x86_64::__cpuid(EAX_VENDOR_INFO);
    x86_family_from_signature(result.eax)
}

#[cfg(target_arch = "x86_64")]
fn x86_family_from_signature(eax: u32) -> &'static str {
    let model = (eax >> 4) & 0xf;
    let family = (eax >> 8) & 0xf;
    let extended_model = (eax >> 16) & 0xf;
    let extended_family = (eax >> 20) & 0xff;

    if family == 0xf && extended_family == 0x8 {
        // AMD Family 23 (17h)
        if extended_model == 0x0 || extended_model == 0x1 || extended_model == 0x2 {
            return pmu_data::AMDZEN1;
        } else if extended_model == 0x3
            || extended_model == 0x4
            || extended_model == 0x6
            || extended_model == 0x7
            || extended_model == 0x9
        {
            return pmu_data::AMDZEN2;
        }
    } else if family == 0xf && extended_family == 0xa {
        // AMD Family 25 (19h)
        if extended_model == 0x0
            || extended_model == 0x2
            || extended_model == 0x4
            || extended_model == 0x5
        {
            return pmu_data::AMDZEN3;
        } else if extended_model == 0x1 || extended_model == 0x6 || extended_model == 0x7 {
            return pmu_data::AMDZEN4;
        }
    } else if family == 0x6 && extended_family == 0 {
        // Recent Intel processors
        if (extended_model == 0x3 && model == 0xc)
            || (extended_model == 0x4 && (model == 0x5 || model == 0x6))
        {
            return pmu_data::INTEL_HASWELL;
        } else if (extended_model == 0x3 && model == 0xd) || (extended_model == 0x4 && model == 0x7)
        {
            return pmu_data::INTEL_BROADWELL;
        } else if model == 0xe && (extended_model == 0x5 || extended_model == 0x4) {
            return pmu_data::INTEL_SKYLAKE;
        } else if model == 0xe && (extended_model == 0x8 || extended_model == 0x9) {
            return pmu_data::INTEL_KABYLAKE;
        } else if extended_model == 0xa && model == 0x5 {
            return pmu_data::INTEL_COMETLAKE;
        } else if extended_model == 0x7 && model == 0xe {
            return pmu_data::INTEL_ICELAKE;
        } else if extended_model == 0x6 && (model == 0xc || model == 0xa) {
            return pmu_data::INTEL_ICX;
        } else if extended_model == 0x8 && (model == 0xc || model == 0xd) {
            return pmu_data::INTEL_TIGERLAKE;
        } else if extended_model == 0xa && model == 0x7 {
            return pmu_data::INTEL_ROCKETLAKE;
        } else if extended_model == 0x9 && (model == 0x7 || model == 0xa) {
            return pmu_data::INTEL_ALDERLAKE;
        } else if extended_model == 0xb && (model == 0x7 || model == 0xa) {
            return pmu_data::INTEL_RAPTORLAKE;
        }
    }

    "unknown"
}

#[cfg(all(test, target_arch = "x86_64"))]
mod x86_tests {
    use super::*;

    fn signature(family: u32, model: u32, extended_model: u32, extended_family: u32) -> u32 {
        (model << 4) | (family << 8) | (extended_model << 16) | (extended_family << 20)
    }

    #[test]
    fn maps_both_tiger_lake_models() {
        assert_eq!(
            x86_family_from_signature(signature(6, 0xc, 8, 0)),
            pmu_data::INTEL_TIGERLAKE
        );
        assert_eq!(
            x86_family_from_signature(signature(6, 0xd, 8, 0)),
            pmu_data::INTEL_TIGERLAKE
        );
    }

    #[test]
    fn tiger_lake_event_table_is_loaded() {
        let family = find_cpu_family(pmu_data::INTEL_TIGERLAKE).unwrap();
        assert_eq!(family.name, "Intel Tiger Lake");
        assert_eq!(family.events.len(), 231);
        assert_eq!(
            family.aliases.get("cycles").unwrap(),
            "CPU_CLK_UNHALTED.THREAD_P"
        );
        assert_eq!(family.events.get("L1D.REPLACEMENT").unwrap().code, 0x151);
        let ipc = family
            .metrics
            .iter()
            .find(|metric| metric.name == "IPC")
            .unwrap();
        assert_eq!(ipc.expression.0, "instructions / cycles");
        assert_eq!(
            ipc.expression
                .evaluate(&HashMap::from([
                    ("instructions".to_owned(), 2_000.0),
                    ("cycles".to_owned(), 1_000.0),
                ]))
                .unwrap(),
            2.0
        );
    }
}

/// Map a RISC-V `marchid` to a known CPU family id.
#[cfg(target_arch = "riscv64")]
fn riscv_family(marchid: &str) -> &'static str {
    match marchid.trim() {
        // FIXME: technically speaking this also includes E7 and S7
        "0x8000000000000007" => pmu_data::SIFIVE_U7,
        "0x8000000058000001" => pmu_data::SPACEMIT_X60,
        "0x8000000058000002" => pmu_data::SPACEMIT_X100,
        "0x8000000041000002" => pmu_data::SPACEMIT_A100,
        _ => "unknown",
    }
}

#[cfg(target_arch = "riscv64")]
pub fn get_host_cpu_family() -> &'static str {
    use proc_getter::cpuinfo::cpuinfo;

    // The two core types on a SpacemiT K3 use different event encodings, and
    // auto detection below picks the first recognized core (the X100 cluster).
    // A workload placed on the A100 cores — which needs a helper such as
    // k3_ai's `ai`, since Linux will not schedule there on its own — must say
    // so explicitly, because miniperf itself still runs on an X100 core:
    //   MINIPERF_CPU_FAMILY=a100 ai mperf stat -- ./workload
    if let Ok(forced) = std::env::var("MINIPERF_CPU_FAMILY") {
        match forced.as_str() {
            "a100" | "spacemit_a100" => return pmu_data::SPACEMIT_A100,
            "x100" | "spacemit_x100" => return pmu_data::SPACEMIT_X100,
            "x60" | "spacemit_x60" => return pmu_data::SPACEMIT_X60,
            "" => {}
            other => eprintln!("warning: ignoring unknown MINIPERF_CPU_FAMILY='{other}'"),
        }
    }

    let Ok(info) = cpuinfo() else {
        return "unknown";
    };

    // Heterogeneous SoCs expose more than one core type: SpacemiT K3 pairs
    // eight X100 application cores with eight A100 AI cores, each with its own
    // marchid. Scan every entry and return the first core we have event data
    // for, rather than trusting the first one listed.
    info.iter()
        .filter_map(|core| core.get("marchid"))
        .map(|marchid| riscv_family(marchid))
        .find(|family| *family != "unknown")
        .unwrap_or("unknown")
}

#[cfg(all(test, target_arch = "riscv64"))]
mod riscv_tests {
    use super::*;

    #[test]
    fn maps_known_riscv_cores() {
        assert_eq!(riscv_family("0x8000000058000002"), pmu_data::SPACEMIT_X100);
        assert_eq!(riscv_family("0x8000000058000001"), pmu_data::SPACEMIT_X60);
        assert_eq!(riscv_family("0x8000000000000007"), pmu_data::SIFIVE_U7);
        // The A100 AI cores on the same K3 die use a different event encoding.
        assert_eq!(riscv_family("0x8000000041000002"), pmu_data::SPACEMIT_A100);
    }

    #[test]
    fn a100_uses_its_own_encoding_not_x100s() {
        let a100 = find_cpu_family(pmu_data::SPACEMIT_A100).unwrap();
        let x100 = find_cpu_family(pmu_data::SPACEMIT_X100).unwrap();

        // Cycles and instructions agree across both cores...
        assert_eq!(a100.events["cycles"].code, x100.events["cycles"].code);
        assert_eq!(
            a100.events["instructions"].code,
            x100.events["instructions"].code
        );
        // ...but the stall counters are assigned the opposite way round: the
        // A100 matches the K3 device tree and the X100 does not.
        assert_eq!(a100.events["stall_frontend"].code, 0x3);
        assert_eq!(a100.events["stall_backend"].code, 0x4);
        assert_eq!(x100.events["stall_frontend"].code, 0x4);
        assert_eq!(x100.events["stall_backend"].code, 0x3);
        // Raw 0x2d is a load counter on A100 and speculative issue on X100.
        assert_eq!(a100.events["load_inst"].code, 0x2d);
        assert_eq!(x100.events["issued_inst"].code, 0x2d);
    }

    #[test]
    fn x100_event_table_is_loaded() {
        let family = find_cpu_family(pmu_data::SPACEMIT_X100).unwrap();
        assert_eq!(family.name, "SpacemiT K3 (X100)");
        // mhpmcounter3..16 are general purpose; cycles and instructions have
        // dedicated counters 17 and 18.
        assert_eq!(family.max_counters, Some(14));
        assert_eq!(family.events.get("cycles").unwrap().code, 0x1e);
        assert_eq!(family.events.get("instructions").unwrap().code, 0x1f);
        // The K3 device tree labels raw 0x03 as STALLED_CYCLES_FRONTEND and
        // 0x04 as STALLED_CYCLES_BACKEND. Measurement on the board shows the
        // opposite, so miniperf uses the verified assignment.
        assert_eq!(family.events.get("stall_frontend").unwrap().code, 0x4);
        assert_eq!(family.events.get("stall_backend").unwrap().code, 0x3);
        assert_eq!(
            family.aliases.get("stalled_cycles_frontend").unwrap(),
            "stall_frontend"
        );
    }

    #[test]
    fn x100_tma_fits_in_one_counter_group() {
        let family = find_cpu_family(pmu_data::SPACEMIT_X100).unwrap();
        let tma = family.scenarios.get("tma").unwrap();
        assert_eq!(tma.groups.len(), 1);
        // Twelve general-purpose counters plus the two dedicated ones, so the
        // whole scenario is measured without multiplexing.
        assert_eq!(tma.groups[0].events.len(), 14);
        for event in &tma.events {
            assert!(
                family.events.contains_key(event),
                "TMA event {event} missing from the x100 table"
            );
        }
        for metric in &tma.metrics {
            assert_eq!(metric.group.as_deref(), Some("tma"));
        }
    }
}

/// Map an AArch64 (implementer, part) MIDR pair to a known CPU family id.
///
/// `implementer` is MIDR_EL1[31:24] and `part` is MIDR_EL1[15:4]. Variant and
/// revision are intentionally ignored, matching the way Linux perf keys its
/// pmu-events map.
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", test)))]
fn aarch64_family(implementer: u32, part: u32) -> &'static str {
    // 0x41 == 'A', the Arm Limited implementer code.
    const ARM: u32 = 0x41;

    match (implementer, part) {
        (ARM, 0xd80) => pmu_data::ARM_CORTEX_A520,
        (ARM, 0xd81) => pmu_data::ARM_CORTEX_A720,
        _ => "unknown",
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub fn get_host_cpu_family() -> &'static str {
    use proc_getter::cpuinfo::cpuinfo;

    // Heterogeneous (big.LITTLE) hosts expose more than one core type. Auto
    // detection below picks the first recognized core (typically a big core),
    // but a user profiling the little cluster can force a specific family, e.g.
    //   MINIPERF_CPU_FAMILY=cortex_a520 taskset -c 1-4 mperf stat -- ...
    // The override drives both event selection and PMU-type resolution, so the
    // little cluster's PMU is used automatically.
    if let Ok(forced) = std::env::var("MINIPERF_CPU_FAMILY") {
        match forced.as_str() {
            "cortex_a520" => return pmu_data::ARM_CORTEX_A520,
            "cortex_a720" => return pmu_data::ARM_CORTEX_A720,
            "" => {}
            other => eprintln!("warning: ignoring unknown MINIPERF_CPU_FAMILY='{other}'"),
        }
    }

    let info = match cpuinfo() {
        Ok(info) => info,
        Err(_) => return "unknown",
    };

    let parse = |s: &str| -> Option<u32> {
        let s = s.trim();
        let s = s.strip_prefix("0x").unwrap_or(s);
        u32::from_str_radix(s, 16).ok()
    };

    // On big.LITTLE systems different cores expose different MIDRs, so scan
    // every processor entry and return the first one we recognize.
    for core in &info {
        let implementer = core.get("CPU implementer").and_then(|v| parse(v));
        let part = core.get("CPU part").and_then(|v| parse(v));

        if let (Some(implementer), Some(part)) = (implementer, part) {
            let family = aarch64_family(implementer, part);
            if family != "unknown" {
                return family;
            }
        }
    }

    "unknown"
}

#[cfg(all(target_arch = "aarch64", not(target_os = "linux")))]
pub fn get_host_cpu_family() -> &'static str {
    // MIDR detection currently relies on /proc/cpuinfo, which is Linux-only.
    "unknown"
}

/// Read `MIDR_EL1` for a specific logical CPU from sysfs and map it to a known
/// family. Returns `"unknown"` when the CPU is absent or unrecognized.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn midr_family(cpu: u32) -> &'static str {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/regs/identification/midr_el1");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return "unknown",
    };
    let raw = raw.trim();
    let raw = raw.strip_prefix("0x").unwrap_or(raw);
    let midr = match u64::from_str_radix(raw, 16) {
        Ok(midr) => midr,
        Err(_) => return "unknown",
    };

    let implementer = ((midr >> 24) & 0xff) as u32;
    let part = ((midr >> 4) & 0xfff) as u32;
    aarch64_family(implementer, part)
}

/// Parse the first CPU id out of a sysfs cpumask list such as `"0,5-11"`.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn first_cpu_in_mask(mask: &str) -> Option<u32> {
    let first = mask.trim().split(',').next()?;
    let start = first.split('-').next()?;
    start.trim().parse().ok()
}

/// On heterogeneous (big.LITTLE) AArch64 systems each cluster exposes its own
/// PMU with a dynamically allocated `perf_event` type id. The legacy
/// `PERF_TYPE_RAW`/`PERF_TYPE_HARDWARE` encodings bind to only one cluster, so
/// events silently fail to count when the task runs on the other cluster.
///
/// This resolves the `perf_event` PMU `type` that backs the detected host CPU
/// family by matching each PMU's CPU mask against the per-CPU MIDR. Returns
/// `None` when detection is not possible (unknown family, missing sysfs), in
/// which case callers should fall back to the legacy encoding.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub fn host_pmu_type() -> Option<u32> {
    let host_family = get_host_cpu_family();
    if host_family == "unknown" {
        return None;
    }

    let devices = std::fs::read_dir("/sys/bus/event_source/devices").ok()?;

    for entry in devices.flatten() {
        let path = entry.path();

        let cpus = match std::fs::read_to_string(path.join("cpus")) {
            Ok(cpus) => cpus,
            Err(_) => continue, // not a core PMU (no cpumask)
        };

        let Some(first_cpu) = first_cpu_in_mask(&cpus) else {
            continue;
        };

        if midr_family(first_cpu) != host_family {
            continue;
        }

        if let Ok(type_str) = std::fs::read_to_string(path.join("type")) {
            if let Ok(pmu_type) = type_str.trim().parse::<u32>() {
                return Some(pmu_type);
            }
        }
    }

    None
}

/// A hardware performance-monitoring unit backing one cluster of cores on a
/// (possibly heterogeneous) system, together with the CPU family it implements.
#[derive(Clone, Debug)]
pub struct CorePmu {
    /// Dynamic `perf_event` PMU `type` id read from sysfs.
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    pub pmu_type: u32,
    /// Known family id (e.g. `"cortex_a720"`), or `"unknown"` for a cluster we
    /// have no event data for.
    pub family_id: &'static str,
    /// sysfs cpumask string for display, e.g. `"0,5-11"`.
    pub cpus: String,
}

/// Enumerate the per-cluster core PMUs on the host.
///
/// On big.LITTLE systems this returns one entry per core type (e.g. one for the
/// Cortex-A720 cluster and one for the Cortex-A520 cluster), each with its own
/// dynamic PMU `type`. Callers open every requested counter against every PMU
/// so a migrating task is faithfully counted wherever it runs.
///
/// Returns an empty vector when the platform does not expose separate core PMUs
/// (non-AArch64, non-Linux, or missing sysfs), in which case the legacy
/// single-PMU code path applies.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub fn host_core_pmus() -> Vec<CorePmu> {
    let mut pmus = Vec::new();

    let Ok(devices) = std::fs::read_dir("/sys/bus/event_source/devices") else {
        return pmus;
    };

    for entry in devices.flatten() {
        let path = entry.path();

        // A core PMU exposes a `cpus` cpumask; uncore/other PMUs do not.
        let Ok(cpus) = std::fs::read_to_string(path.join("cpus")) else {
            continue;
        };
        let Some(first_cpu) = first_cpu_in_mask(&cpus) else {
            continue;
        };
        let Ok(type_str) = std::fs::read_to_string(path.join("type")) else {
            continue;
        };
        let Ok(pmu_type) = type_str.trim().parse::<u32>() else {
            continue;
        };

        pmus.push(CorePmu {
            pmu_type,
            family_id: midr_family(first_cpu),
            cpus: cpus.trim().to_string(),
        });
    }

    // Stable order by the cluster's first CPU id so output is deterministic
    // (big cluster, which owns cpu0 on this class of SoC, comes first).
    pmus.sort_by_key(|p| first_cpu_in_mask(&p.cpus).unwrap_or(u32::MAX));
    pmus
}

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
pub fn host_core_pmus() -> Vec<CorePmu> {
    Vec::new()
}

/// Return a `(vendor, model)` description of the host CPU for display and for
/// recording in profile metadata. On heterogeneous systems the model lists each
/// distinct core cluster, e.g. `"ARM Cortex-A720 + ARM Cortex-A520"`.
pub fn host_cpu_description() -> (String, String) {
    #[cfg(target_os = "macos")]
    {
        let model = macos_sysctl_string("machdep.cpu.brand_string")
            .or_else(|| macos_sysctl_string("hw.model"))
            .unwrap_or_else(|| "Unknown".to_string());
        ("Apple".to_string(), model)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let cores = host_core_pmus();

        if cores.len() > 1 {
            let mut names: Vec<String> = Vec::new();
            let mut vendor: Option<String> = None;

            for core in &cores {
                if let Some(family) = find_cpu_family(core.family_id) {
                    if !names.contains(&family.name) {
                        names.push(family.name.clone());
                        vendor.get_or_insert_with(|| family.vendor.clone());
                    }
                }
            }

            if !names.is_empty() {
                return (
                    vendor.unwrap_or_else(|| "Unknown".to_string()),
                    names.join(" + "),
                );
            }
        }

        match find_cpu_family(get_host_cpu_family()) {
            Some(family) => (family.vendor.clone(), family.name.clone()),
            None => ("Unknown".to_string(), "Unknown".to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_sysctl_string(name: &str) -> Option<String> {
    use std::ffi::CString;

    let name = CString::new(name).ok()?;
    let mut len = 0_usize;
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || len == 0
    {
        return None;
    }

    let mut value = vec![0_u8; len];
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    value.truncate(len);
    if value.last() == Some(&0) {
        value.pop();
    }
    String::from_utf8(value).ok()
}

#[cfg(all(test, target_arch = "aarch64"))]
mod aarch64_tests {
    use super::*;

    #[test]
    fn maps_known_arm_cores() {
        // MIDR part numbers for the cores shipped in this project's event data.
        assert_eq!(aarch64_family(0x41, 0xd80), pmu_data::ARM_CORTEX_A520);
        assert_eq!(aarch64_family(0x41, 0xd81), pmu_data::ARM_CORTEX_A720);
    }

    #[test]
    fn unknown_cores_are_unknown() {
        assert_eq!(aarch64_family(0x41, 0xd46), "unknown"); // Cortex-A510, no data
        assert_eq!(aarch64_family(0x00, 0xd81), "unknown"); // wrong implementer
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_first_cpu_in_cpumask() {
        assert_eq!(first_cpu_in_mask("0,5-11"), Some(0));
        assert_eq!(first_cpu_in_mask("1-4"), Some(1));
        assert_eq!(first_cpu_in_mask("7"), Some(7));
        assert_eq!(first_cpu_in_mask("3-3\n"), Some(3));
        assert_eq!(first_cpu_in_mask(""), None);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::host_cpu_description;

    #[test]
    fn reports_apple_cpu_description() {
        let (vendor, model) = host_cpu_description();
        assert_eq!(vendor, "Apple");
        assert_ne!(model, "Unknown");
    }
}
