//! Host clock-frequency and thermal telemetry.
//!
//! Every number a profile reports is conditioned on the clock the part
//! actually ran at, so this samples both signals next to a running workload at
//! about 1Hz. Sensors are discovered and opened once; a tick is a handful of
//! `pread`s. Nothing here is fatal: a sensor that cannot be opened is reported
//! through [`HostTelemetry::unavailable`], and one that disappears mid-run
//! simply drops out of later samples.

use std::io;

/// One tick of host clock and thermal state.
#[derive(Clone, Debug, Default)]
pub struct HostTelemetrySample {
    /// One entry per core cluster, in `clusters` order.
    pub clusters: Vec<ClusterClocks>,
    /// One entry per readable non-CPU clock domain.
    pub devices: Vec<DeviceClocks>,
    /// One entry per readable temperature sensor.
    pub zones: Vec<ThermalZone>,
    /// Cumulative hardware throttle events since boot, when the host counts them.
    pub throttle_events: Option<u64>,
    /// OS-reported thermal pressure, 0..=4 (macOS).
    pub pressure_level: Option<u8>,
}

/// Clocks observed across one core cluster at a single instant.
#[derive(Clone, Debug)]
pub struct ClusterClocks {
    /// Cluster id: `RecordInfo::cores` family id, or `"host"`.
    pub id: String,
    /// Mean current frequency over the cluster's readable CPUs.
    pub mean_hz: f64,
    /// Highest current frequency over the cluster's readable CPUs.
    pub peak_hz: f64,
    /// Cluster ceiling, when the host publishes one.
    pub max_hz: Option<f64>,
    /// `"cpufreq"` | `"pmu_derived"`.
    pub source: &'static str,
}

/// A non-CPU clock domain: GPU, NPU, DDR controller, interconnect.
#[derive(Clone, Debug)]
pub struct DeviceClocks {
    /// Domain id, numbered per resource: `gpu0`, `gpu1`, `npu0`.
    pub id: String,
    /// Owning resource: `"gpu"` | `"npu"` | `"memory"` | `"soc"`.
    pub resource: &'static str,
    /// Current frequency.
    pub cur_hz: f64,
    /// Domain ceiling, when the driver publishes one.
    pub max_hz: Option<f64>,
    /// Engine occupancy where the driver reports it (amdgpu `gpu_busy_percent`).
    pub busy_percent: Option<f64>,
    /// `"devfreq"` | `"drm_sysfs"` | `"hwmon"`.
    pub source: &'static str,
}

/// A single temperature sensor reading.
#[derive(Clone, Debug)]
pub struct ThermalZone {
    /// Sensor label, sanitized to lowercase snake_case: `package`, `soc`, `nvme0`.
    pub id: String,
    /// Owning resource: `"cpu"` | `"gpu"` | `"npu"` | `"memory"` | `"disk"` |
    /// `"network"` | `"thermal"`.
    pub resource: &'static str,
    /// Current temperature.
    pub celsius: f64,
    /// Trip point, when the host publishes one.
    pub critical_celsius: Option<f64>,
    /// `"hwmon"` | `"thermal_zone"`.
    pub source: &'static str,
}

#[cfg(target_os = "linux")]
mod imp {
    //! `cpufreq` for clocks, `hwmon` (or `thermal_zone` where a board exposes
    //! nothing else) for temperatures, and `thermal_throttle` for the throttle
    //! counter.

    use super::*;
    use std::{
        collections::{HashMap, HashSet},
        fs::File,
        os::unix::fs::FileExt,
        path::{Path, PathBuf},
        str::FromStr,
    };

    const CPU_ROOT: &str = "/sys/devices/system/cpu";
    const HWMON_ROOT: &str = "/sys/class/hwmon";
    const THERMAL_ROOT: &str = "/sys/class/thermal";
    const DEVFREQ_ROOT: &str = "/sys/class/devfreq";
    const DRM_ROOT: &str = "/sys/class/drm";

    /// A BMC-equipped board can publish 50+ sensors; keep the series count
    /// bounded and say so when the bound bites.
    const MAX_ZONES: usize = 64;
    const MAX_DEVICES: usize = 32;

    /// How far above its published ceiling a clock reading may sit before it
    /// is treated as sensor noise rather than a measurement.
    const CEILING_TOLERANCE: f64 = 1.1;

    /// Substring claims on a sensor name, in precedence order: the device
    /// keywords win over the CPU ones so `gpu_cluster_thermal` is a GPU. SoC
    /// vendors name sensors `<block>_thermal`, which no prefix can catch.
    const NAME_KEYWORDS: &[(&str, &str)] = &[
        ("gpu", "gpu"),
        ("mali", "gpu"),
        ("panfrost", "gpu"),
        ("adreno", "gpu"),
        ("pvr", "gpu"),
        ("npu", "npu"),
        ("vpu", "npu"),
        // No "nna": it collides with `antenna_thermal`, which several phone
        // SoCs publish. devfreq node names keep it, they are never antennas.
        ("tpu", "npu"),
        ("ddr", "memory"),
        ("dram", "memory"),
        ("cluster", "cpu"),
        ("cpu", "cpu"),
        ("core", "cpu"),
    ];

    /// hwmon chip name prefix to the resource that owns the sensor, for chips
    /// no keyword claims. Anything still unmatched falls back to `thermal`:
    /// hwmon is a generic sensor bus and an unattributable temperature is
    /// still worth reporting.
    const HWMON_RESOURCES: &[(&str, &str)] = &[
        ("coretemp", "cpu"),
        ("k10temp", "cpu"),
        ("zenpower", "cpu"),
        ("amdgpu", "gpu"),
        ("radeon", "gpu"),
        ("i915", "gpu"),
        ("xe", "gpu"),
        ("nouveau", "gpu"),
        ("panfrost", "gpu"),
        ("mali", "gpu"),
        ("spd5118", "memory"),
        ("jc42", "memory"),
        ("nvme", "disk"),
        ("drivetemp", "disk"),
        ("iwlwifi", "network"),
        ("mlx5", "network"),
        ("bnxt_en", "network"),
        ("ixgbe", "network"),
    ];

    /// devfreq node name fragment to the resource that owns the clock domain.
    const DEVFREQ_RESOURCES: &[(&str, &str)] = &[
        ("gpu", "gpu"),
        ("mali", "gpu"),
        ("panfrost", "gpu"),
        ("adreno", "gpu"),
        ("npu", "npu"),
        ("vpu", "npu"),
        ("nna", "npu"),
        ("ddr", "memory"),
        ("dmc", "memory"),
    ];

    /// Clock readings in preference order: the AMU-derived average ARM boards
    /// publish, then the hardware-measured counter, then — last, and labelled
    /// so it cannot be mistaken for a measurement — the governor's request.
    const FREQ_SOURCES: &[(&str, &str)] = &[
        ("cpuinfo_avg_freq", "cpufreq_avg"),
        ("cpuinfo_cur_freq", "cpufreq"),
        ("scaling_cur_freq", "cpufreq_requested"),
    ];

    /// hwmon parent-device subsystem to the resource that owns the sensor.
    /// `thermal`, `platform` and `pci` say nothing on their own and are left to
    /// the name-prefix map.
    const SUBSYSTEM_RESOURCES: &[(&str, &str)] = &[
        ("nvme", "disk"),
        ("block", "disk"),
        ("scsi", "disk"),
        ("net", "network"),
        ("mdio_bus", "network"),
        ("drm", "gpu"),
    ];

    /// Class directories a bridge device (`pci`, `platform`) hangs its real
    /// function off, checked when the subsystem itself is uninformative.
    const CHILD_CLASS_RESOURCES: &[(&str, &str)] = &[
        ("drm", "gpu"),
        ("nvme", "disk"),
        ("block", "disk"),
        ("net", "network"),
    ];

    /// Sysfs trees to discover from; parameterized so tests can point at fixtures.
    struct Roots<'a> {
        cpu: &'a Path,
        hwmon: &'a Path,
        thermal: &'a Path,
        devfreq: &'a Path,
        drm: &'a Path,
    }

    struct ClusterSensors {
        id: String,
        /// One tier per clock source, best first.
        tiers: Vec<FreqTier>,
        /// Worst tier still allowed to answer. A tier is latched the first
        /// time it reads, and only ever improves from there: a series that
        /// switched sensors mid-run would not be measuring one thing, and a
        /// single startup `EAGAIN` must not demote a cluster for the whole
        /// recording.
        active: usize,
        max_hz: Option<f64>,
    }

    struct FreqTier {
        /// One handle per policy domain in the cluster.
        files: Vec<File>,
        source: &'static str,
    }

    /// One cpufreq policy: the unit frequency actually scales on.
    struct Domain {
        name: String,
        dir: PathBuf,
        cpus: Vec<u32>,
    }

    struct ZoneSensors {
        id: String,
        resource: &'static str,
        input: File,
        critical_celsius: Option<f64>,
        source: &'static str,
    }

    struct DeviceSensors {
        id: String,
        resource: &'static str,
        current: File,
        /// Multiplier taking the raw reading to hertz: 1 for sysfs nodes that
        /// are already Hz, 1e6 for the DRM MHz nodes.
        scale: f64,
        max_hz: Option<f64>,
        busy: Option<File>,
        source: &'static str,
    }

    /// Open host clock and temperature sensors.
    pub struct HostTelemetry {
        clusters: Vec<ClusterSensors>,
        devices: Vec<DeviceSensors>,
        zones: Vec<ZoneSensors>,
        /// Per-CPU `core_throttle_count`. Package counters are deliberately not
        /// summed here: they are published once per CPU of a package and would
        /// be counted as many times as the package has CPUs.
        throttle: Vec<File>,
        unavailable: Vec<(&'static str, String)>,
        /// Clock readings dropped for exceeding their cluster ceiling.
        discarded: u64,
    }

    impl HostTelemetry {
        /// Discover sensors once. Clocks are grouped by cpufreq policy domain;
        /// `clusters` maps a CPU-family id to its logical CPUs and is used only
        /// to *label* those domains, since a family can span several domains
        /// with different ceilings. An empty slice labels everything `"host"`.
        /// `Ok(None)` when the host exposes neither clocks nor temperatures.
        pub fn start(clusters: &[(String, Vec<u32>)]) -> io::Result<Option<Self>> {
            Self::start_at(
                &Roots {
                    cpu: Path::new(CPU_ROOT),
                    hwmon: Path::new(HWMON_ROOT),
                    thermal: Path::new(THERMAL_ROOT),
                    devfreq: Path::new(DEVFREQ_ROOT),
                    drm: Path::new(DRM_ROOT),
                },
                clusters,
            )
        }

        /// Per-signal reasons for anything `start` could not open.
        pub fn unavailable(&self) -> &[(&'static str, String)] {
            &self.unavailable
        }

        /// How many clock readings have been dropped as impossible so far.
        pub fn discarded_readings(&self) -> u64 {
            self.discarded
        }

        /// Read every open sensor.
        pub fn sample(&mut self) -> io::Result<HostTelemetrySample> {
            let mut discarded = 0_u64;
            let clusters = self
                .clusters
                .iter_mut()
                .filter_map(|cluster| {
                    let (max_hz, active) = (cluster.max_hz, cluster.active);
                    let (tier, mean_hz, peak_hz) = cluster.tiers[..=active]
                        .iter()
                        .enumerate()
                        .find_map(|(index, tier)| {
                            let readings: Vec<f64> = tier
                                .files
                                .iter()
                                .filter_map(|file| read_number::<f64>(file).map(khz_to_hz))
                                .filter(|hz| {
                                    let keep = plausible(*hz, max_hz);
                                    discarded += u64::from(!keep);
                                    keep
                                })
                                .collect();
                            let (mean_hz, peak_hz) = aggregate(&readings)?;
                            Some((index, mean_hz, peak_hz))
                        })?;
                    cluster.active = tier;
                    Some(ClusterClocks {
                        id: cluster.id.clone(),
                        mean_hz,
                        peak_hz,
                        max_hz,
                        source: cluster.tiers[tier].source,
                    })
                })
                .collect();
            self.discarded += discarded;
            let devices = self
                .devices
                .iter()
                .filter_map(|device| {
                    Some(DeviceClocks {
                        id: device.id.clone(),
                        resource: device.resource,
                        cur_hz: read_number::<f64>(&device.current)? * device.scale,
                        max_hz: device.max_hz,
                        busy_percent: device.busy.as_ref().and_then(read_number::<f64>),
                        source: device.source,
                    })
                })
                .collect();
            let zones = self
                .zones
                .iter()
                .filter_map(|zone| {
                    Some(ThermalZone {
                        id: zone.id.clone(),
                        resource: zone.resource,
                        celsius: millicelsius_to_celsius(read_number::<f64>(&zone.input)?),
                        critical_celsius: zone.critical_celsius,
                        source: zone.source,
                    })
                })
                .collect();
            let throttle: Vec<u64> = self
                .throttle
                .iter()
                .filter_map(read_number::<u64>)
                .collect();
            Ok(HostTelemetrySample {
                clusters,
                devices,
                zones,
                throttle_events: (!throttle.is_empty()).then(|| throttle.iter().sum()),
                pressure_level: None,
            })
        }

        fn start_at(roots: &Roots, clusters: &[(String, Vec<u32>)]) -> io::Result<Option<Self>> {
            let cpu_root = roots.cpu;
            let mut unavailable = Vec::new();
            let hints: Vec<(String, Vec<u32>)> = if clusters.is_empty() {
                vec![("host".to_string(), online_cpus(cpu_root))]
            } else {
                clusters.to_vec()
            };

            let clusters = cluster_sensors(cpu_root, &hints);
            if clusters.is_empty() {
                unavailable.push((
                    "frequency",
                    format!("no readable cpufreq policy under {}", cpu_root.display()),
                ));
            }

            let (mut zones, hwmon_devices) = hwmon_sensors(roots.hwmon);
            if zones.is_empty() {
                zones = thermal_zone_zones(roots.thermal);
            }
            if zones.is_empty() {
                unavailable.push((
                    "temperature",
                    format!(
                        "no temperature sensor under {} or {}",
                        roots.hwmon.display(),
                        roots.thermal.display()
                    ),
                ));
            }
            if let Some(reason) = cap(&mut zones, MAX_ZONES, "temperature sensors") {
                unavailable.push(("temperature", reason));
            }

            let mut devices = devfreq_devices(roots.devfreq);
            devices.extend(drm_devices(roots.drm));
            devices.extend(hwmon_devices);
            let device_cap = cap(&mut devices, MAX_DEVICES, "device clock domains");
            assign_device_ids(&mut devices);
            if let Some(reason) = device_cap {
                unavailable.push(("device_clocks", reason));
            }

            let throttle: Vec<File> = online_cpus(cpu_root)
                .into_iter()
                .filter_map(|cpu| {
                    File::open(
                        cpu_root
                            .join(format!("cpu{cpu}"))
                            .join("thermal_throttle/core_throttle_count"),
                    )
                    .ok()
                })
                .collect();
            if throttle.is_empty() {
                unavailable.push((
                    "throttle_events",
                    "no thermal_throttle counters exposed by this host".to_string(),
                ));
            }

            if clusters.is_empty() && zones.is_empty() && devices.is_empty() {
                return Ok(None);
            }
            Ok(Some(Self {
                clusters,
                devices,
                zones,
                throttle,
                unavailable,
                discarded: 0,
            }))
        }
    }

    /// Group cpufreq policy domains into reported clusters. Frequency is a
    /// property of the clock domain, not of the CPU family: a board can run
    /// four ceilings across one family, and folding those together would show a
    /// core pegged at its own ceiling as permanently throttled. Domains that
    /// share both a family label and a ceiling are reported as one series, so a
    /// 128-core x86 host with one policy per CPU still yields a single cluster.
    fn cluster_sensors(cpu_root: &Path, hints: &[(String, Vec<u32>)]) -> Vec<ClusterSensors> {
        let mut groups: Vec<(Option<String>, Option<f64>, Vec<Domain>)> = Vec::new();
        for domain in cpu_domains(cpu_root) {
            let family = hint_family(hints, &domain.cpus);
            let max_hz =
                read_file_number::<f64>(&domain.dir.join("cpuinfo_max_freq")).map(khz_to_hz);
            match groups
                .iter_mut()
                .find(|(group, ceiling, _)| *group == family && *ceiling == max_hz)
            {
                Some((_, _, domains)) => domains.push(domain),
                None => groups.push((family, max_hz, vec![domain])),
            }
        }

        let mut used = HashSet::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        groups
            .iter()
            .filter_map(|(family, max_hz, domains)| {
                let base = match family {
                    Some(family) => {
                        let spans = groups
                            .iter()
                            .filter(|(other, ..)| other == &Some(family.clone()))
                            .count();
                        let index = seen.entry(family.clone()).or_default();
                        let base = if spans > 1 {
                            format!("{family}_{index}")
                        } else {
                            family.clone()
                        };
                        *index += 1;
                        base
                    }
                    None => domains[0].name.clone(),
                };
                let tiers: Vec<FreqTier> = FREQ_SOURCES
                    .iter()
                    .filter_map(|(node, source)| {
                        let files: Vec<File> = domains
                            .iter()
                            .filter_map(|domain| File::open(domain.dir.join(node)).ok())
                            .collect();
                        (!files.is_empty()).then_some(FreqTier { files, source })
                    })
                    .collect();
                (!tiers.is_empty()).then(|| ClusterSensors {
                    id: unique_id(base, &mut used),
                    active: tiers.len() - 1,
                    tiers,
                    max_hz: *max_hz,
                })
            })
            .collect()
    }

    /// Clock domains, preferring the policy tree; the per-CPU `cpufreq`
    /// directories are symlinks into it on kernels that expose both.
    fn cpu_domains(cpu_root: &Path) -> Vec<Domain> {
        let mut domains: Vec<Domain> = sorted_dir(&cpu_root.join("cpufreq"))
            .into_iter()
            .filter_map(|policy| {
                let name = policy.file_name()?.to_str()?.to_string();
                let cpus = parse_cpu_list(&read_trimmed(&policy.join("related_cpus"))?);
                (!cpus.is_empty()).then_some(Domain {
                    name,
                    dir: policy,
                    cpus,
                })
            })
            .collect();
        if !domains.is_empty() {
            domains.sort_by_key(|domain| domain.cpus.iter().copied().min());
            return domains;
        }
        let mut claimed = HashSet::new();
        for cpu in online_cpus(cpu_root) {
            if claimed.contains(&cpu) {
                continue;
            }
            let dir = cpu_root.join(format!("cpu{cpu}")).join("cpufreq");
            let cpus = read_trimmed(&dir.join("related_cpus"))
                .map(|list| parse_cpu_list(&list))
                .filter(|cpus| !cpus.is_empty())
                .unwrap_or_else(|| vec![cpu]);
            if !dir.is_dir() {
                continue;
            }
            claimed.extend(cpus.iter().copied());
            domains.push(Domain {
                name: format!("policy{cpu}"),
                dir,
                cpus,
            });
        }
        domains
    }

    /// The caller's cluster ids are a labelling hint: the family owning a
    /// domain's CPUs names it, and `policyN` covers domains no hint claims.
    fn hint_family(hints: &[(String, Vec<u32>)], cpus: &[u32]) -> Option<String> {
        hints
            .iter()
            .find(|(_, hinted)| cpus.iter().any(|cpu| hinted.contains(cpu)))
            .map(|(id, _)| id.clone())
    }

    fn parse_cpu_list(list: &str) -> Vec<u32> {
        list.split_whitespace()
            .filter_map(|cpu| cpu.parse().ok())
            .collect()
    }

    fn online_cpus(cpu_root: &Path) -> Vec<u32> {
        let mut cpus: Vec<u32> = std::fs::read_dir(cpu_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()?
                    .strip_prefix("cpu")?
                    .parse::<u32>()
                    .ok()
            })
            .collect();
        cpus.sort_unstable();
        cpus
    }

    /// hwmon carries every kind of sensor, so both temperatures and the clock
    /// domains some GPU drivers publish as `freqN_input` come off one walk.
    fn hwmon_sensors(root: &Path) -> (Vec<ZoneSensors>, Vec<DeviceSensors>) {
        let mut used = HashSet::new();
        let mut zones = Vec::new();
        let mut devices = Vec::new();
        for chip in sorted_dir(root) {
            let Some(name) = read_trimmed(&chip.join("name")) else {
                continue;
            };
            let name = sanitize(&name);
            let resource =
                hwmon_device_resource(&chip).unwrap_or_else(|| hwmon_resource_by_name(&name));
            let inputs = sorted_dir(&chip);
            let temp_channels = inputs
                .iter()
                .filter(|input| {
                    input
                        .file_name()
                        .and_then(|file| file.to_str())
                        .is_some_and(|file| file.starts_with("temp") && file.ends_with("_input"))
                })
                .count();
            for input in inputs {
                let Some(prefix) = input
                    .file_name()
                    .and_then(|file| file.to_str())
                    .and_then(|file| file.strip_suffix("_input"))
                    .map(str::to_string)
                else {
                    continue;
                };
                let Ok(file) = File::open(&input) else {
                    continue;
                };
                if prefix.starts_with("temp") {
                    let label =
                        read_trimmed(&chip.join(format!("{prefix}_label"))).unwrap_or_else(|| {
                            match temp_channels {
                                1 => name.clone(),
                                _ => format!("{name}_{}", prefix.trim_start_matches("temp")),
                            }
                        });
                    let critical_celsius =
                        read_file_number::<f64>(&chip.join(format!("{prefix}_crit")))
                            .or_else(|| {
                                read_file_number::<f64>(&chip.join(format!("{prefix}_max")))
                            })
                            .map(millicelsius_to_celsius);
                    zones.push(ZoneSensors {
                        id: unique_id(sanitize(&label), &mut used),
                        resource,
                        input: file,
                        critical_celsius,
                        source: "hwmon",
                    });
                } else if prefix.starts_with("freq") && resource == "gpu" {
                    devices.push(DeviceSensors {
                        id: String::new(),
                        resource,
                        current: file,
                        scale: 1.0,
                        max_hz: None,
                        busy: File::open(chip.join("device/gpu_busy_percent")).ok(),
                        source: "hwmon",
                    });
                }
            }
        }
        (zones, devices)
    }

    /// The clocks analogue of cpufreq for non-CPU blocks. Empty on x86;
    /// primary on SoCs, where GPU, NPU and DDR scaling all live here.
    fn devfreq_devices(root: &Path) -> Vec<DeviceSensors> {
        sorted_dir(root)
            .into_iter()
            .filter_map(|node| {
                let name = node.file_name()?.to_str()?.to_string();
                let driver = read_trimmed(&node.join("device/uevent")).unwrap_or_default();
                Some(DeviceSensors {
                    id: String::new(),
                    resource: devfreq_resource(&format!("{name} {driver}")),
                    current: File::open(node.join("cur_freq")).ok()?,
                    scale: 1.0,
                    max_hz: read_file_number::<f64>(&node.join("max_freq")),
                    busy: None,
                    source: "devfreq",
                })
            })
            .collect()
    }

    /// Intel GPU clocks: `i915` and `xe` publish the render clock in MHz on the
    /// card node itself. amdgpu's clock comes off hwmon instead.
    fn drm_devices(root: &Path) -> Vec<DeviceSensors> {
        sorted_dir(root)
            .into_iter()
            .filter(|card| {
                card.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("card"))
                    .is_some_and(|index| index.chars().all(|c| c.is_ascii_digit()))
            })
            .filter_map(|card| {
                let driver = std::fs::read_link(card.join("device/driver")).ok()?;
                let driver = driver.file_name()?.to_str()?.to_string();
                if driver != "i915" && driver != "xe" {
                    return None;
                }
                let current = File::open(card.join("gt_act_freq_mhz"))
                    .or_else(|_| File::open(card.join("gt_cur_freq_mhz")))
                    .ok()?;
                Some(DeviceSensors {
                    id: String::new(),
                    resource: "gpu",
                    current,
                    scale: 1e6,
                    max_hz: read_file_number::<f64>(&card.join("gt_RP0_freq_mhz")).map(mhz_to_hz),
                    busy: File::open(card.join("device/gpu_busy_percent")).ok(),
                    source: "drm_sysfs",
                })
            })
            .collect()
    }

    /// Ask the kernel what the sensor's parent device is before guessing from
    /// its name: chip names cannot keep up with the driver universe, but the
    /// device model already knows a `r8169` PHY is a network device.
    fn hwmon_device_resource(chip: &Path) -> Option<&'static str> {
        let device = std::fs::canonicalize(chip.join("device")).ok()?;
        let subsystem = std::fs::read_link(device.join("subsystem")).ok()?;
        let subsystem = subsystem.file_name()?.to_str()?.to_string();
        SUBSYSTEM_RESOURCES
            .iter()
            .find(|(class, _)| *class == subsystem)
            .or_else(|| {
                CHILD_CLASS_RESOURCES
                    .iter()
                    .find(|(class, _)| device.join(class).is_dir())
            })
            .map(|(_, resource)| *resource)
    }

    fn hwmon_resource_by_name(name: &str) -> &'static str {
        let name = name.to_ascii_lowercase();
        NAME_KEYWORDS
            .iter()
            .find(|(keyword, _)| name.contains(keyword))
            .or_else(|| {
                HWMON_RESOURCES
                    .iter()
                    .find(|(prefix, _)| name.starts_with(prefix))
            })
            .map_or("thermal", |(_, resource)| *resource)
    }

    fn devfreq_resource(node: &str) -> &'static str {
        let node = node.to_ascii_lowercase();
        DEVFREQ_RESOURCES
            .iter()
            .find(|(fragment, _)| node.contains(fragment))
            .map_or("soc", |(_, resource)| *resource)
    }

    /// Number domains per resource in discovery order: `gpu0`, `gpu1`, `npu0`.
    fn assign_device_ids(devices: &mut [DeviceSensors]) {
        let mut used = HashSet::new();
        let mut counts: HashMap<&'static str, usize> = HashMap::new();
        for device in devices {
            let index = counts.entry(device.resource).or_default();
            device.id = unique_id(format!("{}{index}", device.resource), &mut used);
            *index += 1;
        }
    }

    /// Truncate to `limit`, reporting how much was dropped. A truncated sensor
    /// list that reads as complete is worse than no list.
    fn cap<T>(items: &mut Vec<T>, limit: usize, what: &str) -> Option<String> {
        let dropped = items.len().checked_sub(limit).filter(|n| *n > 0)?;
        items.truncate(limit);
        Some(format!(
            "{} {what} discovered, capped at {limit}: {dropped} dropped",
            limit + dropped
        ))
    }

    fn thermal_zone_zones(root: &Path) -> Vec<ZoneSensors> {
        let mut used = HashSet::new();
        let mut zones = Vec::new();
        for zone in sorted_dir(root) {
            if !zone
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("thermal_zone"))
            {
                continue;
            }
            let (Some(kind), Ok(input)) = (
                read_trimmed(&zone.join("type")),
                File::open(zone.join("temp")),
            ) else {
                continue;
            };
            let kind = sanitize(&kind);
            zones.push(ZoneSensors {
                resource: hwmon_resource_by_name(&kind),
                id: unique_id(kind, &mut used),
                input,
                critical_celsius: None,
                source: "thermal_zone",
            });
        }
        zones
    }

    fn sorted_dir(path: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        entries
    }

    fn read_trimmed(path: &Path) -> Option<String> {
        Some(std::fs::read_to_string(path).ok()?.trim().to_string())
    }

    fn read_file_number<T: FromStr>(path: &Path) -> Option<T> {
        read_trimmed(path)?.parse().ok()
    }

    fn read_number<T: FromStr>(file: &File) -> Option<T> {
        let mut buffer = [0_u8; 32];
        let read = file.read_at(&mut buffer, 0).ok()?;
        std::str::from_utf8(&buffer[..read])
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn khz_to_hz(khz: f64) -> f64 {
        khz * 1_000.0
    }

    fn mhz_to_hz(mhz: f64) -> f64 {
        mhz * 1e6
    }

    fn millicelsius_to_celsius(millicelsius: f64) -> f64 {
        millicelsius / 1_000.0
    }

    /// Whether a clock reading can be believed. `cpuinfo_max_freq` already
    /// includes boost, so the headroom only absorbs firmware rounding — it is
    /// not a margin for a sensor that is wrong: the Orion O6's `cppc_cpufreq`
    /// reports 3.9GHz on a 1.8GHz part. A host that publishes no ceiling gets
    /// the benefit of the doubt, since there is nothing to check against.
    fn plausible(hz: f64, max_hz: Option<f64>) -> bool {
        max_hz.is_none_or(|max_hz| hz <= max_hz * CEILING_TOLERANCE)
    }

    fn aggregate(values: &[f64]) -> Option<(f64, f64)> {
        let peak = values.iter().copied().fold(f64::MIN, f64::max);
        (!values.is_empty()).then(|| (values.iter().sum::<f64>() / values.len() as f64, peak))
    }

    fn sanitize(label: &str) -> String {
        let mut result = String::with_capacity(label.len());
        for character in label.chars() {
            if character.is_ascii_alphanumeric() {
                result.push(character.to_ascii_lowercase());
            } else if !result.ends_with('_') {
                result.push('_');
            }
        }
        result.trim_matches('_').to_string()
    }

    fn unique_id(base: String, used: &mut HashSet<String>) -> String {
        let mut id = base.clone();
        let mut suffix = 1;
        while !used.insert(id.clone()) {
            suffix += 1;
            id = format!("{base}_{suffix}");
        }
        id
    }
}

#[cfg(target_os = "macos")]
mod imp {
    //! macOS exposes no unprivileged per-core clock. The kperf/KPC driver in
    //! this crate can count cycles system-wide, but KPC has a single global
    //! counter program: taking it over here would clobber the profiling
    //! session this monitor runs beside. So frequency is reported as
    //! unavailable and only the `sysctl` ceiling is published, with
    //! `mean_hz`/`peak_hz` left at zero. Die temperature is behind private
    //! SMC/IOKit keys and is not collected. Thermal pressure comes from the
    //! public `kOSThermalNotificationPressureLevelName` notification.

    use super::*;
    use std::ffi::{c_char, c_int, CString};

    const PRESSURE_NOTIFICATION: &str = "com.apple.system.thermalpressurelevel";
    const MAX_PRESSURE_LEVEL: u64 = 4;

    extern "C" {
        fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
        fn notify_get_state(token: c_int, state: *mut u64) -> u32;
    }

    /// Host ceiling clocks and OS thermal pressure.
    pub struct HostTelemetry {
        clusters: Vec<ClusterClocks>,
        pressure_token: Option<c_int>,
        unavailable: Vec<(&'static str, String)>,
    }

    impl HostTelemetry {
        /// Discover sensors once. `clusters` maps a cluster id to its logical
        /// CPUs; an empty slice means one `"host"` cluster. `Ok(None)` when
        /// the host exposes neither clocks nor temperatures.
        pub fn start(clusters: &[(String, Vec<u32>)]) -> io::Result<Option<Self>> {
            let mut unavailable = vec![
                (
                    "frequency",
                    "macOS exposes no unprivileged per-core clock; system-wide KPC cycle counting \
                     would collide with the profiler's own counter program"
                        .to_string(),
                ),
                (
                    "temperature",
                    "macOS exposes die temperature only through private SMC/IOKit APIs; not \
                     collected"
                        .to_string(),
                ),
            ];

            let max_hz = max_frequency_hz();
            let ids: Vec<String> = if clusters.is_empty() {
                vec!["host".to_string()]
            } else {
                clusters.iter().map(|(id, _)| id.clone()).collect()
            };
            let clusters: Vec<ClusterClocks> = max_hz
                .into_iter()
                .flat_map(|max_hz| {
                    ids.iter().map(move |id| ClusterClocks {
                        id: id.clone(),
                        mean_hz: 0.0,
                        peak_hz: 0.0,
                        max_hz: Some(max_hz),
                        source: "sysctl",
                    })
                })
                .collect();

            let pressure_token = register_pressure();
            if pressure_token.is_none() {
                unavailable.push((
                    "pressure_level",
                    format!("{PRESSURE_NOTIFICATION} is not published by this host"),
                ));
            }
            if clusters.is_empty() && pressure_token.is_none() {
                return Ok(None);
            }
            Ok(Some(Self {
                clusters,
                pressure_token,
                unavailable,
            }))
        }

        /// Per-signal reasons for anything `start` could not open.
        pub fn unavailable(&self) -> &[(&'static str, String)] {
            &self.unavailable
        }

        /// The clock ceiling is republished verbatim, so nothing is discarded.
        pub fn discarded_readings(&self) -> u64 {
            0
        }

        /// Read the OS thermal pressure level and republish the clock ceiling.
        pub fn sample(&mut self) -> io::Result<HostTelemetrySample> {
            let pressure_level = self.pressure_token.and_then(|token| {
                let mut state = 0_u64;
                (unsafe { notify_get_state(token, &mut state) } == 0)
                    .then(|| state.min(MAX_PRESSURE_LEVEL) as u8)
            });
            Ok(HostTelemetrySample {
                clusters: self.clusters.clone(),
                devices: Vec::new(),
                zones: Vec::new(),
                throttle_events: None,
                pressure_level,
            })
        }
    }

    fn register_pressure() -> Option<c_int> {
        let name = CString::new(PRESSURE_NOTIFICATION).ok()?;
        let mut token = 0;
        (unsafe { notify_register_check(name.as_ptr(), &mut token) } == 0).then_some(token)
    }

    fn max_frequency_hz() -> Option<f64> {
        [
            "hw.cpufrequency_max",
            "hw.perflevel0.freq_hardware_max",
            "hw.perflevel0.cpufrequency_max",
        ]
        .into_iter()
        .find_map(sysctl_u64)
        .map(|hz| hz as f64)
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        let name = CString::new(name).ok()?;
        let mut value = 0_u64;
        let mut len = std::mem::size_of::<u64>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::addr_of_mut!(value).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && value != 0).then_some(value)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use imp::HostTelemetry;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
/// Unsupported-platform stub.
pub struct HostTelemetry;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl HostTelemetry {
    /// No host clock or thermal sensors are available.
    pub fn start(_clusters: &[(String, Vec<u32>)]) -> io::Result<Option<Self>> {
        Ok(None)
    }
    /// No sensor was opened, so nothing is missing.
    pub fn unavailable(&self) -> &[(&'static str, String)] {
        &[]
    }
    /// No sensor was opened, so nothing was discarded.
    pub fn discarded_readings(&self) -> u64 {
        0
    }
    /// Return an empty sample.
    pub fn sample(&mut self) -> io::Result<HostTelemetrySample> {
        Ok(HostTelemetrySample::default())
    }
}
