//! Linux system-scoped memory-controller bandwidth counters.

use std::io;

/// PMU name prefixes that can expose memory-controller bandwidth counters.
pub const BANDWIDTH_PMU_PREFIXES: [&str; 4] = ["uncore_imc", "amd_df", "amd_umc", "arm_cmn"];

/// Vendor bandwidth device used where no uncore PMU is exposed.
const DDR_PERF_DEVICE: &str = "/dev/ddr_perf";

/// Whether `caps` describes a host on which [`MemoryControllerMonitor`] can
/// count DRAM bytes. The rung requirement and the monitor's own discovery must
/// agree, so both answer from here.
pub fn bandwidth_counters_present(caps: &crate::Capabilities) -> bool {
    BANDWIDTH_PMU_PREFIXES
        .iter()
        .any(|prefix| caps.pmus_with_prefix(prefix).next().is_some())
        || std::path::Path::new(DDR_PERF_DEVICE).exists()
}

/// Cumulative memory-controller bytes since the monitor was enabled.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryControllerSample {
    /// Bytes read by all discovered controllers.
    pub read_bytes: u64,
    /// Bytes written by all discovered controllers.
    pub write_bytes: u64,
    /// Package energy consumed while the monitor was enabled.
    pub package_joules: Option<f64>,
    /// CPU-core energy consumed while the monitor was enabled.
    pub core_joules: Option<f64>,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use perf_event_open_sys::{bindings, bindings::perf_event_attr, ioctls};
    use std::{
        collections::HashMap,
        fs,
        os::fd::RawFd,
        path::{Path, PathBuf},
    };

    const READ_ALIASES: &[&str] = &[
        "cas_count_read",
        "dram_reads",
        "data_read",
        "data_reads",
        "read",
    ];
    const WRITE_ALIASES: &[&str] = &[
        "cas_count_write",
        "dram_writes",
        "data_write",
        "data_writes",
        "write",
    ];

    #[derive(Clone, Copy)]
    enum Measurement {
        ReadBytes,
        WriteBytes,
        PackageJoules,
        CoreJoules,
    }

    struct Handle {
        fd: RawFd,
        measurement: Measurement,
        scale: f64,
    }

    /// Fallback bandwidth source for SoCs with no perf-exposed memory
    /// controller: the `ddr_bw` character device, `/dev/ddr_perf`, which takes
    /// an ioctl per AXI port and answers with the traffic seen since the
    /// previous call. Nothing here is tied to a particular SoC — the port
    /// count is probed, and the device is simply absent on hosts that lack it.
    ///
    /// Two things force a background poller rather than reads on demand. The
    /// returned deltas are `u32` bytes, so they saturate at 4GiB — under a
    /// second of real traffic on any modern controller — and the "previous"
    /// value lives in the driver, one per port for the whole system, so
    /// whoever calls first consumes the delta. Polling on a fixed short
    /// interval and accumulating into 64-bit counters keeps `sample` correct
    /// at any cadence.
    mod ddr_perf {
        use super::*;
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };
        use std::time::Duration;

        const DEVICE: &str = super::super::DDR_PERF_DEVICE;
        /// Bound on the port probe. Parts using this interface have a handful
        /// of AXI ports; the bound only stops a driver that answers for every
        /// index from spinning the probe forever.
        const MAX_PORTS: u32 = 16;
        /// 50ms keeps a single window under the driver's 4GiB saturation point
        /// for anything up to ~85GB/s, well past what parts lacking an uncore
        /// PMU deliver.
        const POLL: Duration = Duration::from_millis(50);

        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Stat {
            port_id: u32,
            window: u32,
            read_bytes: u32,
            write_bytes: u32,
            read_reqs: u32,
            write_reqs: u32,
            read_latency: u32,
        }

        const fn ioc(dir: u32, nr: u32, size: u32) -> libc::c_ulong {
            ((dir << 30) | (size << 16) | (b'D' as u32) << 8 | nr) as libc::c_ulong
        }
        // _IOW('D', 1, int) and _IOWR('D', 2, struct ddr_perf_stat).
        const IOC_INIT_PORT: libc::c_ulong = ioc(1, 1, 4);
        const IOC_GET_STAT: libc::c_ulong = ioc(3, 2, std::mem::size_of::<Stat>() as u32);

        struct Totals {
            read: AtomicU64,
            write: AtomicU64,
        }

        pub struct DdrPerf {
            totals: Arc<Totals>,
            stop: Arc<AtomicBool>,
            worker: Option<std::thread::JoinHandle<()>>,
        }

        fn get_stat(fd: i32, port: u32) -> Option<Stat> {
            let mut stat = Stat {
                port_id: port,
                ..Default::default()
            };
            let rc = unsafe { libc::ioctl(fd, IOC_GET_STAT, &mut stat as *mut Stat) };
            (rc == 0).then_some(stat)
        }

        impl DdrPerf {
            pub fn start() -> Option<Self> {
                if !Path::new(DEVICE).exists() {
                    return None;
                }
                let path = std::ffi::CString::new(DEVICE).ok()?;
                // The device is root-only; a plain user simply has no DDR
                // counters, which is not an error worth propagating.
                let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
                if fd < 0 {
                    return None;
                }

                // How many ports exist is a property of the SoC, not of the
                // interface, so probe rather than assume: initialize ports in
                // turn until the driver stops answering. The first read per
                // port only establishes the driver's baseline, so the probe
                // doubles as priming every port before counting.
                let ports = (0..MAX_PORTS)
                    .take_while(|port| {
                        let value = *port as libc::c_int;
                        unsafe { libc::ioctl(fd, IOC_INIT_PORT, &value as *const libc::c_int) };
                        get_stat(fd, *port).is_some()
                    })
                    .count() as u32;
                if ports == 0 {
                    unsafe { libc::close(fd) };
                    return None;
                }

                let totals = Arc::new(Totals {
                    read: AtomicU64::new(0),
                    write: AtomicU64::new(0),
                });
                let stop = Arc::new(AtomicBool::new(false));
                let worker = std::thread::spawn({
                    let totals = Arc::clone(&totals);
                    let stop = Arc::clone(&stop);
                    move || {
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(POLL);
                            let (mut read, mut write) = (0_u64, 0_u64);
                            for port in 0..ports {
                                if let Some(stat) = get_stat(fd, port) {
                                    read += u64::from(stat.read_bytes);
                                    write += u64::from(stat.write_bytes);
                                }
                            }
                            totals.read.fetch_add(read, Ordering::Relaxed);
                            totals.write.fetch_add(write, Ordering::Relaxed);
                        }
                        unsafe { libc::close(fd) };
                    }
                });

                Some(Self {
                    totals,
                    stop,
                    worker: Some(worker),
                })
            }

            pub fn sample(&self) -> (u64, u64) {
                (
                    self.totals.read.load(Ordering::Relaxed),
                    self.totals.write.load(Ordering::Relaxed),
                )
            }
        }

        impl Drop for DdrPerf {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
        }
    }

    /// Active system-scoped controller counters.
    pub struct MemoryControllerMonitor {
        handles: Vec<Handle>,
        ddr_perf: Option<ddr_perf::DdrPerf>,
    }

    impl MemoryControllerMonitor {
        /// Discover Intel IMC and AMD data-fabric PMUs, falling back to a
        /// vendor bandwidth device where no uncore PMU is exposed. `None`
        /// means the host offers neither.
        pub fn start() -> io::Result<Option<Self>> {
            let root = Path::new("/sys/bus/event_source/devices");
            let mut handles = Vec::new();
            let mut last_error = None;
            for entry in fs::read_dir(root).into_iter().flatten() {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !super::BANDWIDTH_PMU_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                {
                    continue;
                }
                let path = entry.path();
                for (measurement, aliases) in [
                    (Measurement::ReadBytes, READ_ALIASES),
                    (Measurement::WriteBytes, WRITE_ALIASES),
                ] {
                    let Some((event_name, event_path)) = aliases.iter().find_map(|alias| {
                        let candidate = path.join("events").join(alias);
                        candidate
                            .is_file()
                            .then(|| ((*alias).to_string(), candidate))
                    }) else {
                        continue;
                    };
                    match open_event(&path, &event_name, &event_path, measurement) {
                        Ok(handle) => handles.push(handle),
                        Err(error) => {
                            // PMUs and aliases are independently exposed and
                            // independently permissioned. Preserve every
                            // usable channel instead of discarding a host's
                            // read bandwidth because one write event failed.
                            last_error = Some(error);
                        }
                    }
                }
            }
            // No perf-exposed controller: try the vendor bandwidth device,
            // which some SoCs publish DDR traffic through instead.
            let ddr_perf = handles.is_empty().then(ddr_perf::DdrPerf::start).flatten();
            if handles.is_empty() && ddr_perf.is_none() {
                return match last_error {
                    Some(error) => Err(error),
                    None => Ok(None),
                };
            }
            // Energy counters are independent of where bandwidth comes from.
            let power = root.join("power");
            for (measurement, event_name) in [
                (Measurement::PackageJoules, "energy-pkg"),
                (Measurement::CoreJoules, "energy-cores"),
            ] {
                let event_path = power.join("events").join(event_name);
                if !event_path.is_file() {
                    continue;
                }
                if let Ok(handle) = open_event(&power, event_name, &event_path, measurement) {
                    handles.push(handle);
                }
            }
            for handle in &handles {
                unsafe {
                    ioctls::RESET(handle.fd, 0);
                    ioctls::ENABLE(handle.fd, 0);
                }
            }
            Ok(Some(Self { handles, ddr_perf }))
        }

        /// Where the counters come from, for recording in snapshot metadata.
        pub fn source(&self) -> &'static str {
            if self.ddr_perf.is_some() {
                "ddr_perf"
            } else {
                "perf_event/sysfs"
            }
        }

        /// Read cumulative, multiplexing-scaled bytes.
        pub fn sample(&self) -> io::Result<MemoryControllerSample> {
            let mut result = MemoryControllerSample::default();
            if let Some(ddr_perf) = &self.ddr_perf {
                let (read, write) = ddr_perf.sample();
                result.read_bytes = read;
                result.write_bytes = write;
            }
            for handle in &self.handles {
                let mut values = [0_u64; 3];
                let bytes = unsafe {
                    libc::read(
                        handle.fd,
                        values.as_mut_ptr().cast(),
                        std::mem::size_of_val(&values),
                    )
                };
                if bytes != std::mem::size_of_val(&values) as isize {
                    return Err(io::Error::last_os_error());
                }
                let scaled = if values[2] == 0 {
                    0.0
                } else {
                    values[0] as f64 * values[1] as f64 / values[2] as f64
                };
                let value = (scaled * handle.scale).max(0.0);
                match handle.measurement {
                    Measurement::ReadBytes => {
                        result.read_bytes = result
                            .read_bytes
                            .saturating_add(value.min(u64::MAX as f64) as u64)
                    }
                    Measurement::WriteBytes => {
                        result.write_bytes = result
                            .write_bytes
                            .saturating_add(value.min(u64::MAX as f64) as u64)
                    }
                    Measurement::PackageJoules => {
                        result.package_joules = Some(result.package_joules.unwrap_or(0.0) + value)
                    }
                    Measurement::CoreJoules => {
                        result.core_joules = Some(result.core_joules.unwrap_or(0.0) + value)
                    }
                }
            }
            Ok(result)
        }
    }

    impl Drop for MemoryControllerMonitor {
        fn drop(&mut self) {
            for handle in &self.handles {
                unsafe {
                    ioctls::DISABLE(handle.fd, 0);
                    libc::close(handle.fd);
                }
            }
        }
    }

    fn open_event(
        device: &Path,
        event_name: &str,
        event_path: &Path,
        measurement: Measurement,
    ) -> io::Result<Handle> {
        let pmu_type = read_number(&device.join("type"))? as u32;
        let cpu = fs::read_to_string(device.join("cpumask"))
            .or_else(|_| fs::read_to_string(device.join("cpus")))
            .ok()
            .and_then(|value| first_cpu(&value))
            .unwrap_or(0) as i32;
        let formats = read_formats(&device.join("format"))?;
        let assignments = fs::read_to_string(event_path)?;
        let mut registers = [0_u64; 3];
        for assignment in assignments.trim().split(',') {
            let Some((name, value)) = assignment.split_once('=') else {
                continue;
            };
            let value = parse_number(value.trim())?;
            if let Some((register, bits)) = formats.get(name.trim()) {
                apply_bits(&mut registers[*register], bits, value);
            }
        }
        let mut attr: perf_event_attr = unsafe { std::mem::zeroed() };
        attr.size = std::mem::size_of::<perf_event_attr>() as u32;
        attr.type_ = pmu_type;
        attr.config = registers[0];
        attr.config1 = registers[1];
        attr.config2 = registers[2];
        attr.set_disabled(1);
        attr.read_format = (bindings::PERF_FORMAT_TOTAL_TIME_ENABLED
            | bindings::PERF_FORMAT_TOTAL_TIME_RUNNING) as u64;
        let fd = unsafe { perf_event_open_sys::perf_event_open(&mut attr, -1, cpu, -1, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle {
            fd,
            measurement,
            scale: event_scale(device, event_name).unwrap_or(match measurement {
                Measurement::ReadBytes | Measurement::WriteBytes => 64.0,
                Measurement::PackageJoules | Measurement::CoreJoules => 1.0,
            }),
        })
    }

    fn event_scale(device: &Path, event: &str) -> Option<f64> {
        let scale = fs::read_to_string(device.join("events").join(format!("{event}.scale")))
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?;
        let unit = fs::read_to_string(device.join("events").join(format!("{event}.unit")))
            .unwrap_or_default()
            .to_ascii_lowercase();
        Some(
            scale
                * if unit.contains("mib") {
                    1024.0 * 1024.0
                } else if unit.contains("kib") {
                    1024.0
                } else {
                    1.0
                },
        )
    }

    fn read_formats(path: &Path) -> io::Result<HashMap<String, (usize, Vec<u32>)>> {
        let mut result = HashMap::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let value = fs::read_to_string(entry.path())?;
            let (register, bits) = value
                .trim()
                .split_once(':')
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid PMU format"))?;
            let register = match register {
                "config" => 0,
                "config1" => 1,
                "config2" => 2,
                _ => continue,
            };
            result.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (register, parse_bits(bits)?),
            );
        }
        Ok(result)
    }

    fn parse_bits(value: &str) -> io::Result<Vec<u32>> {
        let mut bits = Vec::new();
        for part in value.split(',') {
            if let Some((start, end)) = part.split_once('-') {
                bits.extend(
                    start.parse::<u32>().map_err(invalid)?.to_owned()
                        ..=end.parse::<u32>().map_err(invalid)?,
                );
            } else {
                bits.push(part.parse::<u32>().map_err(invalid)?);
            }
        }
        Ok(bits)
    }

    fn apply_bits(target: &mut u64, bits: &[u32], value: u64) {
        for (source, destination) in bits.iter().enumerate() {
            if value & (1_u64 << source) != 0 {
                *target |= 1_u64 << destination;
            }
        }
    }
    fn first_cpu(value: &str) -> Option<u32> {
        value
            .trim()
            .split(',')
            .next()?
            .split('-')
            .next()?
            .parse()
            .ok()
    }
    fn read_number(path: &PathBuf) -> io::Result<u64> {
        parse_number(fs::read_to_string(path)?.trim())
    }
    fn parse_number(value: &str) -> io::Result<u64> {
        u64::from_str_radix(
            value.trim_start_matches("0x"),
            if value.starts_with("0x") { 16 } else { 10 },
        )
        .map_err(invalid)
    }
    fn invalid(error: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
    }
}

#[cfg(target_os = "linux")]
pub use imp::MemoryControllerMonitor;

#[cfg(not(target_os = "linux"))]
/// Unsupported-platform stub.
pub struct MemoryControllerMonitor;

#[cfg(not(target_os = "linux"))]
impl MemoryControllerMonitor {
    /// No platform memory counters are available.
    pub fn start() -> io::Result<Option<Self>> {
        Ok(None)
    }
    /// Never reached: [`Self::start`] reports no monitor.
    pub fn source(&self) -> &'static str {
        "unavailable"
    }
    /// Return an empty sample.
    pub fn sample(&self) -> io::Result<MemoryControllerSample> {
        Ok(MemoryControllerSample::default())
    }
}
