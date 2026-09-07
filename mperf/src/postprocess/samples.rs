use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kdam::BarExt;
use mperf_data::{CpuClockSource, RecordInfo, ScenarioInfo};
use smallvec::SmallVec;
use store::arrow::array::{Array, BinaryArray, Int64Array, ListArray, UInt32Array, UInt64Array};
use store::arrow::record_batch::RecordBatch;
use store::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use store::{SampleRows, SegmentWriter, StackRows};
use tokio::{fs::File, io::AsyncWriteExt};

use super::tables::{Columns, Tables};
use crate::utils;

/// A core cluster resolved for post-processing: `(family_id, display name,
/// inclusive CPU ranges)`.
type ClusterRanges = (String, String, Vec<(u32, u32)>);

type Frames = SmallVec<[u64; 32]>;

const BATCH_ROWS: usize = 8192;

/// One row of `samples_raw`, borrowed from the Arrow batch it was read from.
pub(crate) struct RawSample<'a> {
    pub timestamp: i64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub group_id: u64,
    pub event_id: u64,
    pub value: i64,
    pub time_enabled: u64,
    pub time_running: u64,
    pub ip: u64,
    pub callchain: &'a [u64],
    pub lbr_callchain: &'a [u64],
    pub regs_mask: u64,
    pub regs: &'a [u64],
    pub user_stack: &'a [u8],
}

impl<'a> RawSample<'a> {
    /// The stack capture libprof needs to walk this sample.
    pub(crate) fn stack(&self) -> libprof::StackSample<'a> {
        libprof::StackSample {
            pid: self.pid,
            group_id: self.group_id,
            regs_mask: self.regs_mask,
            regs: self.regs,
            user_stack: self.user_stack,
            callchain: self.callchain,
        }
    }
}

struct RawColumns<'a> {
    timestamp: &'a Int64Array,
    pid: &'a UInt32Array,
    tid: &'a UInt32Array,
    cpu: &'a UInt32Array,
    group_id: &'a UInt64Array,
    event_id: &'a UInt64Array,
    value: &'a Int64Array,
    time_enabled: &'a UInt64Array,
    time_running: &'a UInt64Array,
    ip: &'a UInt64Array,
    callchain: &'a ListArray,
    lbr_callchain: &'a ListArray,
    regs_mask: &'a UInt64Array,
    regs: &'a ListArray,
    user_stack: &'a BinaryArray,
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<T>())
        .with_context(|| format!("samples_raw is missing column '{name}'"))
}

impl<'a> RawColumns<'a> {
    fn new(batch: &'a RecordBatch) -> Result<Self> {
        Ok(RawColumns {
            timestamp: column(batch, "timestamp")?,
            pid: column(batch, "pid")?,
            tid: column(batch, "tid")?,
            cpu: column(batch, "cpu")?,
            group_id: column(batch, "group_id")?,
            event_id: column(batch, "event_id")?,
            value: column(batch, "value")?,
            time_enabled: column(batch, "time_enabled")?,
            time_running: column(batch, "time_running")?,
            ip: column(batch, "ip")?,
            callchain: column(batch, "callchain")?,
            lbr_callchain: column(batch, "lbr_callchain")?,
            regs_mask: column(batch, "regs_mask")?,
            regs: column(batch, "regs")?,
            user_stack: column(batch, "user_stack")?,
        })
    }
}

fn list_values(list: &ListArray, index: usize) -> Result<Vec<u64>> {
    let values = list.value(index);
    let values = values
        .as_any()
        .downcast_ref::<UInt64Array>()
        .context("expected a list of unsigned 64-bit values")?;
    Ok(values.values().to_vec())
}

#[derive(Clone)]
struct CounterLead {
    group_id: u64,
    process_id: u32,
    thread_id: u32,
    cpu: u32,
    time_enabled: u64,
    time_running: u64,
    timestamp: i64,
    frames: Frames,
}

#[derive(Clone)]
pub(crate) struct ResolvedIp {
    functions: Vec<String>,
    function: String,
    file: String,
    line: u32,
    module_path: Option<String>,
}

/// Column accumulator for `pmu_counters`, whose per-event columns depend on the
/// counters the scenario actually recorded.
struct PmuCounters {
    unique_id: Vec<u64>,
    process_id: Vec<i64>,
    thread_id: Vec<i64>,
    cpu: Vec<Option<i64>>,
    time_enabled: Vec<i64>,
    time_running: Vec<i64>,
    confidence: Vec<f64>,
    timestamp: Vec<i64>,
    ip: Vec<u64>,
    call_stack: Vec<String>,
    events: Vec<Vec<Option<i64>>>,
}

impl PmuCounters {
    fn new(event_count: usize) -> Self {
        PmuCounters {
            unique_id: Vec::new(),
            process_id: Vec::new(),
            thread_id: Vec::new(),
            cpu: Vec::new(),
            time_enabled: Vec::new(),
            time_running: Vec::new(),
            confidence: Vec::new(),
            timestamp: Vec::new(),
            ip: Vec::new(),
            call_stack: Vec::new(),
            events: vec![Vec::new(); event_count],
        }
    }

    fn push(
        &mut self,
        lead: &CounterLead,
        counters: &HashMap<String, i64>,
        event_columns: &[String],
        missing_is_null: bool,
    ) {
        if lead.frames.is_empty() || !counters.values().any(|value| *value != 0) {
            return;
        }
        if !event_columns
            .iter()
            .any(|column| counters.contains_key(column))
        {
            return;
        }

        let confidence = if lead.time_enabled > 0 {
            lead.time_running as f64 / lead.time_enabled as f64
        } else {
            0.0
        };
        self.unique_id.push(sample_identity(lead));
        self.process_id.push(lead.process_id as i64);
        self.thread_id.push(lead.thread_id as i64);
        self.cpu
            .push((lead.cpu != u32::MAX).then_some(lead.cpu as i64));
        self.time_enabled.push(lead.time_enabled as i64);
        self.time_running.push(lead.time_running as i64);
        self.confidence.push(confidence);
        self.timestamp.push(lead.timestamp);
        self.ip.push(lead.frames.first().copied().unwrap_or(0));
        self.call_stack.push(serialized_call_stack(&lead.frames));
        for (column, values) in event_columns.iter().zip(self.events.iter_mut()) {
            values.push(
                counters
                    .get(column)
                    .copied()
                    .or_else(|| (!missing_is_null).then_some(0)),
            );
        }
    }

    fn finish(self, event_columns: &[String]) -> Result<RecordBatch> {
        let mut columns = Columns::default();
        columns.u64("unique_id", self.unique_id);
        columns.i64("process_id", self.process_id);
        columns.i64("thread_id", self.thread_id);
        columns.i64_opt("cpu", self.cpu);
        columns.i64("time_enabled", self.time_enabled);
        columns.i64("time_running", self.time_running);
        columns.f64("confidence", self.confidence);
        columns.i64("timestamp", self.timestamp);
        columns.u64("ip", self.ip);
        columns.text("call_stack", self.call_stack);
        for (name, values) in event_columns.iter().zip(self.events) {
            columns.i64_opt(name, values);
        }
        columns.finish()
    }
}

/// The `unique_id` of a counter group. Record time no longer mints one, so it
/// is derived from the identity of the group's lead sample.
fn sample_identity(lead: &CounterLead) -> u64 {
    let mut bytes = [0u8; 24];
    bytes[..8].copy_from_slice(&lead.timestamp.to_le_bytes());
    bytes[8..12].copy_from_slice(&lead.process_id.to_le_bytes());
    bytes[12..16].copy_from_slice(&lead.thread_id.to_le_bytes());
    bytes[16..].copy_from_slice(&lead.group_id.to_le_bytes());
    store::xxh3(&bytes)
}

#[derive(Default)]
struct CpuObservations {
    process_id: Vec<i64>,
    thread_id: Vec<i64>,
    cpu: Vec<Option<i64>>,
    timestamp: Vec<i64>,
    interval_start_ns: Vec<Option<i64>>,
    weight_ns: Vec<i64>,
    source: Vec<String>,
    call_stack: Vec<String>,
}

impl CpuObservations {
    fn push(&mut self, lead: &CounterLead, counters: &HashMap<String, i64>, source: &str) {
        let Some(weight_ns) = counters
            .get("os_cpu_clock")
            .copied()
            .filter(|value| *value > 0)
        else {
            return;
        };
        self.process_id.push(lead.process_id as i64);
        self.thread_id.push(lead.thread_id as i64);
        self.cpu
            .push((lead.cpu != u32::MAX).then_some(lead.cpu as i64));
        self.timestamp.push(lead.timestamp);
        self.interval_start_ns.push(
            (source == CpuClockSource::CounterDelta.as_str() && lead.time_enabled > 0)
                .then_some(lead.timestamp.saturating_sub(lead.time_enabled as i64)),
        );
        self.weight_ns.push(weight_ns);
        self.source.push(source.to_owned());
        self.call_stack.push(serialized_call_stack(&lead.frames));
    }

    fn finish(self) -> Result<RecordBatch> {
        let mut columns = Columns::default();
        columns.i64("process_id", self.process_id);
        columns.i64("thread_id", self.thread_id);
        columns.i64_opt("cpu", self.cpu);
        columns.i64("timestamp", self.timestamp);
        columns.i64_opt("interval_start_ns", self.interval_start_ns);
        columns.i64("weight_ns", self.weight_ns);
        columns.text("source", self.source);
        columns.text("call_stack", self.call_stack);
        columns.finish()
    }
}

#[derive(Default)]
struct ProcMap {
    ip: Vec<u64>,
    func_name: Vec<String>,
    file_name: Vec<String>,
    line: Vec<i64>,
    module_path: Vec<Option<String>>,
}

impl ProcMap {
    fn push(&mut self, ip: u64, resolved: &ResolvedIp) {
        self.ip.push(ip);
        self.func_name.push(resolved.function.clone());
        self.file_name.push(resolved.file.clone());
        self.line.push(resolved.line as i64);
        self.module_path.push(resolved.module_path.clone());
    }

    fn finish(self) -> Result<RecordBatch> {
        let mut columns = Columns::default();
        columns.u64("ip", self.ip);
        columns.text("func_name", self.func_name);
        columns.text("file_name", self.file_name);
        columns.i64("line", self.line);
        columns.text_opt("module_path", self.module_path);
        columns.finish()
    }
}

/// Deduplicated call stacks shared by every sample of the session.
struct Stacks {
    rows: StackRows,
    seen: HashSet<u64>,
}

impl Stacks {
    fn intern(&mut self, frames: &[u64]) -> u64 {
        let stack_id = store::stack_hash(frames);
        if self.seen.insert(stack_id) {
            self.rows.stack_id.push(stack_id);
            self.rows.frames.push(frames.to_vec());
        }
        stack_id
    }
}

/// Record-time sample segments, in write order.
fn raw_segments(res_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(res_dir)? {
        let path = entry?.path();
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("samples_raw-") && name.ends_with(".parquet"));
        if is_segment {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Delete the record-time sample intermediate once the derived tables exist.
pub(crate) fn remove_raw_segments(tables: &Tables, res_dir: &Path) -> Result<()> {
    tables
        .connection()
        .execute_batch("DROP VIEW IF EXISTS samples_raw;")?;
    for path in raw_segments(res_dir)? {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Symbolize the recorded samples and materialize `proc_map`, `pmu_counters`,
/// `cpu_observations`, `samples`, `stacks` and the folded flamegraphs.
pub(crate) async fn process(
    tables: &Tables,
    record_info: &RecordInfo,
    res_dir: &Path,
    pb: &mut kdam::Bar,
) -> Result<()> {
    let info = &record_info.scenario_info;
    let events = match info {
        ScenarioInfo::Snapshot(s) => &s.counters,
        ScenarioInfo::Mem(m) => &m.counters,
        ScenarioInfo::Roofline(r) => &r.counters,
        ScenarioInfo::TMA(t) => &t.counters,
    };
    let missing_is_null = matches!(info, ScenarioInfo::TMA(_));

    let mut seen_columns = HashSet::new();
    let event_columns = events
        .iter()
        .map(super::event_column_name)
        .filter(|column| column != "pmu_unknown")
        .filter(|column| seen_columns.insert(column.clone()))
        .collect::<Vec<_>>();

    let modules = if tables.has_table("modules") {
        utils::load_modules(tables.connection())?
    } else {
        Vec::new()
    };
    let strings = if tables.has_table("strings") {
        utils::load_strings(tables.connection())?
    } else {
        HashMap::new()
    };
    let resolver = utils::resolve_proc_maps(&modules);
    let mut unwinder = libprof::PostHocUnwinder::new(&utils::proc_addrs(&modules));

    let clusters: Vec<ClusterRanges> = record_info
        .cores
        .iter()
        .cloned()
        .map(|c| (c.family_id, c.name, parse_cpumask(&c.cpus)))
        .collect();
    let cpu_clock_source = record_info
        .cpu_clock_source
        .map(CpuClockSource::as_str)
        .unwrap_or("legacy_unknown");

    let mut counters = PmuCounters::new(event_columns.len());
    let mut observations = CpuObservations::default();
    let mut proc_map = ProcMap::default();
    let mut stacks = Stacks {
        rows: StackRows::default(),
        seen: HashSet::new(),
    };
    let mut samples = SampleRows::default();
    let mut samples_writer = SegmentWriter::new(res_dir, "samples", None, SampleRows::schema());

    let mut column_names = HashMap::<u64, String>::new();
    let mut known_ips = HashSet::<u64>::new();
    let mut resolved_ips = HashMap::<(u32, u64), ResolvedIp>::new();
    let mut group = HashMap::<String, i64>::new();
    let mut lead: Option<CounterLead> = None;
    let mut folded_stack = String::new();

    let mut flamegraph_cycles = HashMap::<String, u64>::new();
    let mut flamegraph_instructions = HashMap::<String, u64>::new();
    // family_id -> (display name, folded stack -> value)
    let mut per_core_cycles = HashMap::<String, (String, HashMap<String, u64>)>::new();
    let mut per_core_instructions = HashMap::<String, (String, HashMap<String, u64>)>::new();

    let segments = raw_segments(res_dir)?;
    let mut readers = Vec::new();
    let mut total_rows = 0_usize;
    for path in &segments {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("failed to read {}", path.display()))?;
        total_rows += builder.metadata().file_metadata().num_rows().max(0) as usize;
        readers.push(builder.with_batch_size(BATCH_ROWS).build()?);
    }

    pb.reset(Some(total_rows));
    pb.write("Collecting hotspots")?;

    let mut scanned = 0_usize;
    for reader in readers {
        for batch in reader {
            let batch = batch?;
            let raw = RawColumns::new(&batch)?;
            for index in 0..batch.num_rows() {
                let callchain = list_values(raw.callchain, index)?;
                let lbr_callchain = list_values(raw.lbr_callchain, index)?;
                let regs = list_values(raw.regs, index)?;
                let sample = RawSample {
                    timestamp: raw.timestamp.value(index),
                    pid: raw.pid.value(index),
                    tid: raw.tid.value(index),
                    cpu: raw.cpu.value(index),
                    group_id: raw.group_id.value(index),
                    event_id: raw.event_id.value(index),
                    value: raw.value.value(index),
                    time_enabled: raw.time_enabled.value(index),
                    time_running: raw.time_running.value(index),
                    ip: raw.ip.value(index),
                    callchain: &callchain,
                    lbr_callchain: &lbr_callchain,
                    regs_mask: raw.regs_mask.value(index),
                    regs: &regs,
                    user_stack: raw.user_stack.value(index),
                };

                let frames = unwinder.resolve(&sample.stack());
                let frames = merge_lbr_stack(frames, sample.lbr_callchain);
                let stack_id = stacks.intern(&frames);
                samples.timestamp.push(sample.timestamp);
                samples.pid.push(sample.pid);
                samples.tid.push(sample.tid);
                samples.cpu.push(sample.cpu);
                samples.group_id.push(sample.group_id);
                samples.event_id.push(sample.event_id);
                samples.value.push(sample.value);
                samples.time_enabled.push(sample.time_enabled);
                samples.time_running.push(sample.time_running);
                samples.ip.push(sample.ip);
                samples.stack_id.push(stack_id);
                if samples.len() >= BATCH_ROWS {
                    samples_writer.write(&samples.to_batch()?)?;
                }

                if !resolver.has_process(sample.pid) {
                    continue;
                }

                if lead
                    .as_ref()
                    .is_none_or(|lead| sample.group_id != lead.group_id)
                {
                    if let Some(lead) = &lead {
                        observations.push(lead, &group, cpu_clock_source);
                        counters.push(lead, &group, &event_columns, missing_is_null);
                        group.clear();
                    }

                    folded_stack =
                        resolve_folded_stack(&resolver, &mut resolved_ips, sample.pid, &frames);

                    for ip in &frames {
                        if !known_ips.insert(*ip) {
                            continue;
                        }
                        let resolved = resolve_ip(&resolver, &mut resolved_ips, sample.pid, *ip);
                        proc_map.push(*ip, resolved);
                    }

                    lead = Some(CounterLead {
                        group_id: sample.group_id,
                        process_id: sample.pid,
                        thread_id: sample.tid,
                        cpu: sample.cpu,
                        time_enabled: sample.time_enabled,
                        time_running: sample.time_running,
                        timestamp: sample.timestamp,
                        frames: frames.clone(),
                    });
                }

                let column = column_names
                    .entry(sample.event_id)
                    .or_insert_with(|| {
                        super::event_column_name_for(
                            strings
                                .get(&sample.event_id)
                                .map(String::as_str)
                                .unwrap_or(""),
                        )
                    })
                    .clone();

                // Frequency sampling makes every delivered overflow one observation.
                // Do not weight it by the cumulative counter delta: after a lost or
                // throttled interval that delta spans many seconds and cannot be
                // attributed to the single IP which happens to arrive next.
                // Zero is the initial KPC baseline and is not an actual observation.
                if !folded_stack.is_empty()
                    && let Some(weight) = flamegraph_sample_weight(sample.value)
                {
                    let cluster = cluster_of(&clusters, sample.cpu);
                    if column == "pmu_cycles" {
                        *flamegraph_cycles.entry(folded_stack.clone()).or_default() += weight;
                        if let Some((family_id, name)) = cluster {
                            *per_core_cycles
                                .entry(family_id.to_owned())
                                .or_insert_with(|| (name.to_owned(), HashMap::new()))
                                .1
                                .entry(folded_stack.clone())
                                .or_default() += weight;
                        }
                    } else if column == "pmu_instructions" {
                        *flamegraph_instructions
                            .entry(folded_stack.clone())
                            .or_default() += weight;
                        if let Some((family_id, name)) = cluster {
                            *per_core_instructions
                                .entry(family_id.to_owned())
                                .or_insert_with(|| (name.to_owned(), HashMap::new()))
                                .1
                                .entry(folded_stack.clone())
                                .or_default() += weight;
                        }
                    }
                }

                group.insert(column, sample.value);
            }
            scanned += batch.num_rows();
            pb.update_to(scanned)?;
        }
    }

    if let Some(lead) = &lead {
        observations.push(lead, &group, cpu_clock_source);
        counters.push(lead, &group, &event_columns, missing_is_null);
    }
    pb.update_to(total_rows)?;

    if !samples.is_empty() {
        samples_writer.write(&samples.to_batch()?)?;
    }
    let sample_files = samples_writer.finish()?;
    if sample_files.is_empty() {
        tables.write("samples", samples.to_batch()?)?;
    } else {
        tables.register("samples", &sample_files)?;
    }
    tables.write("stacks", stacks.rows.to_batch()?)?;
    tables.write("proc_map", proc_map.finish()?)?;
    tables.write("pmu_counters", counters.finish(&event_columns)?)?;
    tables.write("cpu_observations", observations.finish()?)?;

    write_flamegraph(res_dir, "flamegraph_cycles", flamegraph_cycles).await?;
    write_flamegraph(res_dir, "flamegraph_instructions", flamegraph_instructions).await?;

    // Per-core flamegraphs on heterogeneous systems, e.g.
    // `flamegraph_cycles_cortex_a720.folded`.
    for (family_id, (_name, map)) in per_core_cycles {
        write_flamegraph(res_dir, &format!("flamegraph_cycles_{family_id}"), map).await?;
    }
    for (family_id, (_name, map)) in per_core_instructions {
        write_flamegraph(
            res_dir,
            &format!("flamegraph_instructions_{family_id}"),
            map,
        )
        .await?;
    }

    Ok(())
}

pub(crate) fn resolve_ip<'a>(
    resolver: &symbolize::Resolver,
    cache: &'a mut HashMap<(u32, u64), ResolvedIp>,
    pid: u32,
    ip: u64,
) -> &'a ResolvedIp {
    cache.entry((pid, ip)).or_insert_with(|| {
        let frames = resolver.resolve(pid, ip);
        let functions = if frames.is_empty() {
            vec!["[unknown]".to_owned()]
        } else {
            frames.iter().map(|frame| frame.function.clone()).collect()
        };
        let primary = frames.first();
        let module_path = primary
            .and_then(|frame| frame.module.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| {
                resolver
                    .module_path(pid, ip)
                    .map(|path| path.to_string_lossy().into_owned())
            });
        ResolvedIp {
            functions,
            function: primary
                .map(|frame| frame.function.clone())
                .unwrap_or_else(|| "[unknown]".to_owned()),
            file: primary
                .and_then(|frame| frame.file.clone())
                .unwrap_or_else(|| "unknown".to_owned()),
            line: primary.and_then(|frame| frame.line).unwrap_or_default(),
            module_path,
        }
    })
}

pub(crate) fn resolve_folded_stack(
    resolver: &symbolize::Resolver,
    cache: &mut HashMap<(u32, u64), ResolvedIp>,
    pid: u32,
    frames: &[u64],
) -> String {
    let mut functions = SmallVec::<[String; 32]>::new();
    for ip in frames.iter().rev() {
        let resolved = resolve_ip(resolver, cache, pid, *ip);
        functions.extend(resolved.functions.iter().rev().cloned());
    }
    functions.join(";")
}

/// Pick the better of the unwound stack and the branch-record (LBR) stack.
///
/// The unwound stack — kernel callchain or post-hoc DWARF — is authoritative
/// once it walked past the sampled frame; LBR only sees the last ~32 calls and
/// only in user space, so on a deeper call tree it is always rootless. It wins
/// only where it is deeper *and* demonstrably the same stack: the unwound
/// stack's outermost frame must be one of its call sites, i.e. the unwinder
/// gave up inside the window the hardware saw.
pub(crate) fn merge_lbr_stack(frames: Frames, lbr: &[u64]) -> Frames {
    if lbr.len() <= frames.len() {
        return frames;
    }
    match frames.last() {
        Some(outermost) if !lbr.iter().any(|site| is_call_site(*site, *outermost)) => frames,
        _ => Frames::from_slice(lbr),
    }
}

/// Whether `site` is the call whose return address is `ret`. Unwinders yield
/// return addresses while branch records yield the call instruction itself,
/// which sits at most one instruction (15 bytes on x86) before it.
fn is_call_site(site: u64, ret: u64) -> bool {
    ret.wrapping_sub(site) <= 15
}

fn flamegraph_sample_weight(counter_delta: i64) -> Option<u64> {
    (counter_delta != 0).then_some(1)
}

fn serialized_call_stack(frames: &[u64]) -> String {
    format!(
        "[{}]",
        frames
            .iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Parse a sysfs cpumask list such as `"0,5-11"` into inclusive `(start, end)`
/// ranges.
/// Expand a sysfs cpumask string such as `0,5-11` into inclusive CPU ranges.
pub(crate) fn parse_cpumask(mask: &str) -> Vec<(u32, u32)> {
    mask.trim()
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            match part.split_once('-') {
                Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
                None => {
                    let v: u32 = part.parse().ok()?;
                    Some((v, v))
                }
            }
        })
        .collect()
}

/// Find the `(family_id, name)` of the core cluster a CPU belongs to.
fn cluster_of(clusters: &[ClusterRanges], cpu: u32) -> Option<(&str, &str)> {
    if cpu == u32::MAX {
        return None;
    }
    clusters
        .iter()
        .find(|(_, _, ranges)| ranges.iter().any(|(a, b)| cpu >= *a && cpu <= *b))
        .map(|(family_id, name, _)| (family_id.as_str(), name.as_str()))
}

/// Write a folded stack collapse map to `<stem>.folded` and, when the map is
/// non-empty, render it to `<stem>.svg`.
async fn write_flamegraph(res_dir: &Path, stem: &str, map: HashMap<String, u64>) -> Result<()> {
    let lines = map
        .into_iter()
        .map(|(key, value)| format!("{} {}", key, value))
        .collect::<Vec<_>>();

    let mut folded = File::create(res_dir.join(format!("{stem}.folded"))).await?;
    for line in &lines {
        folded.write_all(line.as_bytes()).await?;
        folded.write_all(b"\n").await?;
    }

    // Some counters can legitimately have no positive samples (in particular
    // for short-lived processes or unavailable hardware events). Inferno treats
    // an empty input as an error, but that must not invalidate the recording or
    // prevent other counters from being persisted.
    if lines.is_empty() {
        return Ok(());
    }

    let mut options = inferno::flamegraph::Options::default();
    options.reverse_stack_order = false;
    let svg = std::fs::File::create(res_dir.join(format!("{stem}.svg")))?;
    inferno::flamegraph::from_lines(&mut options, lines.iter().map(|s| s.as_str()), &svg)?;

    Ok(())
}
