use std::collections::{BTreeMap, BTreeSet};

/// One PMU exposed under `/sys/bus/event_source/devices`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PmuDevice {
    /// Directory name, e.g. `cpu`, `ibs_op`, `armv8_pmuv3_0`, `uncore_imc_0`.
    pub name: String,
    /// `perf_event_attr.type` for this PMU, when readable.
    pub type_id: Option<u32>,
    /// Event aliases under `events/`.
    pub events: BTreeSet<String>,
    /// Config bitfield names under `format/`.
    pub formats: BTreeSet<String>,
    /// Values under `caps/`, e.g. `max_precise`, `branches`, `pmu_name`, `slots`.
    pub caps: BTreeMap<String, String>,
    /// The `cpus` cpumask a core PMU covers, e.g. `0,5-11`. Absent for PMUs
    /// that expose no cpumask.
    pub cpus: Option<String>,
}

impl PmuDevice {
    /// Whether this PMU advertises an event alias.
    pub fn has_event(&self, event: &str) -> bool {
        self.events.contains(event)
    }

    /// Whether any event alias starts with `prefix`.
    pub fn has_event_prefix(&self, prefix: &str) -> bool {
        self.events.iter().any(|event| event.starts_with(prefix))
    }

    /// A `caps/` value, when the kernel exposes it.
    pub fn cap(&self, name: &str) -> Option<&str> {
        self.caps.get(name).map(String::as_str)
    }

    /// A `caps/` value parsed as a number. Arm reports hexadecimal, Intel
    /// decimal.
    pub fn cap_number(&self, name: &str) -> Option<u64> {
        let value = self.cap(name)?.trim();
        match value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => value.parse().ok(),
        }
    }
}

/// Host facilities that influence which profiling features are available.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Linux `perf_event_paranoid`, when readable.
    pub perf_event_paranoid: Option<i32>,
    /// Whether the current process has `CAP_PERFMON` in its effective set.
    pub has_cap_perfmon: bool,
    /// Whether the current process runs as root.
    pub is_root: bool,
    /// Running kernel release, when readable.
    pub kernel_version: Option<String>,
    /// Whether a hardware CPU-cycles event can be opened for the current thread.
    pub hardware_counters: bool,
    /// Kernel limit for frequency-mode sampling.
    pub max_sample_rate: Option<u64>,
    /// Whether kernel symbol addresses are available to this process.
    pub kernel_symbols: bool,
    /// Linux `kptr_restrict`, when readable.
    pub kptr_restrict: Option<u32>,
    /// Whether the NMI watchdog holds a hardware counter (`kernel.nmi_watchdog`).
    pub nmi_watchdog: Option<bool>,
    /// Whether unprivileged processes may load BPF programs.
    pub unprivileged_bpf: bool,
    /// Whether the kernel exposes BTF, which bpftrace needs.
    pub kernel_btf: bool,
    /// Per-CPU perf mmap allowance in KiB (`perf_event_mlock_kb`).
    pub mmap_limit_kb: Option<u64>,
    /// Whether the kernel accepts perf's mmap data-watermark wakeup mode.
    pub mmap_data_watermark: bool,
    /// Every PMU exposed under `/sys/bus/event_source/devices`.
    pub pmus: Vec<PmuDevice>,
    /// x86 `/proc/cpuinfo` feature flags (empty on other architectures).
    pub cpu_flags: BTreeSet<String>,
    /// RISC-V ISA string from `/proc/cpuinfo`, when present.
    pub riscv_isa: Option<String>,
    /// x86 `vendor_id` from `/proc/cpuinfo`, when present.
    pub cpu_vendor: Option<String>,
    /// Host architecture, as in [`std::env::consts::ARCH`].
    pub arch: String,
}

impl Capabilities {
    /// A PMU by exact name.
    pub fn pmu(&self, name: &str) -> Option<&PmuDevice> {
        self.pmus.iter().find(|pmu| pmu.name == name)
    }

    /// Every PMU whose name starts with `prefix`.
    pub fn pmus_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a PmuDevice> {
        self.pmus
            .iter()
            .filter(move |pmu| pmu.name.starts_with(prefix))
    }

    /// Whether an x86 CPUID feature flag is present.
    pub fn has_cpu_flag(&self, flag: &str) -> bool {
        self.cpu_flags.contains(flag)
    }

    /// Highest `precise_ip` the core PMU accepts (Intel PEBS / AMD IBS-backed).
    pub fn max_precise(&self) -> Option<u64> {
        self.core_pmus()
            .filter_map(|pmu| pmu.cap_number("max_precise"))
            .max()
    }

    /// Core PMUs: `cpu`, hybrid `cpu_core`/`cpu_atom`, and Arm `armv8_pmuv3*`
    /// instances (more than one means a heterogeneous host).
    pub fn core_pmus(&self) -> impl Iterator<Item = &PmuDevice> {
        self.pmus.iter().filter(|pmu| {
            pmu.name == "cpu" || pmu.name.starts_with("cpu_") || pmu.name.starts_with("armv8_pmuv3")
        })
    }

    /// Whether system-wide (uncore) events can be opened by this process.
    pub fn system_wide_allowed(&self) -> bool {
        self.has_cap_perfmon || self.is_root || self.perf_event_paranoid.is_some_and(|p| p <= 0)
    }

    /// Whether the host CPU vendor is Intel.
    pub fn is_intel(&self) -> bool {
        self.cpu_vendor.as_deref() == Some("GenuineIntel")
    }

    /// Whether the host CPU vendor is AMD.
    pub fn is_amd(&self) -> bool {
        matches!(
            self.cpu_vendor.as_deref(),
            Some("AuthenticAMD" | "HygonGenuine")
        )
    }

    /// Whether the RISC-V ISA string advertises `sscofpmf` (sampling support).
    pub fn has_sscofpmf(&self) -> bool {
        self.riscv_isa
            .as_deref()
            .is_some_and(|isa| isa.contains("sscofpmf"))
    }
}

/// Probe profiling-related kernel capabilities without panicking.
pub fn capabilities() -> Capabilities {
    #[cfg(target_os = "linux")]
    {
        linux_capabilities()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Capabilities::default()
    }
}

#[cfg(target_os = "linux")]
fn linux_capabilities() -> Capabilities {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    capabilities_from_probe(ProbeResult {
        perf_event_paranoid: read_number("/proc/sys/kernel/perf_event_paranoid"),
        effective_capabilities: effective_capabilities(),
        hardware_counters: probe_hardware_counter(),
        max_sample_rate: read_number("/proc/sys/kernel/perf_event_max_sample_rate"),
        kptr_restrict: read_number("/proc/sys/kernel/kptr_restrict"),
        nmi_watchdog: read_number::<u32>("/proc/sys/kernel/nmi_watchdog").map(|value| value != 0),
        unprivileged_bpf_disabled: read_number("/proc/sys/kernel/unprivileged_bpf_disabled"),
        kernel_btf: std::path::Path::new("/sys/kernel/btf/vmlinux").is_file(),
        mmap_limit_kb: read_number("/proc/sys/kernel/perf_event_mlock_kb"),
        mmap_data_watermark: probe_mmap_data_watermark(),
        effective_uid: unsafe { libc::geteuid() },
        kernel_version: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|release| release.trim().to_owned()),
        pmus: discover_pmus("/sys/bus/event_source/devices"),
        cpu_flags: cpuinfo_set(&cpuinfo, "flags"),
        riscv_isa: cpuinfo_value(&cpuinfo, "isa"),
        cpu_vendor: cpuinfo_value(&cpuinfo, "vendor_id"),
    })
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ProbeResult {
    perf_event_paranoid: Option<i32>,
    effective_capabilities: Option<u64>,
    hardware_counters: bool,
    max_sample_rate: Option<u64>,
    kptr_restrict: Option<u32>,
    nmi_watchdog: Option<bool>,
    unprivileged_bpf_disabled: Option<u32>,
    kernel_btf: bool,
    mmap_limit_kb: Option<u64>,
    mmap_data_watermark: bool,
    effective_uid: u32,
    kernel_version: Option<String>,
    pmus: Vec<PmuDevice>,
    cpu_flags: BTreeSet<String>,
    riscv_isa: Option<String>,
    cpu_vendor: Option<String>,
}

#[cfg(target_os = "linux")]
fn capabilities_from_probe(probe: ProbeResult) -> Capabilities {
    let has_cap_perfmon = probe
        .effective_capabilities
        .is_some_and(|caps| caps & (1_u64 << 38) != 0);
    let is_root = probe.effective_uid == 0;
    let kernel_symbols = matches!(probe.kptr_restrict, Some(0))
        || (matches!(probe.kptr_restrict, Some(1)) && (has_cap_perfmon || is_root));

    Capabilities {
        perf_event_paranoid: probe.perf_event_paranoid,
        has_cap_perfmon,
        is_root,
        kernel_version: probe.kernel_version,
        hardware_counters: probe.hardware_counters,
        max_sample_rate: probe.max_sample_rate,
        kernel_symbols,
        kptr_restrict: probe.kptr_restrict,
        nmi_watchdog: probe.nmi_watchdog,
        unprivileged_bpf: probe.unprivileged_bpf_disabled.unwrap_or(0) == 0,
        kernel_btf: probe.kernel_btf,
        mmap_limit_kb: probe.mmap_limit_kb,
        mmap_data_watermark: probe.mmap_data_watermark,
        pmus: probe.pmus,
        cpu_flags: probe.cpu_flags,
        riscv_isa: probe.riscv_isa,
        cpu_vendor: probe.cpu_vendor,
        arch: std::env::consts::ARCH.to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn discover_pmus(root: &str) -> Vec<PmuDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut pmus: Vec<PmuDevice> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_str()?.to_owned();
            Some(PmuDevice {
                type_id: read_number(path.join("type").to_str()?),
                events: sysfs_names(&path.join("events")),
                formats: sysfs_names(&path.join("format")),
                caps: sysfs_values(&path.join("caps")),
                cpus: std::fs::read_to_string(path.join("cpus"))
                    .ok()
                    .map(|mask| mask.trim().to_owned()),
                name,
            })
        })
        .collect();
    pmus.sort_by(|a, b| a.name.cmp(&b.name));
    pmus
}

/// Entry names of a sysfs directory, dropping the `.scale`/`.unit` companions
/// of event aliases.
#[cfg(target_os = "linux")]
fn sysfs_names(dir: &std::path::Path) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| !name.contains('.'))
        .collect()
}

#[cfg(target_os = "linux")]
fn sysfs_values(dir: &std::path::Path) -> BTreeMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            let value = std::fs::read_to_string(entry.path()).ok()?;
            Some((name, value.trim().to_owned()))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn cpuinfo_value(cpuinfo: &str, key: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().to_owned())
    })
}

#[cfg(target_os = "linux")]
fn cpuinfo_set(cpuinfo: &str, key: &str) -> BTreeSet<String> {
    cpuinfo_value(cpuinfo, key)
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn read_number<T: std::str::FromStr>(path: &str) -> Option<T> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn effective_capabilities() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let raw = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))?
        .trim();
    u64::from_str_radix(raw, 16).ok()
}

#[cfg(target_os = "linux")]
fn probe_hardware_counter() -> bool {
    use perf_event_open_sys::{bindings, bindings::perf_event_attr};

    let mut attr = perf_event_attr::default();
    attr.size = std::mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = bindings::PERF_TYPE_HARDWARE;
    attr.config = bindings::PERF_COUNT_HW_CPU_CYCLES as u64;
    attr.set_disabled(1);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    let fd = unsafe { perf_event_open_sys::perf_event_open(&mut attr, 0, -1, -1, 0) };
    if fd < 0 {
        false
    } else {
        unsafe { libc::close(fd) };
        true
    }
}

#[cfg(target_os = "linux")]
fn probe_mmap_data_watermark() -> bool {
    use perf_event_open_sys::{bindings, bindings::perf_event_attr};

    // Opening a disabled software event makes the kernel validate the attr
    // without requiring a hardware PMU or creating an mmap. A successful open
    // proves that watermark-mode wakeups are usable by this process/kernel.
    let mut attr = perf_event_attr::default();
    attr.size = std::mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = bindings::PERF_TYPE_SOFTWARE;
    attr.config = bindings::PERF_COUNT_SW_DUMMY as u64;
    attr.set_disabled(1);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_watermark(1);
    attr.wakeup_watermark = 1;
    let fd = unsafe { perf_event_open_sys::perf_event_open(&mut attr, 0, -1, -1, 0) };
    if fd < 0 {
        false
    } else {
        unsafe { libc::close(fd) };
        true
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn probe_is_total() {
        let caps = capabilities();
        assert!(caps.max_sample_rate.is_some());
        assert!(caps.mmap_limit_kb.is_some());
    }

    #[test]
    fn discovers_core_pmu_from_sysfs() {
        let caps = capabilities();
        assert!(caps.kernel_version.is_some());
        let Some(cpu) = caps.core_pmus().next() else {
            return; // No core PMU in a VM or a restricted container.
        };
        assert!(cpu.type_id.is_some());
    }

    #[test]
    fn missing_sysfs_root_yields_no_pmus() {
        assert!(discover_pmus("/nonexistent/event_source/devices").is_empty());
    }

    #[test]
    fn parses_cpuinfo_fields() {
        let cpuinfo = "processor\t: 0\nflags\t\t: fpu ibs sse\nisa\t\t: rv64imafdc_sscofpmf\n";
        assert!(cpuinfo_set(cpuinfo, "flags").contains("ibs"));
        assert_eq!(
            cpuinfo_value(cpuinfo, "isa").as_deref(),
            Some("rv64imafdc_sscofpmf")
        );
        assert!(cpuinfo_set(cpuinfo, "absent").is_empty());
    }

    #[test]
    fn maps_mocked_probe_results() {
        let caps = capabilities_from_probe(ProbeResult {
            perf_event_paranoid: Some(4),
            effective_capabilities: Some(1_u64 << 38),
            hardware_counters: false,
            max_sample_rate: Some(12_345),
            kptr_restrict: Some(1),
            mmap_limit_kb: Some(516),
            mmap_data_watermark: true,
            effective_uid: 1000,
            ..ProbeResult::default()
        });

        assert_eq!(caps.perf_event_paranoid, Some(4));
        assert!(caps.has_cap_perfmon);
        assert!(!caps.hardware_counters);
        assert_eq!(caps.max_sample_rate, Some(12_345));
        assert!(caps.kernel_symbols);
        assert_eq!(caps.mmap_limit_kb, Some(516));
        assert!(caps.mmap_data_watermark);
        assert!(caps.system_wide_allowed());
    }

    #[test]
    fn mocked_probe_reports_unavailable_watermark_without_panicking() {
        let caps = capabilities_from_probe(ProbeResult {
            perf_event_paranoid: Some(4),
            hardware_counters: false,
            mmap_data_watermark: false,
            effective_uid: 1000,
            ..ProbeResult::default()
        });

        assert_eq!(caps.perf_event_paranoid, Some(4));
        assert!(!caps.hardware_counters);
        assert!(!caps.mmap_data_watermark);
        assert!(!caps.system_wide_allowed());
    }
}
