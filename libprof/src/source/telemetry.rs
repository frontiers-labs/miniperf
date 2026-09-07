//! Host clock and thermal sampling, in any scenario.
//!
//! Frequencies and temperatures are host state, not a feature of one analysis:
//! every recording is conditioned on the clock the part actually ran at. The
//! monitor runs at 1Hz next to the workload and emits the same resource
//! samples the procfs collector does, so a consumer unions the two.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::{HostTelemetry, HostTelemetrySample, Record, ResourceSample, Sink, SourceStatus};

const INTERVAL: Duration = Duration::from_secs(1);

/// Samples the host's clock and temperature sensors while a workload runs.
#[derive(Default)]
pub struct HostTelemetrySource {
    stop: Option<Arc<AtomicBool>>,
    worker: Option<thread::JoinHandle<Vec<SourceStatus>>>,
}

impl Source for HostTelemetrySource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "host_telemetry",
        }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        Availability::Available
    }

    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()> {
        let clusters = host_clusters();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let sink = context.sink.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("libprof-host-telemetry".to_string())
                .spawn(move || collect(sink.as_ref(), &clusters, worker_stop))?,
        );
        self.stop = Some(stop);
        Ok(())
    }

    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        let worker = self.worker.take();
        if let Some(worker) = &worker {
            worker.thread().unpark();
        }
        worker
            .map(|worker| {
                worker.join().unwrap_or_else(|_| {
                    vec![SourceStatus::new(
                        "host_telemetry",
                        "error",
                        "internal",
                        "unavailable",
                        "host telemetry thread did not shut down cleanly",
                    )]
                })
            })
            .unwrap_or_default()
    }
}

/// Cluster id to logical CPUs, from the host's core PMUs. Empty on a
/// homogeneous host, which `HostTelemetry` reads as a single `host` cluster.
fn host_clusters() -> Vec<(String, Vec<u32>)> {
    crate::host_core_clusters()
        .into_iter()
        .map(|cluster| (cluster.family_id, parse_cpumask(&cluster.cpus)))
        .collect()
}

/// Expand a sysfs cpumask such as `"0,5-11"` into its logical CPU numbers.
fn parse_cpumask(mask: &str) -> Vec<u32> {
    mask.split(',')
        .filter_map(|range| {
            let range = range.trim();
            match range.split_once('-') {
                Some((first, last)) => Some(first.parse().ok()?..=last.parse().ok()?),
                None => {
                    let cpu = range.parse().ok()?;
                    Some(cpu..=cpu)
                }
            }
        })
        .flatten()
        .collect()
}

fn collect(
    sink: &dyn Sink,
    clusters: &[(String, Vec<u32>)],
    stop: Arc<AtomicBool>,
) -> Vec<SourceStatus> {
    let mut statuses = Vec::new();
    let mut telemetry = match HostTelemetry::start(clusters) {
        Ok(Some(telemetry)) => telemetry,
        Ok(None) => {
            statuses.push(SourceStatus::new(
                "host_telemetry",
                "unavailable",
                "sysfs",
                "unavailable",
                "the host exposes neither clock nor temperature sensors",
            ));
            return statuses;
        }
        Err(error) => {
            statuses.push(SourceStatus::new(
                "host_telemetry",
                "unavailable",
                "sysfs",
                "unavailable",
                &error.to_string(),
            ));
            return statuses;
        }
    };
    for (signal, reason) in telemetry.unavailable() {
        statuses.push(SourceStatus::new(
            signal,
            "unavailable",
            "sysfs",
            "unavailable",
            reason,
        ));
    }

    let start = Instant::now();
    let mut source = "sysfs";
    loop {
        let timestamp_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        match telemetry.sample() {
            Ok(sample) => {
                if let Some(cluster) = sample.clusters.first() {
                    source = cluster.source;
                }
                for row in telemetry_rows(timestamp_ns, &sample) {
                    sink.record(Record::Resource(row));
                }
            }
            Err(error) => {
                statuses.push(SourceStatus::new(
                    "host_telemetry",
                    "degraded",
                    source,
                    "best_effort",
                    &error.to_string(),
                ));
                break;
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        thread::park_timeout(INTERVAL);
    }
    let discarded = telemetry.discarded_readings();
    statuses.push(if discarded > 0 {
        SourceStatus::new(
            "host_telemetry",
            "degraded",
            source,
            "best_effort",
            &format!(
                "{discarded} clock reading(s) exceeded their cluster ceiling and were \
                 discarded; this host's cpufreq driver reports impossible frequencies"
            ),
        )
    } else {
        SourceStatus::new(
            "host_telemetry",
            "available",
            source,
            "exact_system",
            "host clock and temperature sensors",
        )
    });
    statuses
}

/// The rows one tick contributes. Measurements that read as zero carry no
/// information and would drag a mean down, so only ceilings and counters are
/// emitted at zero.
fn telemetry_rows(timestamp_ns: u64, sample: &HostTelemetrySample) -> Vec<ResourceSample> {
    let mut rows = Vec::new();
    let mut measurement = |resource: &str, id: &str, metric: &str, value: f64, unit, source| {
        if value.is_finite() && value > 0.0 {
            rows.push(host_sample(
                timestamp_ns,
                resource,
                id,
                "utilization",
                metric,
                value,
                unit,
                source,
            ));
        }
    };
    for cluster in &sample.clusters {
        measurement(
            "cpu",
            &cluster.id,
            "frequency",
            cluster.mean_hz,
            "hertz",
            cluster.source,
        );
        measurement(
            "cpu",
            &cluster.id,
            "frequency_peak",
            cluster.peak_hz,
            "hertz",
            cluster.source,
        );
        if let Some(max_hz) = cluster.max_hz {
            measurement(
                "cpu",
                &cluster.id,
                "frequency_max",
                max_hz,
                "hertz",
                cluster.source,
            );
        }
    }
    for device in &sample.devices {
        measurement(
            device.resource,
            &device.id,
            "frequency",
            device.cur_hz,
            "hertz",
            device.source,
        );
        if let Some(max_hz) = device.max_hz {
            measurement(
                device.resource,
                &device.id,
                "frequency_max",
                max_hz,
                "hertz",
                device.source,
            );
        }
        if let Some(busy) = device.busy_percent {
            measurement(
                device.resource,
                &device.id,
                "busy",
                busy,
                "percent",
                device.source,
            );
        }
    }
    for zone in &sample.zones {
        measurement(
            zone.resource,
            &zone.id,
            "temperature",
            zone.celsius,
            "celsius",
            zone.source,
        );
        if let Some(critical) = zone.critical_celsius {
            measurement(
                zone.resource,
                &zone.id,
                "temperature_critical",
                critical,
                "celsius",
                zone.source,
            );
        }
    }
    if let Some(events) = sample.throttle_events {
        rows.push(host_sample(
            timestamp_ns,
            "cpu",
            "host",
            "saturation",
            "throttle_events",
            events as f64,
            "events",
            "sysfs",
        ));
    }
    if let Some(level) = sample.pressure_level {
        rows.push(host_sample(
            timestamp_ns,
            "thermal",
            "host",
            "saturation",
            "thermal_pressure_level",
            level as f64,
            "level",
            "os_thermal_notification",
        ));
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn host_sample(
    timestamp_ns: u64,
    resource: &str,
    id: &str,
    category: &str,
    metric: &str,
    value: f64,
    unit: &str,
    source: &str,
) -> ResourceSample {
    super::resource_sample(
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
