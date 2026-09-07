use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use mperf_data::{RecordInfo, ScenarioInfo};
use serde::Deserialize;

use super::tables::{Columns, Tables};

#[derive(Deserialize)]
struct MemoryArtifact {
    format_version: u32,
    line_size: u64,
    references: u64,
    architectural_load_bytes: u64,
    architectural_store_bytes: u64,
    unique_lines: u64,
    distinct_bytes: u64,
    cold_references: u64,
    reuse_distance_log2: BTreeMap<u32, u64>,
    spatial_utilization_percent: BTreeMap<u32, u64>,
    stride_lines_log2: BTreeMap<i32, u64>,
    working_set: Vec<MemoryWorkingSetArtifact>,
}

#[derive(Deserialize)]
struct MemoryWorkingSetArtifact {
    window_references: u64,
    mean_lines: f64,
    p95_lines: u64,
    max_lines: u64,
}

#[derive(Deserialize)]
struct NativeTimingArtifact {
    pid: i32,
    start_ns: u64,
    end_ns: u64,
}

pub(crate) struct BandwidthSample {
    pub(crate) timestamp: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

struct AllocationPoint {
    timestamp: u64,
    live_allocated: u64,
    live_mapped: u64,
}

struct AllocationSummary {
    live_allocated: u64,
    peak_allocated: u64,
    live_mapped: u64,
    points: Vec<AllocationPoint>,
    child_seen: bool,
}

#[derive(Default)]
struct Timeline {
    timestamp_ns: Vec<i64>,
    live_allocated_bytes: Vec<Option<i64>>,
    live_mapped_bytes: Vec<Option<i64>>,
    rss_bytes: Vec<Option<i64>>,
    dram_read: Vec<Option<f64>>,
    dram_write: Vec<Option<f64>>,
    bandwidth_source: Vec<Option<String>>,
}

#[derive(Default)]
struct TimelinePoint<'a> {
    timestamp: u64,
    allocated: Option<i64>,
    mapped: Option<i64>,
    rss: Option<i64>,
    read: Option<f64>,
    write: Option<f64>,
    source: Option<&'a str>,
}

impl Timeline {
    fn push(&mut self, point: TimelinePoint<'_>) {
        self.timestamp_ns.push(point.timestamp as i64);
        self.live_allocated_bytes.push(point.allocated);
        self.live_mapped_bytes.push(point.mapped);
        self.rss_bytes.push(point.rss);
        self.dram_read.push(point.read);
        self.dram_write.push(point.write);
        self.bandwidth_source.push(point.source.map(str::to_owned));
    }

    fn finish(self) -> Result<store::arrow::record_batch::RecordBatch> {
        let mut columns = Columns::default();
        columns.i64("timestamp_ns", self.timestamp_ns);
        columns.i64_opt("live_allocated_bytes", self.live_allocated_bytes);
        columns.i64_opt("live_mapped_bytes", self.live_mapped_bytes);
        columns.i64_opt("rss_bytes", self.rss_bytes);
        columns.f64_opt("dram_read_gbytes_per_second", self.dram_read);
        columns.f64_opt("dram_write_gbytes_per_second", self.dram_write);
        columns.text_opt("bandwidth_source", self.bandwidth_source);
        columns.finish()
    }
}

/// Materialize the memory tables from the QEMU model and native side files.
pub(crate) fn process(tables: &Tables, record_info: &RecordInfo, res_dir: &Path) -> Result<()> {
    let artifact_path = res_dir.join("qemu-roofline.memory.json");
    let artifact: MemoryArtifact = serde_json::from_reader(
        std::fs::File::open(&artifact_path)
            .with_context(|| format!("open memory artifact '{}'", artifact_path.display()))?,
    )
    .with_context(|| format!("parse memory artifact '{}'", artifact_path.display()))?;
    if artifact.format_version != 1 {
        anyhow::bail!(
            "unsupported memory artifact version {}",
            artifact.format_version
        );
    }
    let timing: NativeTimingArtifact = serde_json::from_reader(
        std::fs::File::open(res_dir.join("memory-native.json"))
            .context("open native memory timing artifact")?,
    )
    .context("parse native memory timing artifact")?;
    let duration_ns = timing.end_ns.saturating_sub(timing.start_ns);
    let counts = parse_counter_file(&res_dir.join("qemu-roofline.counts"))?;
    let modeled_load = counts.get("dram_bytes_load").copied().unwrap_or(0);
    let modeled_store = counts.get("dram_bytes_store").copied().unwrap_or(0);
    let modeled_total = modeled_load.saturating_add(modeled_store);
    let bandwidth_samples = parse_bandwidth_samples(&res_dir.join("memory-bandwidth.txt"))?;
    let hardware_total = bandwidth_samples
        .last()
        .map(|sample| sample.read_bytes.saturating_add(sample.write_bytes));
    let primary_total = hardware_total.unwrap_or(modeled_total);
    let (bandwidth_source, bandwidth_scope) = if hardware_total.is_some() {
        ("hardware_memory_controller", "system_during_target")
    } else {
        ("process_modeled", "process")
    };
    let achieved_gbytes_per_second =
        (duration_ns != 0).then(|| primary_total as f64 / duration_ns as f64);
    let calibration = record_info.cpu_info.memory_calibration.as_deref();
    let peak = calibration.map(|value| value.gbytes_per_second);
    let utilization = achieved_gbytes_per_second
        .zip(peak)
        .filter(|(_, peak)| *peak > 0.0)
        .map(|(achieved, peak)| achieved / peak);
    let cold_fraction = (artifact.references != 0)
        .then(|| artifact.cold_references as f64 / artifact.references as f64);

    let rss_samples = parse_rss_samples(&res_dir.join("memory-rss.txt"))?;
    let peak_rss = rss_samples.iter().map(|(_, rss)| *rss).max();
    let allocations = parse_allocation_samples(&res_dir.join("memory-allocations.txt"))?;
    let mut quality = match &record_info.scenario_info {
        ScenarioInfo::Mem(info) => info.method.quality.clone(),
        ScenarioInfo::Roofline(info) => info
            .method
            .as_deref()
            .map_or("legacy".to_owned(), |method| method.quality.clone()),
        _ => "unknown".to_owned(),
    };
    if allocations
        .as_ref()
        .is_some_and(|allocations| allocations.child_seen)
    {
        quality.push_str("+children-excluded");
    }

    let mut summary = Columns::default();
    summary.i64("format_version", vec![artifact.format_version as i64]);
    summary.i64("process_id", vec![timing.pid as i64]);
    summary.i64("line_size", vec![artifact.line_size as i64]);
    summary.i64("reference_count", vec![artifact.references as i64]);
    summary.i64(
        "architectural_load_bytes",
        vec![artifact.architectural_load_bytes as i64],
    );
    summary.i64(
        "architectural_store_bytes",
        vec![artifact.architectural_store_bytes as i64],
    );
    summary.i64("unique_lines", vec![artifact.unique_lines as i64]);
    summary.i64(
        "accessed_footprint_bytes",
        vec![artifact.distinct_bytes as i64],
    );
    summary.i64("cold_references", vec![artifact.cold_references as i64]);
    summary.f64_opt("cold_fraction", vec![cold_fraction]);
    summary.i64("modeled_dram_read_bytes", vec![modeled_load as i64]);
    summary.i64("modeled_dram_write_bytes", vec![modeled_store as i64]);
    summary.i64("native_duration_ns", vec![duration_ns as i64]);
    summary.f64_opt(
        "achieved_gbytes_per_second",
        vec![achieved_gbytes_per_second],
    );
    summary.f64_opt("peak_gbytes_per_second", vec![peak]);
    summary.f64_opt("bandwidth_utilization", vec![utilization]);
    summary.text("bandwidth_source", vec![bandwidth_source.to_owned()]);
    summary.text("bandwidth_scope", vec![bandwidth_scope.to_owned()]);
    summary.i64_opt(
        "live_allocated_bytes",
        vec![allocations.as_ref().map(|a| a.live_allocated as i64)],
    );
    summary.i64_opt(
        "peak_allocated_bytes",
        vec![allocations.as_ref().map(|a| a.peak_allocated as i64)],
    );
    summary.i64_opt(
        "live_mapped_bytes",
        vec![allocations.as_ref().map(|a| a.live_mapped as i64)],
    );
    summary.i64_opt("peak_rss_bytes", vec![peak_rss.map(|rss| rss as i64)]);
    summary.text("quality", vec![quality]);
    tables.write("memory_summary", summary.finish()?)?;

    let mut timeline = Timeline::default();
    for (timestamp, rss) in rss_samples {
        timeline.push(TimelinePoint {
            timestamp,
            rss: Some(rss as i64),
            ..TimelinePoint::default()
        });
    }
    for pair in bandwidth_samples.windows(2) {
        let elapsed = pair[1].timestamp.saturating_sub(pair[0].timestamp);
        if elapsed == 0 {
            continue;
        }
        let read = pair[1].read_bytes.saturating_sub(pair[0].read_bytes) as f64 / elapsed as f64;
        let write = pair[1].write_bytes.saturating_sub(pair[0].write_bytes) as f64 / elapsed as f64;
        timeline.push(TimelinePoint {
            timestamp: pair[1].timestamp,
            read: Some(read),
            write: Some(write),
            source: Some("hardware_memory_controller"),
            ..TimelinePoint::default()
        });
    }
    if let Some(allocations) = &allocations {
        for point in &allocations.points {
            timeline.push(TimelinePoint {
                timestamp: point.timestamp,
                allocated: Some(point.live_allocated as i64),
                mapped: Some(point.live_mapped as i64),
                ..TimelinePoint::default()
            });
        }
    }
    tables.write("memory_timeline", timeline.finish()?)?;

    let mut working_set = Columns::default();
    working_set.i64(
        "window_references",
        artifact
            .working_set
            .iter()
            .map(|window| window.window_references as i64)
            .collect(),
    );
    working_set.f64(
        "mean_bytes",
        artifact
            .working_set
            .iter()
            .map(|window| window.mean_lines * artifact.line_size as f64)
            .collect(),
    );
    working_set.i64(
        "p95_bytes",
        artifact
            .working_set
            .iter()
            .map(|window| window.p95_lines.saturating_mul(artifact.line_size) as i64)
            .collect(),
    );
    working_set.i64(
        "max_bytes",
        artifact
            .working_set
            .iter()
            .map(|window| window.max_lines.saturating_mul(artifact.line_size) as i64)
            .collect(),
    );
    tables.write("memory_working_set", working_set.finish()?)?;

    write_histogram(
        tables,
        "memory_spatial_utilization",
        "utilization_percent",
        "lines",
        artifact.spatial_utilization_percent,
    )?;
    write_histogram(
        tables,
        "memory_strides",
        "stride_log2_lines",
        "reference_count",
        artifact.stride_lines_log2,
    )?;
    write_histogram(
        tables,
        "memory_reuse_distance",
        "distance_log2_lines",
        "reference_count",
        artifact.reuse_distance_log2.clone(),
    )?;

    let mut miss = Columns::default();
    let powers = 0..=30_u32;
    let lines = powers
        .clone()
        .map(|power| 1_u64 << power)
        .collect::<Vec<_>>();
    miss.i64("cache_lines", lines.iter().map(|v| *v as i64).collect());
    miss.i64(
        "cache_bytes",
        lines
            .iter()
            .map(|v| v.saturating_mul(artifact.line_size) as i64)
            .collect(),
    );
    miss.f64(
        "miss_ratio",
        powers
            .map(|power| {
                let hits = artifact
                    .reuse_distance_log2
                    .iter()
                    .filter(|(bucket, _)| **bucket <= power)
                    .map(|(_, count)| *count)
                    .sum::<u64>();
                if artifact.references == 0 {
                    0.0
                } else {
                    1.0 - hits as f64 / artifact.references as f64
                }
            })
            .collect(),
    );
    tables.write("memory_miss_ratio", miss.finish()?)?;

    Ok(())
}

fn write_histogram<K>(
    tables: &Tables,
    table: &str,
    bucket_name: &str,
    count_name: &str,
    values: BTreeMap<K, u64>,
) -> Result<()>
where
    K: Into<i64> + Copy,
{
    let mut columns = Columns::default();
    columns.i64(
        bucket_name,
        values.keys().map(|bucket| (*bucket).into()).collect(),
    );
    columns.i64(
        count_name,
        values.values().map(|count| *count as i64).collect(),
    );
    tables.write(table, columns.finish()?)
}

fn parse_counter_file(path: &Path) -> Result<HashMap<String, u64>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read counter artifact '{}'", path.display()))?;
    contents
        .lines()
        .map(|line| {
            let (name, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("invalid counter line '{line}'"))?;
            Ok((name.to_string(), value.parse::<u64>()?))
        })
        .collect()
}

fn parse_rss_samples(path: &Path) -> Result<Vec<(u64, u64)>> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    contents
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let timestamp = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("RSS sample has no timestamp"))?
                .parse()?;
            let rss = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("RSS sample has no value"))?
                .parse()?;
            Ok((timestamp, rss))
        })
        .collect()
}

pub(crate) fn parse_bandwidth_samples(path: &Path) -> Result<Vec<BandwidthSample>> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    contents
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            Ok(BandwidthSample {
                timestamp: fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("bandwidth sample has no timestamp"))?
                    .parse()?,
                read_bytes: fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("bandwidth sample has no read count"))?
                    .parse()?,
                write_bytes: fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("bandwidth sample has no write count"))?
                    .parse()?,
            })
        })
        .collect()
}

fn parse_allocation_samples(path: &Path) -> Result<Option<AllocationSummary>> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut heap = HashMap::<u64, u64>::new();
    let mut mappings = BTreeMap::<u64, u64>::new();
    let mut live_allocated = 0_u64;
    let mut peak_allocated = 0_u64;
    let mut live_mapped = 0_u64;
    let mut points = Vec::new();
    let mut child_seen = false;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let operation = fields
            .next()
            .and_then(|value| value.as_bytes().first())
            .copied()
            .ok_or_else(|| anyhow::anyhow!("invalid allocation event '{line}'"))?;
        let timestamp = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("allocation event has no timestamp"))?
            .parse::<u64>()?;
        let first = u64::from_str_radix(
            fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("allocation event has no address"))?,
            16,
        )?;
        let second = u64::from_str_radix(
            fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("allocation event has no second address"))?,
            16,
        )?;
        let size = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("allocation event has no size"))?
            .parse::<u64>()?;
        match operation {
            b'A' if first != 0 => {
                if let Some(old) = heap.insert(first, size) {
                    live_allocated = live_allocated.saturating_sub(old);
                }
                live_allocated = live_allocated.saturating_add(size);
            }
            b'F' => {
                if let Some(old) = heap.remove(&first) {
                    live_allocated = live_allocated.saturating_sub(old);
                }
            }
            b'R' if second != 0 => {
                if let Some(old) = heap.remove(&first) {
                    live_allocated = live_allocated.saturating_sub(old);
                }
                if let Some(old) = heap.insert(second, size) {
                    live_allocated = live_allocated.saturating_sub(old);
                }
                live_allocated = live_allocated.saturating_add(size);
            }
            b'M' if first != 0 => {
                if let Some(old) = mappings.insert(first, size) {
                    live_mapped = live_mapped.saturating_sub(old);
                }
                live_mapped = live_mapped.saturating_add(size);
            }
            b'U' => unmap_range(&mut mappings, first, size, &mut live_mapped),
            b'C' => child_seen = true,
            _ => {}
        }
        peak_allocated = peak_allocated.max(live_allocated);
        points.push(AllocationPoint {
            timestamp,
            live_allocated,
            live_mapped,
        });
    }
    Ok(Some(AllocationSummary {
        live_allocated,
        peak_allocated,
        live_mapped,
        points,
        child_seen,
    }))
}

fn unmap_range(mappings: &mut BTreeMap<u64, u64>, start: u64, size: u64, live: &mut u64) {
    let end = start.saturating_add(size);
    let overlaps = mappings
        .range(..end)
        .filter_map(|(&base, &length)| {
            (base.saturating_add(length) > start).then_some((base, length))
        })
        .collect::<Vec<_>>();
    for (base, length) in overlaps {
        mappings.remove(&base);
        let mapping_end = base.saturating_add(length);
        let removed_start = base.max(start);
        let removed_end = mapping_end.min(end);
        *live = live.saturating_sub(removed_end.saturating_sub(removed_start));
        if base < start {
            mappings.insert(base, start - base);
        }
        if mapping_end > end {
            mappings.insert(end, mapping_end - end);
        }
    }
}
