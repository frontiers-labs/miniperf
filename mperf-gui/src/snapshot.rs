use std::collections::BTreeMap;

use crate::sql::Connection;

/// USE-method snapshot data: per-resource utilization/saturation/errors
/// summaries, ranked findings, collector coverage, and metric time series.
#[derive(Debug, Clone)]
pub struct SnapshotData {
    pub resources: Vec<ResourceUse>,
    pub findings: Vec<SnapshotFinding>,
    pub collectors: Vec<SnapshotCollector>,
    clock_health: Option<ClockHealth>,
}

/// Clock state of the worst-off core cluster over the whole recording: every
/// other metric is conditioned on the frequency the part actually ran at.
#[derive(Debug, Clone)]
pub struct ClockHealth {
    /// The cluster these numbers describe, named only on heterogeneous hosts.
    pub cluster: Option<String>,
    pub mean_hz: f64,
    pub max_hz: Option<f64>,
    /// Throttle events accumulated during the run, not since boot.
    pub throttle_events: f64,
    pub severity: Severity,
}

impl ClockHealth {
    /// One-line badge text, e.g.
    /// `clocks: cortex_a520 at 1.10 GHz avg · 61% of 1.80 GHz max`.
    pub fn label(&self) -> String {
        let mut label = match &self.cluster {
            Some(cluster) => format!(
                "clocks: {cluster} at {} avg",
                format_value(self.mean_hz, "hertz")
            ),
            None => format!("clocks: {} avg", format_value(self.mean_hz, "hertz")),
        };
        if let Some(max) = self.max_hz {
            label.push_str(&format!(
                " · {:.0}% of {} max",
                self.mean_hz / max * 100.0,
                format_value(max, "hertz")
            ));
        }
        if self.throttle_events > 0.0 {
            label.push_str(&format!(
                " · {} throttle events",
                format_count(self.throttle_events)
            ));
        }
        label
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Medium,
    High,
}

impl Severity {
    fn parse(value: &str) -> Self {
        match value {
            "high" => Self::High,
            "medium" => Self::Medium,
            _ => Self::Info,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UseCategory {
    Utilization,
    Saturation,
    Errors,
}

impl UseCategory {
    pub const ALL: [Self; 3] = [Self::Utilization, Self::Saturation, Self::Errors];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "utilization" => Some(Self::Utilization),
            "saturation" => Some(Self::Saturation),
            "errors" => Some(Self::Errors),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Utilization => "Utilization",
            Self::Saturation => "Saturation",
            Self::Errors => "Errors",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotFinding {
    pub severity: Severity,
    pub resource: String,
    pub finding: String,
    pub evidence: String,
    pub recommendation: String,
    pub quality: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotCollector {
    pub name: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ResourceUse {
    pub resource: String,
    /// Derived one-line summary, e.g. "3.5% of 48 CPUs".
    pub headline: Option<String>,
    /// Formatted summary metrics per USE category, in category order.
    pub summaries: [Vec<SummaryMetric>; 3],
    pub charts: Vec<SnapshotChart>,
}

#[derive(Debug, Clone)]
pub struct SummaryMetric {
    pub metric: String,
    /// Which cluster, zone or device this row measured, on the resources that
    /// have more than one. `None` where the name would say nothing.
    pub entity: Option<String>,
    pub value: String,
    pub scope: String,
    /// 0..=1 position against the metric's natural ceiling, when it has one.
    pub fraction: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SnapshotChart {
    pub metric: String,
    pub category: UseCategory,
    /// Display unit after rate conversion, e.g. "MiB/s", "cores", "%".
    pub unit: String,
    /// One series per resource id (host, device, interface, …), sorted by id.
    pub series: Vec<ChartSeries>,
    pub max_value: f64,
}

#[derive(Debug, Clone)]
pub struct ChartSeries {
    pub id: String,
    /// (seconds from recording start, value in display units)
    pub points: Vec<(f64, f64)>,
}

struct SampleRow {
    timestamp_ns: u64,
    resource: String,
    resource_id: String,
    category: UseCategory,
    metric: String,
    value: f64,
    unit: String,
    scope: String,
}

impl SnapshotData {
    /// Returns `None` when the recording carries no usable snapshot tables.
    pub fn load(connection: &Connection, logical_cpu_count: Option<u32>) -> Option<Self> {
        let samples = load_samples(connection)?;
        if samples.is_empty() {
            return None;
        }
        let duration_ns = samples
            .iter()
            .map(|sample| sample.timestamp_ns)
            .max()
            .unwrap_or(0);
        let findings = load_findings(connection);
        let collectors = load_collectors(connection);
        let summary_rows = load_summary(connection);
        let clock_health = clock_health(&samples, &summary_rows);
        let resources = build_resources(samples, &summary_rows, duration_ns, logical_cpu_count);
        if resources.is_empty() {
            return None;
        }

        Some(Self {
            resources,
            findings,
            collectors,
            clock_health,
        })
    }

    /// Mean clock, its ceiling and the throttling seen during the run, when the
    /// recording carries frequency samples.
    pub fn clock_health(&self) -> Option<ClockHealth> {
        self.clock_health.clone()
    }
}

/// Clocks are per core cluster, and a low clock is only evidence of being held
/// back when the cluster demonstrated it could boost: a cluster whose observed
/// `frequency_peak` reached its own `frequency_max` and whose mean sat well
/// below it was throttled, while one that never approached its ceiling was
/// merely idle. The worst cluster wins; hardware throttle counts outrank both.
fn clock_health(samples: &[SampleRow], summary_rows: &[SummaryRow]) -> Option<ClockHealth> {
    #[derive(Default)]
    struct Cluster {
        sum: f64,
        count: u64,
        peak_hz: f64,
        max_hz: Option<f64>,
    }

    impl Cluster {
        fn ratio(&self) -> Option<f64> {
            let max = self.max_hz.filter(|max| *max > 0.0)?;
            Some(self.sum / self.count as f64 / max)
        }

        /// Severity from clocks alone, `Info` without proof the cluster boosted.
        fn severity(&self) -> Severity {
            let Some(ratio) = self.ratio() else {
                return Severity::Info;
            };
            if self.peak_hz < self.max_hz.unwrap_or(0.0) * 0.90 {
                Severity::Info
            } else if ratio < 0.75 {
                Severity::High
            } else if ratio < 0.90 {
                Severity::Medium
            } else {
                Severity::Info
            }
        }
    }

    let mut clusters = BTreeMap::<&str, Cluster>::new();
    let mut throttle = BTreeMap::<&str, (f64, f64)>::new();
    for sample in samples.iter().filter(|sample| sample.resource == "cpu") {
        match sample.metric.as_str() {
            "frequency" => {
                let cluster = clusters.entry(sample.resource_id.as_str()).or_default();
                cluster.sum += sample.value;
                cluster.count += 1;
            }
            "frequency_peak" => {
                let cluster = clusters.entry(sample.resource_id.as_str()).or_default();
                cluster.peak_hz = cluster.peak_hz.max(sample.value);
            }
            "frequency_max" => {
                let cluster = clusters.entry(sample.resource_id.as_str()).or_default();
                cluster.max_hz = Some(cluster.max_hz.unwrap_or(0.0).max(sample.value));
            }
            "throttle_events" => {
                let entry = throttle
                    .entry(sample.resource_id.as_str())
                    .or_insert((sample.value, sample.value));
                entry.0 = entry.0.min(sample.value);
                entry.1 = entry.1.max(sample.value);
            }
            _ => {}
        }
    }
    clusters.retain(|_, cluster| cluster.count > 0);
    if clusters.is_empty() {
        return None;
    }

    let named = clusters.len() > 1;
    let (id, worst) = clusters.iter().max_by(|left, right| {
        left.1.severity().cmp(&right.1.severity()).then_with(|| {
            right
                .1
                .ratio()
                .unwrap_or(f64::INFINITY)
                .total_cmp(&left.1.ratio().unwrap_or(f64::INFINITY))
        })
    })?;

    let mean_hz = worst.sum / worst.count as f64;
    let max_hz = worst.max_hz.filter(|max| *max > 0.0).or_else(|| {
        // A homogeneous host has one ceiling, so the summary's is that cluster's.
        (!named)
            .then(|| summary_value(summary_rows, "cpu", "frequency_max"))
            .flatten()
    });
    let throttle_events = throttle.values().map(|(min, max)| max - min).sum::<f64>();
    let severity = if throttle_events > 0.0 {
        Severity::High
    } else {
        worst.severity()
    };
    Some(ClockHealth {
        cluster: named.then(|| (*id).to_owned()),
        mean_hz,
        max_hz,
        throttle_events,
        severity,
    })
}

fn load_samples(connection: &Connection) -> Option<Vec<SampleRow>> {
    let mut statement = connection
        .prepare(
            "SELECT timestamp_ns, resource, resource_id, category, metric, value, unit, scope
             FROM snapshot_resource_samples ORDER BY timestamp_ns ASC;",
        )
        .ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .ok()?;
    let mut samples = Vec::new();
    for row in rows {
        let (timestamp_ns, resource, resource_id, category, metric, value, unit, scope) =
            row.ok()?;
        let Some(category) = UseCategory::parse(&category) else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        samples.push(SampleRow {
            timestamp_ns: timestamp_ns.max(0) as u64,
            resource,
            resource_id,
            category,
            metric,
            value,
            unit,
            scope,
        });
    }
    Some(samples)
}

struct SummaryRow {
    resource: String,
    resource_id: String,
    category: UseCategory,
    metric: String,
    value: Option<f64>,
    unit: String,
    scope: String,
}

fn load_summary(connection: &Connection) -> Vec<SummaryRow> {
    let Ok(mut statement) = connection.prepare(
        "SELECT resource, resource_id, category, metric, value, unit, scope
         FROM snapshot_summary
         ORDER BY resource, category, metric, resource_id;",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<f64>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(|row| {
        let (resource, resource_id, category, metric, value, unit, scope) = row.ok()?;
        Some(SummaryRow {
            resource,
            resource_id,
            category: UseCategory::parse(&category)?,
            metric,
            value,
            unit,
            scope,
        })
    })
    .collect()
}

fn load_findings(connection: &Connection) -> Vec<SnapshotFinding> {
    let Ok(mut statement) = connection.prepare(
        "SELECT severity, resource, finding, evidence, recommendation, quality
         FROM snapshot_findings ORDER BY \"rank\" ASC;",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok(SnapshotFinding {
            severity: Severity::parse(&row.get::<_, String>(0)?),
            resource: row.get(1)?,
            finding: row.get(2)?,
            evidence: row.get(3)?,
            recommendation: row.get(4)?,
            quality: row.get(5)?,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(|row| row.ok()).collect()
}

fn load_collectors(connection: &Connection) -> Vec<SnapshotCollector> {
    let Ok(mut statement) =
        connection.prepare("SELECT name, status, message FROM snapshot_collectors ORDER BY name;")
    else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok(SnapshotCollector {
            name: row.get(0)?,
            status: row.get(1)?,
            message: row.get(2)?,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(|row| row.ok()).collect()
}

const RESOURCE_ORDER: [&str; 8] = [
    "cpu", "gpu", "npu", "memory", "disk", "io", "network", "thermal",
];

/// Constant ceilings emitted every tick: they are the denominator of a meter,
/// never a measurement, so they get no chart and no summary row of their own.
fn is_ceiling_metric(metric: &str) -> bool {
    matches!(metric, "frequency_max" | "temperature_critical")
}

fn build_resources(
    samples: Vec<SampleRow>,
    summary_rows: &[SummaryRow],
    duration_ns: u64,
    logical_cpu_count: Option<u32>,
) -> Vec<ResourceUse> {
    let mut grouped =
        BTreeMap::<(String, UseCategory, String), BTreeMap<String, Vec<(u64, f64)>>>::new();
    let mut units = BTreeMap::<(String, UseCategory, String), (String, String)>::new();
    for sample in samples {
        if is_ceiling_metric(&sample.metric) {
            continue;
        }
        let key = (sample.resource, sample.category, sample.metric);
        units
            .entry(key.clone())
            .or_insert((sample.unit, sample.scope));
        grouped
            .entry(key)
            .or_default()
            .entry(sample.resource_id)
            .or_default()
            .push((sample.timestamp_ns, sample.value));
    }

    let mut charts_by_resource = BTreeMap::<String, Vec<SnapshotChart>>::new();
    for ((resource, category, metric), by_id) in grouped {
        let (unit, _) = units
            .remove(&(resource.clone(), category, metric.clone()))
            .unwrap_or_default();
        let kind = metric_kind(&metric, &unit);
        let mut series = by_id
            .into_iter()
            .filter_map(|(id, points)| build_series(id, points, kind, &unit))
            .collect::<Vec<_>>();
        if series.is_empty() {
            continue;
        }
        series.sort_by(|left, right| left.id.cmp(&right.id));
        let max_value = series
            .iter()
            .flat_map(|series| series.points.iter().map(|point| point.1))
            .fold(0.0_f64, f64::max);
        charts_by_resource
            .entry(resource)
            .or_default()
            .push(SnapshotChart {
                metric,
                category,
                unit: kind.display_unit(&unit),
                series,
                max_value,
            });
    }
    for charts in charts_by_resource.values_mut() {
        charts.sort_by(|left, right| {
            left.category
                .cmp(&right.category)
                .then_with(|| left.metric.cmp(&right.metric))
        });
    }

    let mut resources = charts_by_resource.keys().cloned().collect::<Vec<_>>();
    resources.sort_by_key(|resource| {
        RESOURCE_ORDER
            .iter()
            .position(|known| known == resource)
            .unwrap_or(RESOURCE_ORDER.len())
    });

    resources
        .into_iter()
        .map(|resource| {
            let charts = charts_by_resource.remove(&resource).unwrap_or_default();
            let summaries = summarize(summary_rows, &resource, duration_ns, logical_cpu_count);
            ResourceUse {
                headline: headline(
                    &resource,
                    &charts,
                    summary_rows,
                    duration_ns,
                    logical_cpu_count,
                ),
                summaries,
                charts,
                resource,
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum MetricKind {
    Gauge,
    /// Monotone counter charted as a per-second rate.
    Rate,
    /// Cumulative busy/CPU time charted as occupancy (time per wall time).
    Occupancy,
}

impl MetricKind {
    fn display_unit(self, unit: &str) -> String {
        match self {
            Self::Gauge => unit.to_string(),
            Self::Rate => format!("{unit}/s"),
            Self::Occupancy if unit == "seconds" => "cores".to_string(),
            Self::Occupancy => "%".to_string(),
        }
    }
}

fn metric_kind(metric: &str, unit: &str) -> MetricKind {
    match unit {
        "seconds" | "milliseconds" => MetricKind::Occupancy,
        "faults" | "switches" | "events" | "operations" => MetricKind::Rate,
        // Instantaneous readings, spelled out so a future unit special case
        // cannot silently turn a clock, a temperature or an engine occupancy
        // into a rate.
        "hertz" | "celsius" | "level" | "percent" => MetricKind::Gauge,
        "bytes"
            if ["read", "write", "receive", "transmit", "dram"]
                .iter()
                .any(|prefix| metric.contains(prefix)) =>
        {
            MetricKind::Rate
        }
        _ => MetricKind::Gauge,
    }
}

fn build_series(
    id: String,
    points: Vec<(u64, f64)>,
    kind: MetricKind,
    unit: &str,
) -> Option<ChartSeries> {
    // Milliseconds of busy time per second of wall time become percent;
    // seconds of CPU time per second of wall time stay as cores.
    let scale = match kind {
        MetricKind::Occupancy if unit == "milliseconds" => 0.1,
        _ => 1.0,
    };
    let converted: Vec<(f64, f64)> = match kind {
        MetricKind::Gauge => points
            .iter()
            .map(|(timestamp, value)| (*timestamp as f64 / 1e9, *value))
            .collect(),
        MetricKind::Rate | MetricKind::Occupancy => points
            .windows(2)
            .filter_map(|pair| {
                let dt = (pair[1].0 as f64 - pair[0].0 as f64) / 1e9;
                if dt <= 0.0 {
                    return None;
                }
                // Counter resets chart as zero instead of a negative spike.
                let delta = (pair[1].1 - pair[0].1).max(0.0);
                Some((pair[1].0 as f64 / 1e9, delta / dt * scale))
            })
            .collect(),
    };
    if converted.is_empty() {
        return None;
    }
    Some(ChartSeries {
        id,
        points: converted,
    })
}

fn summarize(
    summary_rows: &[SummaryRow],
    resource: &str,
    duration_ns: u64,
    logical_cpu_count: Option<u32>,
) -> [Vec<SummaryMetric>; 3] {
    let mut result: [Vec<SummaryMetric>; 3] = Default::default();
    for row in summary_rows
        .iter()
        .filter(|row| row.resource == resource && !is_ceiling_metric(&row.metric))
    {
        let index = UseCategory::ALL
            .iter()
            .position(|category| *category == row.category)
            .unwrap_or(0);
        result[index].push(SummaryMetric {
            metric: row.metric.clone(),
            entity: entity_label(summary_rows, row),
            value: row
                .value
                .map(|value| format_value(value, &row.unit))
                .unwrap_or_else(|| "—".to_string()),
            scope: row.scope.clone(),
            fraction: metric_fraction(row, summary_rows, duration_ns, logical_cpu_count),
        });
    }
    result
}

/// Names the entity a row measured, but only where the resource has several:
/// a host with one thermal zone gains nothing from labelling it.
fn entity_label(summary_rows: &[SummaryRow], row: &SummaryRow) -> Option<String> {
    summary_rows
        .iter()
        .filter(|other| other.resource == row.resource && other.metric == row.metric)
        .nth(1)
        .map(|_| row.resource_id.clone())
}

/// Normalizes a summary value into 0..=1 when its unit has a natural ceiling:
/// percentages, CPU time against the machine's capacity, byte gauges against
/// the host's total, clocks and temperatures against their recorded ceilings.
/// Everything else has no meaningful meter.
fn metric_fraction(
    row: &SummaryRow,
    summary_rows: &[SummaryRow],
    duration_ns: u64,
    logical_cpu_count: Option<u32>,
) -> Option<f64> {
    let value = row.value?;
    let fraction = match row.unit.as_str() {
        "percent" => value / 100.0,
        "seconds" if row.resource == "cpu" => {
            let capacity =
                (duration_ns as f64 / 1e9) * logical_cpu_count.unwrap_or(1).max(1) as f64;
            value / capacity
        }
        // `host_total` is published once for the machine, not per entity.
        "bytes" => value / ceiling(summary_rows, row, "host_total", false)?,
        "hertz" => value / ceiling(summary_rows, row, "frequency_max", true)?,
        "celsius" => value / ceiling(summary_rows, row, "temperature_critical", true)?,
        _ => return None,
    };
    fraction.is_finite().then(|| fraction.clamp(0.0, 1.0))
}

/// A metric's ceiling. Clocks and temperatures are only comparable within one
/// entity: a little core sitting at its own 1.8GHz ceiling is not throttled
/// because some big core on the same die can reach 2.6GHz.
fn ceiling(
    summary_rows: &[SummaryRow],
    row: &SummaryRow,
    metric: &str,
    per_entity: bool,
) -> Option<f64> {
    summary_rows
        .iter()
        .filter(|other| other.resource == row.resource && other.metric == metric)
        .filter(|other| !per_entity || other.resource_id == row.resource_id)
        .find_map(|other| other.value)
        .filter(|ceiling| *ceiling > 0.0)
}

fn summary_value(summary_rows: &[SummaryRow], resource: &str, metric: &str) -> Option<f64> {
    summary_rows
        .iter()
        .filter(|row| row.resource == resource && row.metric == metric)
        .filter_map(|row| row.value)
        .reduce(f64::max)
}

fn headline(
    resource: &str,
    charts: &[SnapshotChart],
    summary_rows: &[SummaryRow],
    duration_ns: u64,
    logical_cpu_count: Option<u32>,
) -> Option<String> {
    let duration_s = (duration_ns as f64 / 1e9).max(f64::EPSILON);
    match resource {
        "cpu" => {
            let logical = logical_cpu_count.unwrap_or(1).max(1) as f64;
            let load = summary_value(summary_rows, "cpu", "cgroup_cpu_time")
                .filter(|v| *v > 0.0)
                .or_else(|| {
                    Some(
                        summary_value(summary_rows, "cpu", "user_time").unwrap_or(0.0)
                            + summary_value(summary_rows, "cpu", "system_time").unwrap_or(0.0),
                    )
                })
                .map(|cpu_s| {
                    format!(
                        "{:.1}% of {} CPUs · {:.2} cores average",
                        cpu_s / duration_s / logical * 100.0,
                        logical as u64,
                        cpu_s / duration_s
                    )
                });
            joined([
                load,
                clock_part(charts, summary_rows),
                peak_temperature(charts),
            ])
        }
        "gpu" | "npu" => joined([
            hottest_series(charts, "frequency")
                .map(|(_, peak)| format!("{} peak", format_value(peak, "hertz"))),
            hottest_series(charts, "busy").map(|(_, busy)| format!("{busy:.0}% busy")),
            peak_temperature(charts),
        ]),
        "thermal" => {
            hottest_series(charts, "temperature").map(|(zone, peak)| {
                match summary_value(summary_rows, "thermal", "temperature_critical")
                    .filter(|critical| *critical > 0.0)
                {
                    Some(critical) => format!(
                        "peak {} on {zone} · {:.0}% of {} trip",
                        format_value(peak, "celsius"),
                        peak / critical * 100.0,
                        format_value(critical, "celsius")
                    ),
                    None => format!("peak {} on {zone}", format_value(peak, "celsius")),
                }
            })
        }
        "memory" => {
            let rss = summary_value(summary_rows, "memory", "pss")
                .into_iter()
                .chain(summary_value(summary_rows, "memory", "rss"))
                .fold(0.0_f64, f64::max);
            let host_total = summary_value(summary_rows, "memory", "host_total").unwrap_or(0.0);
            let footprint = (rss > 0.0).then(|| {
                if host_total > 0.0 {
                    format!(
                        "peak tree {} · {:.1}% of host {}",
                        format_value(rss, "bytes"),
                        rss / host_total * 100.0,
                        format_value(host_total, "bytes")
                    )
                } else {
                    format!("peak tree {}", format_value(rss, "bytes"))
                }
            });
            joined([footprint, peak_temperature(charts)])
        }
        "disk" => joined([
            hottest_series(charts, "busy_time")
                .map(|(device, busy)| format!("peak device busy {busy:.1}% ({device})")),
            peak_temperature(charts),
        ]),
        "network" => {
            let received = summary_value(summary_rows, "network", "receive_bytes").unwrap_or(0.0);
            let transmitted =
                summary_value(summary_rows, "network", "transmit_bytes").unwrap_or(0.0);
            (received + transmitted > 0.0).then(|| {
                format!(
                    "{}/s in · {}/s out average",
                    format_value(received / duration_s, "bytes"),
                    format_value(transmitted / duration_s, "bytes")
                )
            })
        }
        "io" => {
            let psi = summary_rows
                .iter()
                .filter(|row| row.resource == "io" && row.metric.contains("psi_some"))
                .filter_map(|row| row.value)
                .fold(0.0_f64, f64::max);
            Some(format!("peak I/O pressure {psi:.1}% (PSI some avg10)"))
        }
        _ => None,
    }
}

/// The series id and peak value of the busiest series of `metric`.
fn hottest_series<'a>(charts: &'a [SnapshotChart], metric: &str) -> Option<(&'a str, f64)> {
    charts
        .iter()
        .find(|chart| chart.metric == metric)?
        .series
        .iter()
        .map(|series| {
            (
                series.id.as_str(),
                series
                    .points
                    .iter()
                    .map(|point| point.1)
                    .fold(0.0_f64, f64::max),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
}

/// Joins the parts a headline has into one line, `None` when it has none.
fn joined<const N: usize>(parts: [Option<String>; N]) -> Option<String> {
    let parts = parts.into_iter().flatten().collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The hottest temperature this resource now owns, e.g. `71.0 °C`.
fn peak_temperature(charts: &[SnapshotChart]) -> Option<String> {
    hottest_series(charts, "temperature").map(|(_, peak)| format_value(peak, "celsius"))
}

/// `"2.94 GHz avg, 3.80 GHz peak"`, `None` when the run has no clock samples.
fn clock_part(charts: &[SnapshotChart], summary_rows: &[SummaryRow]) -> Option<String> {
    let chart = charts.iter().find(|chart| chart.metric == "frequency")?;
    let points = chart
        .series
        .iter()
        .flat_map(|series| series.points.iter().map(|point| point.1));
    let (sum, count) = points.fold((0.0, 0_u64), |(sum, count), value| (sum + value, count + 1));
    if count == 0 {
        return None;
    }
    let peak = summary_value(summary_rows, "cpu", "frequency_peak")
        .or_else(|| summary_value(summary_rows, "cpu", "frequency"))
        .unwrap_or_else(|| hottest_series(charts, "frequency").map_or(0.0, |(_, peak)| peak));
    Some(format!(
        "{} avg, {} peak",
        format_value(sum / count as f64, "hertz"),
        format_value(peak, "hertz")
    ))
}

/// Formats a raw metric value using its recorded unit.
pub fn format_value(value: f64, unit: &str) -> String {
    match unit {
        "bytes" => format_bytes(value),
        "bits_per_second" => format!("{:.1} Gbit/s", value / 1e9),
        "percent" => format!("{value:.1}%"),
        "hertz" => format_hertz(value),
        "celsius" => format!("{value:.1} °C"),
        "level" => format!("{value:.0}"),
        "seconds" => format!("{value:.1} s"),
        "milliseconds" => {
            if value >= 1_000.0 {
                format!("{:.1} s", value / 1_000.0)
            } else {
                format!("{value:.0} ms")
            }
        }
        _ => format_count(value),
    }
}

fn format_hertz(value: f64) -> String {
    if value.abs() >= 1e9 {
        format!("{:.2} GHz", value / 1e9)
    } else if value.abs() >= 1e6 {
        format!("{:.0} MHz", value / 1e6)
    } else {
        format!("{value:.0} Hz")
    }
}

pub fn format_bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = value;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_count(value: f64) -> String {
    if value.abs() >= 1e9 {
        format!("{:.2}G", value / 1e9)
    } else if value.abs() >= 1e6 {
        format!("{:.2}M", value / 1e6)
    } else if value.abs() >= 1e3 {
        format!("{:.1}k", value / 1e3)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}
