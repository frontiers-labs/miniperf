//! Coarse process-tree and system resource telemetry from procfs and cgroup v2.
//!
//! Nothing here selects code at compile time: every reading is a file that
//! either exists on this host or does not. A host without `/proc` probes
//! unavailable and the recording carries on without it.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{resource_sample, Availability, SessionContext, Source, SourceDecl};
use crate::{
    platform::{self, ProcessContext, ProcessIo},
    MemoryControllerMonitor, ProcessInfo, Record, ResourceSample, Sink, SourceStatus,
};

const INTERVAL: Duration = Duration::from_secs(1);

/// procfs, cgroup and memory-controller pollers as one source: they share the
/// private cgroup scope and the once-per-second collection loop.
#[derive(Default)]
pub struct ProcfsSource {
    cgroup: Option<CgroupScope>,
    stop: Option<Arc<AtomicBool>>,
    worker: Option<thread::JoinHandle<Vec<SourceStatus>>>,
}

impl Source for ProcfsSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "procfs_resources",
        }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        if !platform::process_tree_supported() {
            return Availability::Unavailable {
                reason: "this host exposes no procfs process tree".to_string(),
            };
        }
        Availability::Available
    }

    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()> {
        let root_pid = context.root_pid();
        let launched = context.launched();
        let cgroup = CgroupScope::create(root_pid, launched);
        let path = cgroup.path();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let sink = context.sink.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("libprof-procfs-resources".to_string())
                .spawn(move || collect(sink.as_ref(), root_pid, launched, path, worker_stop))?,
        );
        self.cgroup = Some(cgroup);
        self.stop = Some(stop);
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        let mut statuses = self
            .worker
            .take()
            .map(|worker| {
                worker.join().unwrap_or_else(|_| {
                    vec![SourceStatus::new(
                        "resource_monitor",
                        "error",
                        "internal",
                        "unavailable",
                        "resource collector thread did not shut down cleanly",
                    )]
                })
            })
            .unwrap_or_default();
        if let Some(cgroup) = self.cgroup.take() {
            statuses.push(cgroup.status());
        }
        statuses
    }
}

/// A private cgroup v2 scope holding the launched process tree, so its resource
/// accounting is exact rather than sampled.
struct CgroupScope {
    path: Option<PathBuf>,
    status: SourceStatus,
}

impl CgroupScope {
    fn create(root_pid: u32, launched: bool) -> Self {
        if !launched {
            return Self {
                path: None,
                status: SourceStatus::new(
                    "cgroup",
                    "unavailable",
                    "cgroup_v2",
                    "best_effort",
                    "attached processes are not moved; using non-invasive procfs/BPF tracking",
                ),
            };
        }
        let relative = fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|value| {
                value
                    .lines()
                    .find_map(|line| line.strip_prefix("0::").map(str::to_string))
            });
        let Some(relative) = relative else {
            return Self::failed("unified cgroup v2 is unavailable");
        };
        let parent = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        let path = parent.join(format!("mperf-snapshot-{root_pid}"));
        if let Err(error) = fs::create_dir(&path) {
            return Self::failed(&format!("cgroup delegation is unavailable: {error}"));
        }
        if let Err(error) = fs::write(path.join("cgroup.procs"), root_pid.to_string()) {
            let _ = fs::remove_dir(&path);
            return Self::failed(&format!(
                "could not place target in private cgroup: {error}"
            ));
        }
        Self {
            path: Some(path),
            status: SourceStatus::new(
                "cgroup",
                "available",
                "cgroup_v2",
                "exact_process_tree",
                "launched descendants inherit a private cgroup",
            ),
        }
    }

    fn failed(message: &str) -> Self {
        Self {
            path: None,
            status: SourceStatus::new("cgroup", "unavailable", "cgroup_v2", "best_effort", message),
        }
    }

    fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    fn status(&self) -> SourceStatus {
        self.status.clone()
    }
}

impl Drop for CgroupScope {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir(path);
        }
    }
}

/// Everything one process contributes to the tree's running totals.
#[derive(Clone, Copy, Debug, Default)]
struct ProcessTotals {
    user_ticks: u64,
    system_ticks: u64,
    minor_faults: u64,
    major_faults: u64,
    io: ProcessIo,
    context: ProcessContext,
}

fn collect(
    sink: &dyn Sink,
    root_pid: u32,
    launched: bool,
    cgroup: Option<PathBuf>,
    stop: Arc<AtomicBool>,
) -> Vec<SourceStatus> {
    let start = Instant::now();
    let mut previous = HashMap::<(u32, u64), ProcessTotals>::new();
    let mut cumulative = ProcessTotals::default();
    let mut processes = HashMap::<(u32, u64), ProcessInfo>::new();
    let ticks_per_second = platform::ticks_per_second();
    let page_size = platform::page_size();
    let mut statuses = vec![SourceStatus::new(
        "process_tree",
        "available",
        "procfs",
        "best_effort",
        "existing and future descendants are polled by PID/start-time identity",
    )];
    let mut initial_scan = true;
    let mut memory_controller = match MemoryControllerMonitor::start() {
        Ok(Some(monitor)) => {
            statuses.push(SourceStatus::new(
                "uncore_memory",
                "available",
                monitor.source(),
                "system_during_target",
                "memory-controller read/write counters",
            ));
            Some(monitor)
        }
        Ok(None) => {
            statuses.push(SourceStatus::new(
                "uncore_memory",
                "unavailable",
                "perf_event/sysfs",
                "unavailable",
                "no memory-controller PMU aliases and no vendor bandwidth device",
            ));
            None
        }
        Err(error) => {
            statuses.push(SourceStatus::new(
                "uncore_memory",
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "permission_denied"
                } else {
                    "error"
                },
                "perf_event/sysfs",
                "unavailable",
                &error.to_string(),
            ));
            None
        }
    };

    loop {
        let timestamp_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let tree = platform::process_tree(root_pid).unwrap_or_default();
        let mut rss_bytes = 0_f64;
        let mut pss_bytes = 0_f64;
        let mut current = HashMap::new();
        for stat in tree {
            let key = (stat.pid, stat.start_ticks);
            let totals = ProcessTotals {
                user_ticks: stat.user_ticks,
                system_ticks: stat.system_ticks,
                minor_faults: stat.minor_faults,
                major_faults: stat.major_faults,
                io: platform::process_io(stat.pid),
                context: platform::process_context(stat.pid),
            };
            // A process seen before contributes its delta. One seen for the
            // first time contributes everything it has, except on the very
            // first scan of an attached tree, whose history predates us.
            let baseline = previous
                .get(&key)
                .copied()
                .or_else(|| (launched || !initial_scan).then_some(ProcessTotals::default()));
            if let Some(before) = baseline {
                cumulative.add_delta(&totals, &before);
            }
            rss_bytes += stat.rss_pages.max(0) as f64 * page_size;
            pss_bytes += proc_kib(stat.pid, "smaps_rollup", "Pss") * 1024.0;
            let process = processes.entry(key).or_insert_with(|| ProcessInfo {
                pid: stat.pid,
                ppid: stat.ppid,
                start_ticks: stat.start_ticks,
                first_seen_ns: timestamp_ns,
                last_seen_ns: timestamp_ns,
                command: stat.command.clone(),
                quality: "procfs_best_effort".to_string(),
            });
            process.last_seen_ns = timestamp_ns;
            current.insert(key, totals);
        }
        previous = current;
        initial_scan = false;

        let mut samples = vec![
            tree_sample(
                timestamp_ns,
                "cpu",
                "utilization",
                "user_time",
                cumulative.user_ticks as f64 / ticks_per_second,
                "seconds",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "cpu",
                "utilization",
                "system_time",
                cumulative.system_ticks as f64 / ticks_per_second,
                "seconds",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "cpu",
                "errors",
                "minor_faults",
                cumulative.minor_faults as f64,
                "faults",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "memory",
                "errors",
                "major_faults",
                cumulative.major_faults as f64,
                "faults",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "memory",
                "utilization",
                "pss",
                pss_bytes,
                "bytes",
                "procfs_smaps_rollup",
            ),
            tree_sample(
                timestamp_ns,
                "memory",
                "utilization",
                "rss",
                rss_bytes,
                "bytes",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "disk",
                "utilization",
                "read_bytes",
                cumulative.io.read_bytes as f64,
                "bytes",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "disk",
                "utilization",
                "write_bytes",
                cumulative.io.write_bytes as f64,
                "bytes",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "disk",
                "utilization",
                "read_calls",
                cumulative.io.read_calls as f64,
                "operations",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "disk",
                "utilization",
                "write_calls",
                cumulative.io.write_calls as f64,
                "operations",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "cpu",
                "saturation",
                "voluntary_context_switches",
                cumulative.context.voluntary as f64,
                "switches",
                "procfs",
            ),
            tree_sample(
                timestamp_ns,
                "cpu",
                "saturation",
                "involuntary_context_switches",
                cumulative.context.involuntary as f64,
                "switches",
                "procfs",
            ),
        ];
        collect_pressure(timestamp_ns, &mut samples);
        collect_meminfo(timestamp_ns, &mut samples);
        collect_diskstats(timestamp_ns, &mut samples);
        collect_network(timestamp_ns, &mut samples);
        if let Some(cgroup) = &cgroup {
            collect_cgroup(timestamp_ns, cgroup, &mut samples);
        }
        if let Some(monitor) = memory_controller.as_mut() {
            match monitor.sample() {
                Ok(sample) => {
                    samples.push(system_sample(
                        timestamp_ns,
                        "memory",
                        "utilization",
                        "dram_read_bytes",
                        sample.read_bytes as f64,
                        "bytes",
                        "uncore_pmu",
                    ));
                    samples.push(system_sample(
                        timestamp_ns,
                        "memory",
                        "utilization",
                        "dram_write_bytes",
                        sample.write_bytes as f64,
                        "bytes",
                        "uncore_pmu",
                    ));
                    if let Some(joules) = sample.package_joules {
                        samples.push(system_sample(
                            timestamp_ns,
                            "cpu",
                            "utilization",
                            "package_energy",
                            joules,
                            "joules",
                            "power_pmu",
                        ));
                    }
                    if let Some(joules) = sample.core_joules {
                        samples.push(system_sample(
                            timestamp_ns,
                            "cpu",
                            "utilization",
                            "core_energy",
                            joules,
                            "joules",
                            "power_pmu",
                        ));
                    }
                }
                Err(error) => {
                    statuses.push(SourceStatus::new(
                        "uncore_memory",
                        "error",
                        "perf_event/sysfs",
                        "unavailable",
                        &error.to_string(),
                    ));
                    memory_controller = None;
                }
            }
        }
        for sample in samples {
            sink.record(Record::Resource(sample));
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        thread::park_timeout(INTERVAL);
    }

    let mut rows = processes.into_values().collect::<Vec<_>>();
    rows.sort_by_key(|process| (process.first_seen_ns, process.pid));
    for row in rows {
        sink.record(Record::Process(row));
    }
    statuses
}

impl ProcessTotals {
    /// Fold one process's progress since `before` into the tree's totals.
    fn add_delta(&mut self, after: &ProcessTotals, before: &ProcessTotals) {
        let fold = |total: &mut u64, after: u64, before: u64| {
            *total = total.saturating_add(after.saturating_sub(before));
        };
        fold(&mut self.user_ticks, after.user_ticks, before.user_ticks);
        fold(
            &mut self.system_ticks,
            after.system_ticks,
            before.system_ticks,
        );
        fold(
            &mut self.minor_faults,
            after.minor_faults,
            before.minor_faults,
        );
        fold(
            &mut self.major_faults,
            after.major_faults,
            before.major_faults,
        );
        fold(
            &mut self.io.read_bytes,
            after.io.read_bytes,
            before.io.read_bytes,
        );
        fold(
            &mut self.io.write_bytes,
            after.io.write_bytes,
            before.io.write_bytes,
        );
        fold(
            &mut self.io.read_calls,
            after.io.read_calls,
            before.io.read_calls,
        );
        fold(
            &mut self.io.write_calls,
            after.io.write_calls,
            before.io.write_calls,
        );
        fold(
            &mut self.context.voluntary,
            after.context.voluntary,
            before.context.voluntary,
        );
        fold(
            &mut self.context.involuntary,
            after.context.involuntary,
            before.context.involuntary,
        );
    }
}

/// A `key: value kB` field of a per-process procfs file, zero when absent.
fn proc_kib(pid: u32, file: &str, key: &str) -> f64 {
    key_values(&format!("/proc/{pid}/{file}"))
        .get(key)
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// `key: value` lines, as procfs writes them.
fn key_values(path: &str) -> HashMap<String, String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

/// `key value` lines, as the cgroup stat files write them.
fn whitespace_values(path: &Path) -> HashMap<String, String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

fn collect_pressure(timestamp_ns: u64, samples: &mut Vec<ResourceSample>) {
    for resource in ["cpu", "memory", "io"] {
        let path = format!("/proc/pressure/{resource}");
        let Ok(value) = fs::read_to_string(path) else {
            continue;
        };
        for line in value.lines() {
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next() else { continue };
            for field in fields {
                let Some((name, value)) = field.split_once('=') else {
                    continue;
                };
                if name != "avg10" {
                    continue;
                }
                if let Ok(value) = value.parse::<f64>() {
                    samples.push(system_sample(
                        timestamp_ns,
                        resource,
                        "saturation",
                        &format!("psi_{kind}_avg10"),
                        value,
                        "percent",
                        "procfs_psi",
                    ));
                }
            }
        }
    }
}

fn collect_meminfo(timestamp_ns: u64, samples: &mut Vec<ResourceSample>) {
    let values = key_values("/proc/meminfo");
    for (key, metric) in [
        ("MemTotal", "host_total"),
        ("MemAvailable", "host_available"),
        ("SwapTotal", "swap_total"),
        ("SwapFree", "swap_free"),
    ] {
        if let Some(kib) = values
            .get(key)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<f64>().ok())
        {
            samples.push(system_sample(
                timestamp_ns,
                "memory",
                "utilization",
                metric,
                kib * 1024.0,
                "bytes",
                "procfs",
            ));
        }
    }
}

fn collect_diskstats(timestamp_ns: u64, samples: &mut Vec<ResourceSample>) {
    let Ok(value) = fs::read_to_string("/proc/diskstats") else {
        return;
    };
    for line in value.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 14 {
            continue;
        }
        let device = fields[2];
        let Ok(io_ms) = fields[12].parse::<f64>() else {
            continue;
        };
        let Ok(weighted_ms) = fields[13].parse::<f64>() else {
            continue;
        };
        samples.push(device_sample(
            timestamp_ns,
            "disk",
            device,
            "utilization",
            "busy_time",
            io_ms,
            "milliseconds",
            "procfs_diskstats",
        ));
        samples.push(device_sample(
            timestamp_ns,
            "disk",
            device,
            "saturation",
            "weighted_io_time",
            weighted_ms,
            "milliseconds",
            "procfs_diskstats",
        ));
    }
}

fn collect_network(timestamp_ns: u64, samples: &mut Vec<ResourceSample>) {
    let Ok(value) = fs::read_to_string("/proc/net/dev") else {
        return;
    };
    for line in value.lines().skip(2) {
        let Some((interface, rest)) = line.split_once(':') else {
            continue;
        };
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 16 {
            continue;
        }
        for (metric, index, category) in [
            ("receive_bytes", 0, "utilization"),
            ("receive_errors", 2, "errors"),
            ("receive_drops", 3, "errors"),
            ("transmit_bytes", 8, "utilization"),
            ("transmit_errors", 10, "errors"),
            ("transmit_drops", 11, "errors"),
        ] {
            if let Ok(value) = fields[index].parse::<f64>() {
                samples.push(device_sample(
                    timestamp_ns,
                    "network",
                    interface.trim(),
                    category,
                    metric,
                    value,
                    if metric.ends_with("bytes") {
                        "bytes"
                    } else {
                        "events"
                    },
                    "procfs_netdev",
                ));
            }
        }
        if let Some(mbps) = fs::read_to_string(format!("/sys/class/net/{}/speed", interface.trim()))
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| *value > 0.0)
        {
            samples.push(device_sample(
                timestamp_ns,
                "network",
                interface.trim(),
                "utilization",
                "link_capacity",
                mbps * 1_000_000.0,
                "bits_per_second",
                "sysfs",
            ));
        }
    }
}

fn collect_cgroup(timestamp_ns: u64, path: &Path, samples: &mut Vec<ResourceSample>) {
    let push = |samples: &mut Vec<ResourceSample>,
                resource: &str,
                category: &str,
                metric: &str,
                value: f64,
                unit: &str| {
        samples.push(resource_sample(
            timestamp_ns,
            resource,
            "private_cgroup",
            category,
            metric,
            value,
            unit,
            "process_tree",
            "cgroup_v2",
            "exact_process_tree",
        ));
    };
    let cpu = whitespace_values(&path.join("cpu.stat"));
    for (key, metric, unit, scale, category) in [
        (
            "usage_usec",
            "cgroup_cpu_time",
            "seconds",
            1e-6,
            "utilization",
        ),
        (
            "user_usec",
            "cgroup_user_time",
            "seconds",
            1e-6,
            "utilization",
        ),
        (
            "system_usec",
            "cgroup_system_time",
            "seconds",
            1e-6,
            "utilization",
        ),
        (
            "nr_throttled",
            "cpu_throttle_events",
            "events",
            1.0,
            "saturation",
        ),
        (
            "throttled_usec",
            "cpu_throttled_time",
            "seconds",
            1e-6,
            "saturation",
        ),
    ] {
        if let Some(value) = cpu.get(key).and_then(|value| value.parse::<f64>().ok()) {
            push(samples, "cpu", category, metric, value * scale, unit);
        }
    }
    for (file, metric) in [
        ("memory.current", "cgroup_memory_current"),
        ("memory.peak", "cgroup_memory_peak"),
        ("memory.max", "cgroup_memory_limit"),
    ] {
        if let Some(value) = fs::read_to_string(path.join(file))
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
        {
            push(samples, "memory", "utilization", metric, value, "bytes");
        }
    }
    let memory_events = whitespace_values(&path.join("memory.events"));
    for (key, metric) in [
        ("oom", "oom_events"),
        ("oom_kill", "oom_kills"),
        ("max", "memory_limit_events"),
    ] {
        if let Some(value) = memory_events
            .get(key)
            .and_then(|value| value.parse::<f64>().ok())
        {
            push(samples, "memory", "errors", metric, value, "events");
        }
    }
    if let Ok(io) = fs::read_to_string(path.join("io.stat")) {
        for line in io.lines() {
            let mut fields = line.split_whitespace();
            let device = fields.next().unwrap_or("unknown");
            for field in fields {
                let Some((key, value)) = field.split_once('=') else {
                    continue;
                };
                let Ok(value) = value.parse::<f64>() else {
                    continue;
                };
                let (metric, unit) = match key {
                    "rbytes" => ("cgroup_read_bytes", "bytes"),
                    "wbytes" => ("cgroup_write_bytes", "bytes"),
                    "rios" => ("cgroup_read_operations", "operations"),
                    "wios" => ("cgroup_write_operations", "operations"),
                    _ => continue,
                };
                samples.push(resource_sample(
                    timestamp_ns,
                    "disk",
                    device,
                    "utilization",
                    metric,
                    value,
                    unit,
                    "process_tree",
                    "cgroup_v2",
                    "exact_process_tree",
                ));
            }
        }
    }
    for resource in ["cpu", "memory", "io"] {
        let Ok(value) = fs::read_to_string(path.join(format!("{resource}.pressure"))) else {
            continue;
        };
        for line in value.lines() {
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next() else { continue };
            for field in fields {
                let Some(("avg10", value)) = field.split_once('=') else {
                    continue;
                };
                if let Ok(value) = value.parse::<f64>() {
                    push(
                        samples,
                        resource,
                        "saturation",
                        &format!("cgroup_psi_{kind}_avg10"),
                        value,
                        "percent",
                    );
                }
            }
        }
    }
}

fn tree_sample(
    timestamp_ns: u64,
    resource: &str,
    category: &str,
    metric: &str,
    value: f64,
    unit: &str,
    source: &str,
) -> ResourceSample {
    resource_sample(
        timestamp_ns,
        resource,
        "process_tree",
        category,
        metric,
        value,
        unit,
        "process_tree",
        source,
        "best_effort",
    )
}

fn system_sample(
    timestamp_ns: u64,
    resource: &str,
    category: &str,
    metric: &str,
    value: f64,
    unit: &str,
    source: &str,
) -> ResourceSample {
    resource_sample(
        timestamp_ns,
        resource,
        "host",
        category,
        metric,
        value,
        unit,
        "system_during_target",
        source,
        "exact_system",
    )
}

#[allow(clippy::too_many_arguments)]
fn device_sample(
    timestamp_ns: u64,
    resource: &str,
    id: &str,
    category: &str,
    metric: &str,
    value: f64,
    unit: &str,
    source: &str,
) -> ResourceSample {
    resource_sample(
        timestamp_ns,
        resource,
        id,
        category,
        metric,
        value,
        unit,
        "system_during_target",
        source,
        "exact_system",
    )
}
