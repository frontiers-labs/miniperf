use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use mperf_data::{EventType, ProcMapEntry, RecordInfo, ScenarioInfo};
use object::{Object, ObjectSegment};
use serde::Deserialize;
use store::EventKind;

use super::tables::{Columns, Tables};
use crate::utils;

#[derive(Default)]
struct RooflineLoopInfo {
    id: u64,
    pid: u32,
    tid: u32,
    file_name: u64,
    func_name: u64,
    line: u32,
    start: i64,
    bytes_load: i64,
    bytes_store: i64,
    scalar_int_ops: i64,
    scalar_float_ops: i64,
    scalar_double_ops: i64,
    vector_int_ops: i64,
    vector_float_ops: i64,
    vector_double_ops: i64,
}

struct Payload {
    function_id: u64,
    file_id: u64,
    line: u32,
}

/// One row of the recorded `events` table.
struct TraceEvent {
    timestamp: i64,
    event_id: u64,
    instance: u64,
    parent_id: u64,
    flow_id: u64,
    kind: u8,
    pid: u32,
    tid: u32,
    value: i64,
}

struct RooflineData {
    baseline_pid: i32,
    instrumented_pid: i32,
    loops: HashMap<u64, RooflineLoopInfo>,
    runs: Vec<(RooflineLoopInfo, i64)>,
    ops: Vec<RooflineLoopInfo>,
}

impl RooflineData {
    fn consume(
        &mut self,
        event: &TraceEvent,
        payloads: &HashMap<u64, Payload>,
        names: &HashMap<u64, String>,
    ) -> Result<()> {
        if event.kind == EventKind::Begin as u8 {
            let payload = payloads.get(&event.event_id).ok_or_else(|| {
                anyhow::anyhow!("roofline loop start has no payload {}", event.event_id)
            })?;
            self.loops.insert(
                event.instance,
                RooflineLoopInfo {
                    id: event.instance,
                    pid: event.pid,
                    tid: event.tid,
                    file_name: payload.file_id,
                    func_name: payload.function_id,
                    line: payload.line,
                    start: event.timestamp,
                    ..RooflineLoopInfo::default()
                },
            );
            return Ok(());
        }
        if event.kind == EventKind::End as u8 {
            let loop_info = self.loops.remove(&event.flow_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "roofline loop end references unknown loop {}",
                    event.flow_id
                )
            })?;
            if event.pid as i32 == self.baseline_pid {
                self.runs.push((loop_info, event.timestamp));
            } else if event.pid as i32 == self.instrumented_pid {
                self.ops.push(loop_info);
            }
            return Ok(());
        }
        if event.kind != EventKind::Counter as u8 {
            return Ok(());
        }
        let name = names.get(&event.event_id).map(String::as_str).unwrap_or("");
        let Some(ty) = utils::event_type_from_name(name) else {
            return Ok(());
        };
        let loop_info = self.loops.get_mut(&event.parent_id).ok_or_else(|| {
            anyhow::anyhow!(
                "roofline event references unknown parent {}",
                event.parent_id
            )
        })?;
        match ty {
            EventType::RooflineBytesLoad => loop_info.bytes_load = event.value,
            EventType::RooflineBytesStore => loop_info.bytes_store = event.value,
            EventType::RooflineScalarIntOps => loop_info.scalar_int_ops = event.value,
            EventType::RooflineScalarFloatOps => loop_info.scalar_float_ops = event.value,
            EventType::RooflineScalarDoubleOps => loop_info.scalar_double_ops = event.value,
            EventType::RooflineVectorIntOps => loop_info.vector_int_ops = event.value,
            EventType::RooflineVectorFloatOps => loop_info.vector_float_ops = event.value,
            EventType::RooflineVectorDoubleOps => loop_info.vector_double_ops = event.value,
            _ => {}
        }
        Ok(())
    }
}

/// Cumulative system-wide DRAM bytes recorded during the timed run, as a
/// step-free curve that can be integrated over an arbitrary time window.
struct BandwidthTimeline {
    samples: Vec<(u64, u64)>,
}

impl BandwidthTimeline {
    fn load(res_dir: &Path) -> Result<Option<Self>> {
        let samples =
            super::memory::parse_bandwidth_samples(&res_dir.join("memory-bandwidth.txt"))?
                .into_iter()
                .map(|sample| {
                    (
                        sample.timestamp,
                        sample.read_bytes.saturating_add(sample.write_bytes),
                    )
                })
                .collect::<Vec<_>>();
        Ok((samples.len() >= 2).then_some(Self { samples }))
    }

    fn cumulative_at(&self, timestamp: u64) -> f64 {
        match self
            .samples
            .binary_search_by_key(&timestamp, |(time, _)| *time)
        {
            Ok(index) => self.samples[index].1 as f64,
            Err(0) => self.samples[0].1 as f64,
            Err(index) if index == self.samples.len() => {
                self.samples[self.samples.len() - 1].1 as f64
            }
            Err(index) => {
                let (before_time, before_bytes) = self.samples[index - 1];
                let (after_time, after_bytes) = self.samples[index];
                let span = after_time.saturating_sub(before_time) as f64;
                let fraction = if span == 0.0 {
                    0.0
                } else {
                    (timestamp - before_time) as f64 / span
                };
                before_bytes as f64 + (after_bytes - before_bytes) as f64 * fraction
            }
        }
    }

    /// Bytes observed inside `intervals`, or `None` when nothing was observed
    /// and a measured denominator would be meaningless.
    fn bytes_in(&self, intervals: &[(u64, u64)]) -> Option<i64> {
        let bytes: f64 = intervals
            .iter()
            .map(|(start, end)| self.cumulative_at(*end) - self.cumulative_at(*start))
            .sum();
        (bytes >= 1.0).then_some(bytes as i64)
    }
}

/// Derived Roofline tables: the raw compiler-backend tables plus
/// `roofline_loops`, one row per loop from whichever backend measured it.
pub(crate) fn process(tables: &Tables, record_info: &RecordInfo, res_dir: &Path) -> Result<()> {
    let ScenarioInfo::Roofline(info) = &record_info.scenario_info else {
        return Ok(());
    };
    let (data, names) = load_instrumented_loops(tables, info)?;
    write_instrumented_tables(tables, &data, res_dir)?;
    let mut rows = Vec::new();
    match binary_loop_artifact(info, res_dir)? {
        Some(artifact) => {
            collect_binary_loops(tables, info, record_info, artifact, res_dir, &mut rows)?
        }
        None => collect_instrumented_loops(&data, &names, res_dir, &mut rows)?,
    }
    tables.write("roofline_loops", loop_columns(rows)?)
}

fn load_instrumented_loops(
    tables: &Tables,
    info: &mperf_data::RooflineInfo,
) -> Result<(RooflineData, HashMap<u64, String>)> {
    let mut data = RooflineData {
        baseline_pid: info.perf_pid,
        instrumented_pid: info.inst_pid,
        loops: HashMap::new(),
        runs: Vec::new(),
        ops: Vec::new(),
    };
    if !tables.has_table("events") {
        return Ok((data, HashMap::new()));
    }
    let names = utils::load_strings(tables.connection())?;
    let payloads = load_payloads(tables)?;
    let mut statement = tables.connection().prepare(
        "SELECT timestamp, event_id, instance, parent_id, flow_id, \"type\", pid, tid, value
         FROM events",
    )?;
    let events = statement
        .query_map([], |row| {
            Ok(TraceEvent {
                timestamp: row.get(0)?,
                event_id: row.get(1)?,
                instance: row.get(2)?,
                parent_id: row.get(3)?,
                flow_id: row.get(4)?,
                kind: row.get(5)?,
                pid: row.get(6)?,
                tid: row.get(7)?,
                value: row.get(8)?,
            })
        })?
        .collect::<store::duckdb::Result<Vec<_>>>()?;
    for event in &events {
        data.consume(event, &payloads, &names)?;
    }
    Ok((data, names))
}

/// `roofline_loop_runs` and `roofline_ops`: one row per instrumented loop
/// execution, keyed by string ids.
fn write_instrumented_tables(tables: &Tables, data: &RooflineData, res_dir: &Path) -> Result<()> {
    let mut runs = Columns::default();
    runs.u64(
        "unique_id",
        data.runs.iter().map(|(run, _)| run.id).collect(),
    );
    runs.i64(
        "process_id",
        data.runs.iter().map(|(run, _)| run.pid as i64).collect(),
    );
    runs.i64(
        "thread_id",
        data.runs.iter().map(|(run, _)| run.tid as i64).collect(),
    );
    runs.u64(
        "file_name",
        data.runs.iter().map(|(run, _)| run.file_name).collect(),
    );
    runs.u64(
        "function_name",
        data.runs.iter().map(|(run, _)| run.func_name).collect(),
    );
    runs.i64(
        "line",
        data.runs.iter().map(|(run, _)| run.line as i64).collect(),
    );
    runs.i64(
        "loop_start_ts",
        data.runs.iter().map(|(run, _)| run.start).collect(),
    );
    runs.i64(
        "loop_end_ts",
        data.runs.iter().map(|(_, end)| *end).collect(),
    );
    let bandwidth = BandwidthTimeline::load(res_dir)?;
    runs.i64_opt(
        "measured_dram_bytes",
        data.runs
            .iter()
            .map(|(run, end)| {
                bandwidth
                    .as_ref()
                    .and_then(|timeline| timeline.bytes_in(&[(run.start as u64, *end as u64)]))
            })
            .collect(),
    );
    tables.write("roofline_loop_runs", runs.finish()?)?;

    let mut ops = Columns::default();
    ops.u64("unique_id", data.ops.iter().map(|op| op.id).collect());
    ops.i64(
        "process_id",
        data.ops.iter().map(|op| op.pid as i64).collect(),
    );
    ops.i64(
        "thread_id",
        data.ops.iter().map(|op| op.tid as i64).collect(),
    );
    ops.u64(
        "file_name",
        data.ops.iter().map(|op| op.file_name).collect(),
    );
    ops.u64(
        "function_name",
        data.ops.iter().map(|op| op.func_name).collect(),
    );
    ops.i64("line", data.ops.iter().map(|op| op.line as i64).collect());
    ops.i64(
        "bytes_load",
        data.ops.iter().map(|op| op.bytes_load).collect(),
    );
    ops.i64(
        "bytes_store",
        data.ops.iter().map(|op| op.bytes_store).collect(),
    );
    ops.i64(
        "scalar_int_ops",
        data.ops.iter().map(|op| op.scalar_int_ops).collect(),
    );
    ops.i64(
        "scalar_float_ops",
        data.ops.iter().map(|op| op.scalar_float_ops).collect(),
    );
    ops.i64(
        "scalar_double_ops",
        data.ops.iter().map(|op| op.scalar_double_ops).collect(),
    );
    ops.i64(
        "vector_int_ops",
        data.ops.iter().map(|op| op.vector_int_ops).collect(),
    );
    ops.i64(
        "vector_float_ops",
        data.ops.iter().map(|op| op.vector_float_ops).collect(),
    );
    ops.i64(
        "vector_double_ops",
        data.ops.iter().map(|op| op.vector_double_ops).collect(),
    );
    tables.write("roofline_ops", ops.finish()?)
}

type LoopKey = (u64, u64, u32);
type ThreadInterval = (u32, u64, u64);

/// One `roofline_loops` row per instrumented source loop: the timed run's
/// loop executions from every thread give its occupancy, the instrumented
/// run's counters give its work.
fn collect_instrumented_loops(
    data: &RooflineData,
    names: &HashMap<u64, String>,
    res_dir: &Path,
    rows: &mut Vec<LoopRow>,
) -> Result<()> {
    let bandwidth = BandwidthTimeline::load(res_dir)?;
    let key = |info: &RooflineLoopInfo| (info.file_name, info.func_name, info.line);
    let mut runs: BTreeMap<LoopKey, Vec<ThreadInterval>> = BTreeMap::new();
    for (run, end) in &data.runs {
        runs.entry(key(run)).or_default().push((
            run.tid,
            run.start.max(0) as u64,
            (*end).max(0) as u64,
        ));
    }
    let mut ops: BTreeMap<LoopKey, LoopCounts> = BTreeMap::new();
    for op in &data.ops {
        let counts = ops.entry(key(op)).or_default();
        let add = |total: &mut u64, value: i64| *total = total.saturating_add(value.max(0) as u64);
        add(&mut counts.arch_bytes_load, op.bytes_load);
        add(&mut counts.arch_bytes_store, op.bytes_store);
        add(&mut counts.scalar_int_ops, op.scalar_int_ops);
        add(&mut counts.scalar_float_ops, op.scalar_float_ops);
        add(&mut counts.scalar_double_ops, op.scalar_double_ops);
        add(&mut counts.vector_int_ops, op.vector_int_ops);
        add(&mut counts.vector_float_ops, op.vector_float_ops);
        add(&mut counts.vector_double_ops, op.vector_double_ops);
    }
    for (key, intervals) in runs {
        let samples = intervals.len() as u64;
        let timing = LoopTiming::from_intervals(intervals);
        let name = |id: u64| names.get(&id).cloned();
        rows.push(LoopRow {
            module_offset: None,
            function_name: name(key.1).unwrap_or_else(|| "[unknown loop]".to_owned()),
            file_name: name(key.0).unwrap_or_default(),
            line: key.2 as i64,
            trip_count: None,
            duration_ns: Some(timing.wall_ns()),
            cpu_time_ns: Some(timing.cpu_ns),
            thread_count: timing.threads,
            timing_samples: samples,
            timing_relative_error: None,
            timing_quality: "instrumented",
            measured_dram_bytes: bandwidth
                .as_ref()
                .and_then(|timeline| timeline.bytes_in(&timing.wall)),
            counts: ops.remove(&key).unwrap_or_default(),
        });
    }
    Ok(())
}

fn load_payloads(tables: &Tables) -> Result<HashMap<u64, Payload>> {
    if !tables.has_table("payloads") {
        return Ok(HashMap::new());
    }
    let mut statement = tables
        .connection()
        .prepare("SELECT event_id, function_id, file_id, line FROM payloads")?;
    let payloads = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                Payload {
                    function_id: row.get(1)?,
                    file_id: row.get(2)?,
                    line: row.get(3)?,
                },
            ))
        })?
        .collect::<store::duckdb::Result<HashMap<_, _>>>()?;
    Ok(payloads)
}

#[derive(Deserialize)]
struct BinaryLoopFile {
    format_version: u32,
    executable: String,
    loops: Vec<BinaryLoop>,
}

#[derive(Deserialize)]
struct BinaryLoop {
    module_offset: String,
    function: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    trip_count: u64,
    block_ranges: Vec<BinaryBlockRange>,
    inclusive: LoopCounts,
}

#[derive(Deserialize)]
struct BinaryBlockRange {
    module_start: String,
    module_end: String,
}

#[derive(Default, Deserialize)]
struct LoopCounts {
    scalar_int_ops: u64,
    scalar_float_ops: u64,
    scalar_double_ops: u64,
    vector_int_ops: u64,
    vector_float_ops: u64,
    vector_double_ops: u64,
    bytes_load: u64,
    bytes_store: u64,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
    unclassified_instructions: u64,
}

struct ModuleMapping {
    runtime_start: u64,
    runtime_end: u64,
    svma_start: u64,
}

struct BinaryTimingSample {
    timestamp: u64,
    thread: u32,
    module_address: u64,
}

fn binary_loop_artifact(
    roofline_info: &mperf_data::RooflineInfo,
    res_dir: &Path,
) -> Result<Option<BinaryLoopFile>> {
    let Some(method) = roofline_info.method.as_deref() else {
        return Ok(None);
    };
    if !matches!(method.accounting.as_str(), "qemu" | "dynamorio") || method.performance != "native"
    {
        return Ok(None);
    }
    let artifact_path = res_dir.join("qemu-roofline.loops.json");
    if !artifact_path.is_file() {
        anyhow::bail!(
            "native QEMU Roofline recording is missing '{}'",
            artifact_path.display()
        );
    }
    let artifact: BinaryLoopFile = serde_json::from_reader(
        std::fs::File::open(&artifact_path)
            .with_context(|| format!("open binary loop artifact '{}'", artifact_path.display()))?,
    )
    .with_context(|| format!("parse binary loop artifact '{}'", artifact_path.display()))?;
    if artifact.format_version != 3 {
        anyhow::bail!(
            "unsupported binary loop artifact version {}",
            artifact.format_version
        );
    }
    Ok(Some(artifact))
}

/// One row of `roofline_loops`.
struct LoopRow {
    module_offset: Option<String>,
    function_name: String,
    file_name: String,
    line: i64,
    trip_count: Option<u64>,
    duration_ns: Option<u64>,
    cpu_time_ns: Option<u64>,
    thread_count: u32,
    timing_samples: u64,
    timing_relative_error: Option<f64>,
    timing_quality: &'static str,
    measured_dram_bytes: Option<i64>,
    counts: LoopCounts,
}

fn loop_columns(rows: Vec<LoopRow>) -> Result<store::arrow::record_batch::RecordBatch> {
    let mut columns = Columns::default();
    let column = |pick: fn(&LoopRow) -> i64| rows.iter().map(pick).collect::<Vec<_>>();
    columns.text_opt(
        "module_offset",
        rows.iter().map(|row| row.module_offset.clone()).collect(),
    );
    columns.text(
        "function_name",
        rows.iter().map(|row| row.function_name.clone()).collect(),
    );
    columns.text(
        "file_name",
        rows.iter().map(|row| row.file_name.clone()).collect(),
    );
    columns.i64("line", column(|row| row.line));
    columns.i64_opt(
        "trip_count",
        rows.iter()
            .map(|row| row.trip_count.map(|v| v as i64))
            .collect(),
    );
    columns.i64_opt(
        "duration_ns",
        rows.iter()
            .map(|row| row.duration_ns.map(|v| v as i64))
            .collect(),
    );
    columns.i64_opt(
        "cpu_time_ns",
        rows.iter()
            .map(|row| row.cpu_time_ns.map(|v| v as i64))
            .collect(),
    );
    columns.i64("thread_count", column(|row| row.thread_count as i64));
    columns.i64("timing_samples", column(|row| row.timing_samples as i64));
    columns.f64_opt(
        "timing_relative_error",
        rows.iter().map(|row| row.timing_relative_error).collect(),
    );
    columns.text(
        "timing_quality",
        rows.iter()
            .map(|row| row.timing_quality.to_owned())
            .collect(),
    );
    columns.i64_opt(
        "measured_dram_bytes",
        rows.iter().map(|row| row.measured_dram_bytes).collect(),
    );
    type Pick = fn(&LoopCounts) -> u64;
    let counters: [(&str, Pick); 10] = [
        ("bytes_load", |c| c.bytes_load),
        ("bytes_store", |c| c.bytes_store),
        ("arch_bytes_load", |c| c.arch_bytes_load),
        ("arch_bytes_store", |c| c.arch_bytes_store),
        ("scalar_int_ops", |c| c.scalar_int_ops),
        ("scalar_float_ops", |c| c.scalar_float_ops),
        ("scalar_double_ops", |c| c.scalar_double_ops),
        ("vector_int_ops", |c| c.vector_int_ops),
        ("vector_float_ops", |c| c.vector_float_ops),
        ("vector_double_ops", |c| c.vector_double_ops),
    ];
    for (name, pick) in counters {
        columns.i64(
            name,
            rows.iter().map(|row| pick(&row.counts) as i64).collect(),
        );
    }
    columns.finish()
}

/// Attribute native sample time to the loops discovered by the binary backends.
fn collect_binary_loops(
    tables: &Tables,
    roofline_info: &mperf_data::RooflineInfo,
    record_info: &RecordInfo,
    artifact: BinaryLoopFile,
    res_dir: &Path,
    rows: &mut Vec<LoopRow>,
) -> Result<()> {
    let executable = Path::new(&artifact.executable);
    let object_data = std::fs::read(executable)
        .with_context(|| format!("read Roofline executable '{}'", executable.display()))?;
    let object = object::File::parse(object_data.as_slice())
        .with_context(|| format!("parse Roofline executable '{}'", executable.display()))?;
    let modules = utils::load_modules(tables.connection())?;
    let mappings =
        executable_mappings(&modules, roofline_info.perf_pid as u32, executable, &object);
    if mappings.is_empty() {
        anyhow::bail!(
            "native Roofline samples have no executable mapping for '{}'",
            executable.display()
        );
    }

    let mut samples = Vec::new();
    if tables
        .columns("pmu_counters")
        .iter()
        .any(|column| column == "os_cpu_clock")
    {
        let mut statement = tables.connection().prepare(
            "SELECT timestamp, thread_id, ip FROM pmu_counters
             WHERE process_id = ? AND os_cpu_clock > 0 AND ip != 0
             ORDER BY timestamp",
        )?;
        let timings = statement
            .query_map([roofline_info.perf_pid as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })?
            .collect::<store::duckdb::Result<Vec<_>>>()?;
        for (timestamp, thread, ip) in timings {
            if let Some(module_address) = normalize_sample_ip(ip, &mappings) {
                samples.push(BinaryTimingSample {
                    timestamp: timestamp.max(0) as u64,
                    thread: thread.max(0) as u32,
                    module_address,
                });
            }
        }
    }

    let bandwidth = BandwidthTimeline::load(res_dir)?;
    let sample_period_ns = 1_000_000_000_u64
        .checked_div(record_info.sampling_frequency_hz.unwrap_or(1_000).max(1))
        .unwrap_or(1_000_000);

    for loop_info in artifact.loops {
        let ranges = loop_info
            .block_ranges
            .iter()
            .map(|range| {
                Ok((
                    parse_hex_address(&range.module_start)?,
                    parse_hex_address(&range.module_end)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let observations = samples
            .iter()
            .filter(|sample| {
                ranges.iter().any(|(start, end)| {
                    sample.module_address >= *start && sample.module_address < *end
                })
            })
            .map(|sample| {
                (
                    sample.thread,
                    sample.timestamp.saturating_sub(sample_period_ns),
                    sample.timestamp,
                )
            })
            .collect::<Vec<_>>();
        let sample_count = observations.len() as u64;
        let timing = LoopTiming::from_intervals(observations);
        // Each sampling period of wall-clock occupancy is one independent
        // observation; concurrent threads' samples overlap and do not add
        // evidence. The 95% relative error is approximately 1.96/sqrt(N),
        // and only rows at or below 10% produce a throughput point.
        let relative_error = (sample_count > 0).then(|| {
            let slots = (timing.wall_ns() / sample_period_ns).max(1);
            1.96 / (slots as f64).sqrt()
        });
        let quality = match (
            loop_info.inclusive.unclassified_instructions == 0,
            relative_error,
        ) {
            (false, _) => "unclassified-instructions",
            (true, Some(error)) if error <= 0.10 => "high-confidence",
            (true, Some(error)) if error <= 0.20 => "low-confidence",
            _ => "insufficient-samples",
        };
        let timed = quality == "high-confidence";
        rows.push(LoopRow {
            module_offset: Some(loop_info.module_offset),
            function_name: loop_info
                .function
                .unwrap_or_else(|| "[unknown loop]".to_owned()),
            file_name: loop_info.file.unwrap_or_default(),
            line: loop_info.line.unwrap_or_default() as i64,
            trip_count: Some(loop_info.trip_count),
            duration_ns: timed.then(|| timing.wall_ns()),
            cpu_time_ns: timed.then_some(timing.cpu_ns),
            thread_count: timing.threads,
            timing_samples: sample_count,
            timing_relative_error: relative_error,
            timing_quality: quality,
            measured_dram_bytes: bandwidth
                .as_ref()
                .and_then(|timeline| timeline.bytes_in(&timing.wall)),
            counts: loop_info.inclusive,
        });
    }

    Ok(())
}

fn executable_mappings<'data>(
    modules: &[ProcMapEntry],
    pid: u32,
    executable: &Path,
    object: &object::File<'data>,
) -> Vec<ModuleMapping> {
    modules
        .iter()
        .filter(|mapping| mapping.pid == pid)
        .filter(|mapping| paths_refer_to_same_file(Path::new(&mapping.filename), executable))
        .filter_map(|mapping| {
            let mapping_offset = mapping.offset as u64;
            let segment = object
                .segments()
                .filter(|segment| {
                    let (file_offset, file_size) = segment.file_range();
                    mapping_offset >= (file_offset & !0xfff)
                        && mapping_offset < file_offset.saturating_add(file_size)
                })
                .min_by_key(|segment| segment.file_range().0.abs_diff(mapping_offset))?;
            let (file_offset, _) = segment.file_range();
            Some(ModuleMapping {
                runtime_start: mapping.address as u64,
                runtime_end: mapping.address.saturating_add(mapping.size) as u64,
                svma_start: segment
                    .address()
                    .saturating_add(mapping_offset)
                    .saturating_sub(file_offset),
            })
        })
        .collect()
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    let left_text = left.to_string_lossy();
    let left = Path::new(
        left_text
            .strip_suffix(" (deleted)")
            .unwrap_or(left_text.as_ref()),
    );
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn normalize_sample_ip(ip: u64, mappings: &[ModuleMapping]) -> Option<u64> {
    mappings
        .iter()
        .find(|mapping| ip >= mapping.runtime_start && ip < mapping.runtime_end)
        .map(|mapping| {
            mapping
                .svma_start
                .saturating_add(ip.saturating_sub(mapping.runtime_start))
        })
}

/// Occupancy of one loop across the threads that ran it.
struct LoopTiming {
    /// Union of every thread's active windows, sorted and disjoint.
    wall: Vec<(u64, u64)>,
    /// Sum of each thread's own active time.
    cpu_ns: u64,
    threads: u32,
}

impl LoopTiming {
    fn from_intervals(intervals: impl IntoIterator<Item = ThreadInterval>) -> Self {
        let mut by_thread: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
        for (thread, start, end) in intervals {
            by_thread.entry(thread).or_default().push((start, end));
        }
        let mut cpu_ns = 0_u64;
        let mut all = Vec::new();
        for windows in by_thread.values_mut() {
            let merged = merge_intervals(std::mem::take(windows));
            cpu_ns = cpu_ns.saturating_add(total_duration(&merged));
            all.extend(merged);
        }
        Self {
            wall: merge_intervals(all),
            cpu_ns,
            threads: by_thread.len() as u32,
        }
    }

    fn wall_ns(&self) -> u64 {
        total_duration(&self.wall)
    }
}

fn merge_intervals(mut windows: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    windows.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in windows {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn total_duration(intervals: &[(u64, u64)]) -> u64 {
    intervals.iter().fold(0_u64, |total, (start, end)| {
        total.saturating_add(end.saturating_sub(*start))
    })
}

fn parse_hex_address(value: &str) -> Result<u64> {
    let value = value
        .strip_prefix("0x")
        .with_context(|| format!("binary loop address is not hexadecimal: '{value}'"))?;
    u64::from_str_radix(value, 16)
        .with_context(|| format!("invalid binary loop address '0x{value}'"))
}

/// The Roofline chart table: one row per loop in `roofline_loops`, with
/// aggregate operation rates over wall-clock occupancy and arithmetic
/// intensity against the best available traffic denominator.
pub(crate) fn write_chart(tables: &Tables) -> Result<()> {
    let kinds = [
        "scalar_int",
        "scalar_float",
        "scalar_double",
        "vector_int",
        "vector_float",
        "vector_double",
    ];
    let rates = kinds
        .iter()
        .map(|kind| {
            format!(
                "CAST({kind}_ops AS DOUBLE) * 1000000000.0 / NULLIF(duration_ns, 0) AS {kind}_ops,
                 CAST({kind}_ops AS DOUBLE) / NULLIF(COALESCE(measured_dram_bytes, arch_bytes_load + arch_bytes_store), 0) AS {kind}_ai"
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    tables.write_query(
        "roofline",
        &format!(
            "SELECT
               file_name,
               function_name,
               line,
               {rates},
               timing_samples,
               timing_relative_error,
               timing_quality,
               module_offset,
               trip_count,
               arch_bytes_load + arch_bytes_store AS arch_bytes,
               NULLIF(bytes_load + bytes_store, 0) AS dram_bytes,
               measured_dram_bytes,
               CASE WHEN measured_dram_bytes IS NULL THEN 'architectural'
                    ELSE 'uncore_measured' END AS traffic_source,
               duration_ns,
               cpu_time_ns,
               thread_count
             FROM roofline_loops"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_timeline_integrates_over_kernel_windows() {
        let timeline = BandwidthTimeline {
            samples: vec![(0, 0), (1_000, 1_000), (2_000, 5_000)],
        };
        assert_eq!(timeline.bytes_in(&[(0, 2_000)]), Some(5_000));
        assert_eq!(timeline.bytes_in(&[(1_000, 1_500)]), Some(2_000));
        // Windows outside the recorded curve contribute nothing.
        assert_eq!(timeline.bytes_in(&[(3_000, 4_000)]), None);
    }

    fn row(measured: Option<i64>) -> LoopRow {
        LoopRow {
            module_offset: Some("0x100".to_owned()),
            function_name: "kernel".to_owned(),
            file_name: "kernel.c".to_owned(),
            line: 7,
            trip_count: Some(100),
            duration_ns: Some(1_000_000_000),
            cpu_time_ns: Some(4_000_000_000),
            thread_count: 4,
            timing_samples: 400,
            timing_relative_error: Some(0.098),
            timing_quality: "high-confidence",
            measured_dram_bytes: measured,
            counts: LoopCounts {
                bytes_load: 800,
                bytes_store: 200,
                arch_bytes_load: 4000,
                arch_bytes_store: 1000,
                scalar_double_ops: 2_000_000_000,
                vector_double_ops: 6_000_000_000,
                ..LoopCounts::default()
            },
        }
    }

    /// The single chart row produced for one loop with 5000 architectural
    /// bytes, 1000 modeled DRAM bytes, and `measured` bytes counted on the
    /// memory controller.
    fn chart_row(rows: Vec<LoopRow>) -> (String, f64, f64, String, Option<i64>, String, i64, i64) {
        let dir = tempfile::tempdir().unwrap();
        let tables = Tables::open(dir.path()).unwrap();
        tables
            .write("roofline_loops", loop_columns(rows).unwrap())
            .unwrap();
        write_chart(&tables).unwrap();
        tables
            .connection()
            .query_row(
                "SELECT function_name, scalar_double_ai, vector_double_ai, timing_quality,
                        measured_dram_bytes, traffic_source, thread_count, cpu_time_ns
                 FROM roofline",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap()
    }

    #[test]
    fn chart_uses_architectural_bytes_and_carries_thread_columns() {
        let (function, scalar_ai, vector_ai, quality, measured, source, threads, cpu) =
            chart_row(vec![row(None)]);
        assert_eq!(function, "kernel");
        // Without measured traffic the denominator is the 5000 architectural
        // bytes, not the 1000 bytes the model says reached DRAM.
        assert_eq!(scalar_ai, 400_000.0);
        assert_eq!(vector_ai, 1_200_000.0);
        assert_eq!(quality, "high-confidence");
        assert_eq!(measured, None);
        assert_eq!(source, "architectural");
        assert_eq!(threads, 4);
        assert_eq!(cpu, 4_000_000_000);
    }

    #[test]
    fn measured_dram_bytes_become_the_intensity_denominator() {
        let (_, scalar_ai, vector_ai, _, measured, source, _, _) =
            chart_row(vec![row(Some(2_000))]);
        assert_eq!(scalar_ai, 1_000_000.0);
        assert_eq!(vector_ai, 3_000_000.0);
        assert_eq!(measured, Some(2_000));
        assert_eq!(source, "uncore_measured");
    }

    #[test]
    fn concurrent_threads_share_wall_time_but_add_cpu_time() {
        let timing = LoopTiming::from_intervals([
            (1, 0, 1_000),
            (2, 0, 1_000),
            (1, 1_000, 2_000),
            (2, 500, 1_500),
        ]);
        assert_eq!(timing.wall_ns(), 2_000);
        assert_eq!(timing.cpu_ns, 3_500);
        assert_eq!(timing.threads, 2);

        let staggered = LoopTiming::from_intervals([(1, 0, 1_000), (2, 3_000, 4_000)]);
        assert_eq!(staggered.wall_ns(), 2_000);
        assert_eq!(staggered.cpu_ns, 2_000);

        let none = LoopTiming::from_intervals([]);
        assert_eq!(none.wall_ns(), 0);
        assert_eq!(none.threads, 0);
    }

    #[test]
    fn instrumented_loops_aggregate_across_threads() {
        let loop_info = |tid: u32, start: i64, scalar_double_ops: i64| RooflineLoopInfo {
            tid,
            file_name: 1,
            func_name: 2,
            line: 9,
            start,
            bytes_load: 100,
            scalar_double_ops,
            ..RooflineLoopInfo::default()
        };
        let data = RooflineData {
            baseline_pid: 1,
            instrumented_pid: 2,
            loops: HashMap::new(),
            runs: vec![
                (loop_info(10, 0, 0), 1_000),
                (loop_info(11, 0, 0), 1_000),
                (loop_info(10, 2_000, 0), 3_000),
            ],
            ops: vec![loop_info(20, 0, 50), loop_info(21, 0, 70)],
        };
        let names = HashMap::from([(1, "k.c".to_owned()), (2, "kernel".to_owned())]);
        let mut rows = Vec::new();
        collect_instrumented_loops(&data, &names, Path::new("/nonexistent"), &mut rows).unwrap();

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.function_name, "kernel");
        assert_eq!(row.file_name, "k.c");
        assert_eq!(row.duration_ns, Some(2_000));
        assert_eq!(row.cpu_time_ns, Some(3_000));
        assert_eq!(row.thread_count, 2);
        assert_eq!(row.timing_samples, 3);
        assert_eq!(row.timing_quality, "instrumented");
        assert_eq!(row.counts.scalar_double_ops, 120);
        assert_eq!(row.counts.arch_bytes_load, 200);
        assert_eq!(row.counts.bytes_load, 0);
    }
}
