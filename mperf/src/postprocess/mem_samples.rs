use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mperf_data::{MemDataSource, MemLevel, MemSnoop};
use store::arrow::array::{Array, BinaryArray, Int64Array, ListArray, UInt32Array, UInt64Array};
use store::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use super::samples::{RawSample, ResolvedIp, merge_lbr_stack, resolve_folded_stack};
use super::tables::{Columns, Tables};
use crate::utils;

const BATCH_ROWS: usize = 8192;

/// Allocation live ranges plus the frames of each site that produced them.
type AllocationMap = (Vec<Allocation>, HashMap<u64, Vec<u64>>);

/// A live heap allocation: an address range over a time window, attributed to
/// the call stack that requested it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation {
    pub start: u64,
    pub end: u64,
    pub from_ns: i64,
    pub to_ns: i64,
    pub site: u64,
}

/// For every `(address, timestamp)` probe, the index of the allocation that was
/// live at that address and time, if any.
///
/// Sweeps both sides in address order with a heap of currently-spanning
/// allocations, so the cost is `O((n + m) log n)` rather than a nested scan.
pub(crate) fn join_allocations(
    allocations: &[Allocation],
    probes: &[(u64, i64)],
) -> Vec<Option<usize>> {
    let mut by_start: Vec<usize> = (0..allocations.len()).collect();
    by_start.sort_unstable_by_key(|index| allocations[*index].start);
    let mut by_address: Vec<usize> = (0..probes.len()).collect();
    by_address.sort_unstable_by_key(|index| probes[*index].0);

    let mut matches = vec![None; probes.len()];
    let mut active = BinaryHeap::<Reverse<(u64, usize)>>::new();
    let mut next = 0;

    for probe_index in by_address {
        let (address, time) = probes[probe_index];
        while next < by_start.len() && allocations[by_start[next]].start <= address {
            let allocation = &allocations[by_start[next]];
            active.push(Reverse((allocation.end, by_start[next])));
            next += 1;
        }
        while active
            .peek()
            .is_some_and(|Reverse((end, _))| *end <= address)
        {
            active.pop();
        }
        // The heap holds every allocation spanning this address across the whole
        // recording; address reuse keeps that set tiny, so picking the one live
        // at this instant is a short scan.
        matches[probe_index] = active
            .iter()
            .map(|Reverse((_, index))| *index)
            .find(|index| {
                let allocation = &allocations[*index];
                allocation.from_ns <= time && time < allocation.to_ns
            });
    }

    matches
}

/// One symbolized precise memory sample.
struct Sample {
    timestamp: i64,
    pid: u32,
    tid: u32,
    cpu: u32,
    ip: u64,
    data_addr: u64,
    latency: u64,
    source: MemDataSource,
}

/// Materialize `mem_samples` plus the derived `alloc_site_memory` and
/// `cacheline_contention` tables. A no-op unless the recording ran at the
/// precise-memory rung.
pub(crate) fn process(tables: &Tables, res_dir: &Path) -> Result<()> {
    let segments = raw_segments(res_dir)?;
    if segments.is_empty() {
        return Ok(());
    }

    let modules = if tables.has_table("modules") {
        utils::load_modules(tables.connection())?
    } else {
        Vec::new()
    };
    let resolver = utils::resolve_proc_maps(&modules);
    let mut unwinder = libprof::PostHocUnwinder::new(&utils::proc_addrs(&modules));
    let mut resolved_ips = HashMap::<(u32, u64), ResolvedIp>::new();

    let mut samples = Vec::new();
    let mut call_stacks = Vec::new();
    let mut stack_ids = Vec::new();
    for path in &segments {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("failed to read {}", path.display()))?
            .with_batch_size(BATCH_ROWS)
            .build()?;
        for batch in reader {
            let batch = batch?;
            let timestamp = column::<Int64Array>(&batch, "timestamp")?;
            let pid = column::<UInt32Array>(&batch, "pid")?;
            let tid = column::<UInt32Array>(&batch, "tid")?;
            let cpu = column::<UInt32Array>(&batch, "cpu")?;
            let ip = column::<UInt64Array>(&batch, "ip")?;
            let data_addr = column::<UInt64Array>(&batch, "data_addr")?;
            let latency = column::<UInt64Array>(&batch, "latency")?;
            let data_src = column::<UInt64Array>(&batch, "data_src")?;
            let callchain = column::<ListArray>(&batch, "callchain")?;
            let lbr_callchain = column::<ListArray>(&batch, "lbr_callchain")?;
            let regs_mask = column::<UInt64Array>(&batch, "regs_mask")?;
            let regs = column::<ListArray>(&batch, "regs")?;
            let user_stack = column::<BinaryArray>(&batch, "user_stack")?;

            for index in 0..batch.num_rows() {
                let chain = list_values(callchain, index)?;
                let lbr_chain = list_values(lbr_callchain, index)?;
                let registers = list_values(regs, index)?;
                let raw = RawSample {
                    timestamp: timestamp.value(index),
                    pid: pid.value(index),
                    tid: tid.value(index),
                    cpu: cpu.value(index),
                    group_id: 0,
                    event_id: 0,
                    value: 0,
                    time_enabled: 0,
                    time_running: 0,
                    ip: ip.value(index),
                    callchain: &chain,
                    lbr_callchain: &lbr_chain,
                    regs_mask: regs_mask.value(index),
                    regs: &registers,
                    user_stack: user_stack.value(index),
                };
                let frames = unwinder.resolve(&raw.stack());
                let frames = merge_lbr_stack(frames.into_iter().collect(), &lbr_chain);
                stack_ids.push(store::stack_hash(&frames));
                call_stacks.push(resolve_folded_stack(
                    &resolver,
                    &mut resolved_ips,
                    raw.pid,
                    &frames,
                ));
                samples.push(Sample {
                    timestamp: raw.timestamp,
                    pid: raw.pid,
                    tid: raw.tid,
                    cpu: raw.cpu,
                    ip: raw.ip,
                    data_addr: data_addr.value(index),
                    latency: latency.value(index),
                    source: MemDataSource::from_perf(data_src.value(index)),
                });
            }
        }
    }

    write_mem_samples(tables, &samples, stack_ids, call_stacks)?;
    write_alloc_site_memory(tables, res_dir, &samples, &resolver, &mut resolved_ips)?;
    write_cacheline_contention(tables)?;

    tables
        .connection()
        .execute_batch("DROP VIEW IF EXISTS mem_samples_raw;")?;
    for path in segments {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn write_mem_samples(
    tables: &Tables,
    samples: &[Sample],
    stack_ids: Vec<u64>,
    call_stacks: Vec<String>,
) -> Result<()> {
    let mut columns = Columns::default();
    columns.i64("timestamp", samples.iter().map(|s| s.timestamp).collect());
    columns.i64("pid", samples.iter().map(|s| s.pid as i64).collect());
    columns.i64("tid", samples.iter().map(|s| s.tid as i64).collect());
    columns.i64("cpu", samples.iter().map(|s| s.cpu as i64).collect());
    columns.u64("ip", samples.iter().map(|s| s.ip).collect());
    columns.u64("stack_id", stack_ids);
    columns.text("call_stack", call_stacks);
    columns.u64("data_addr", samples.iter().map(|s| s.data_addr).collect());
    columns.u64(
        "cache_line",
        samples.iter().map(|s| s.data_addr >> 6).collect(),
    );
    columns.i64(
        "latency_cycles",
        samples.iter().map(|s| s.latency as i64).collect(),
    );
    columns.text(
        "op",
        samples
            .iter()
            .map(|s| s.source.op.as_str().to_owned())
            .collect(),
    );
    columns.text(
        "level",
        samples
            .iter()
            .map(|s| s.source.level.as_str().to_owned())
            .collect(),
    );
    columns.text(
        "hit_miss",
        samples
            .iter()
            .map(|s| match s.source.hit {
                Some(true) => "hit".to_owned(),
                Some(false) => "miss".to_owned(),
                None => "unknown".to_owned(),
            })
            .collect(),
    );
    columns.text(
        "snoop",
        samples
            .iter()
            .map(|s| s.source.snoop.as_str().to_owned())
            .collect(),
    );
    columns.text(
        "tlb",
        samples
            .iter()
            .map(|s| s.source.tlb.as_str().to_owned())
            .collect(),
    );
    columns.i64(
        "remote",
        samples.iter().map(|s| s.source.remote as i64).collect(),
    );
    columns.i64(
        "locked",
        samples.iter().map(|s| s.source.locked as i64).collect(),
    );
    tables.write("mem_samples", columns.finish()?)
}

/// Per allocation-site memory behaviour: the flagship join of precise samples
/// against the libc shim's live allocation ranges.
fn write_alloc_site_memory(
    tables: &Tables,
    res_dir: &Path,
    samples: &[Sample],
    resolver: &symbolize::Resolver,
    resolved_ips: &mut HashMap<(u32, u64), ResolvedIp>,
) -> Result<()> {
    let (allocations, sites) = load_allocations(tables, res_dir)?;
    let pid = samples.first().map(|sample| sample.pid).unwrap_or_default();
    let probes: Vec<(u64, i64)> = samples
        .iter()
        .map(|sample| (sample.data_addr, sample.timestamp))
        .collect();
    let matches = join_allocations(&allocations, &probes);

    #[derive(Default)]
    struct SiteStats {
        allocations: i64,
        allocated_bytes: i64,
        samples: i64,
        misses: i64,
        hitm: i64,
        levels: HashMap<&'static str, i64>,
        latencies: Vec<u64>,
    }

    let mut stats = HashMap::<u64, SiteStats>::new();
    for allocation in &allocations {
        let entry = stats.entry(allocation.site).or_default();
        entry.allocations += 1;
        entry.allocated_bytes += allocation.end.saturating_sub(allocation.start) as i64;
    }
    for (sample, matched) in samples.iter().zip(&matches) {
        let Some(allocation) = matched.map(|index| &allocations[index]) else {
            continue;
        };
        let entry = stats.entry(allocation.site).or_default();
        entry.samples += 1;
        entry.misses += i64::from(sample.source.hit == Some(false));
        entry.hitm += i64::from(sample.source.snoop == MemSnoop::Hitm);
        *entry
            .levels
            .entry(sample.source.level.as_str())
            .or_default() += 1;
        entry.latencies.push(sample.latency);
    }

    let mut rows: Vec<(u64, SiteStats)> = stats.into_iter().collect();
    rows.sort_unstable_by_key(|(site, entry)| {
        (Reverse(entry.latencies.iter().sum::<u64>()), *site)
    });

    let level_count = |entry: &SiteStats, level: MemLevel| {
        entry
            .levels
            .get(level.as_str())
            .copied()
            .unwrap_or_default()
    };
    let mut columns = Columns::default();
    columns.text(
        "alloc_site",
        rows.iter()
            .map(|(site, _)| {
                sites
                    .get(site)
                    .map(|frames| resolve_folded_stack(resolver, resolved_ips, pid, frames))
                    .unwrap_or_else(|| "[unknown]".to_owned())
            })
            .collect(),
    );
    columns.u64(
        "alloc_stack_id",
        rows.iter().map(|(site, _)| *site).collect(),
    );
    columns.i64(
        "allocation_count",
        rows.iter().map(|(_, e)| e.allocations).collect(),
    );
    columns.i64(
        "allocated_bytes",
        rows.iter().map(|(_, e)| e.allocated_bytes).collect(),
    );
    columns.i64(
        "sample_count",
        rows.iter().map(|(_, e)| e.samples).collect(),
    );
    columns.i64("miss_count", rows.iter().map(|(_, e)| e.misses).collect());
    columns.i64("hitm_count", rows.iter().map(|(_, e)| e.hitm).collect());
    for level in [MemLevel::L1, MemLevel::L2, MemLevel::L3, MemLevel::Ram] {
        columns.i64(
            &format!("{}_count", level.as_str()),
            rows.iter().map(|(_, e)| level_count(e, level)).collect(),
        );
    }
    columns.f64_opt(
        "avg_latency_cycles",
        rows.iter().map(|(_, e)| mean(&e.latencies)).collect(),
    );
    columns.i64_opt(
        "p95_latency_cycles",
        rows.iter()
            .map(|(_, e)| percentile(&e.latencies, 0.95).map(|value| value as i64))
            .collect(),
    );
    tables.write("alloc_site_memory", columns.finish()?)
}

fn mean(values: &[u64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<u64>() as f64 / values.len() as f64)
}

fn percentile(values: &[u64], fraction: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 * fraction).ceil() as usize).clamp(1, sorted.len()) - 1;
    Some(sorted[index])
}

/// Cache lines touched by more than one thread, ordered by how strongly they
/// look like false sharing.
fn write_cacheline_contention(tables: &Tables) -> Result<()> {
    tables.write_query(
        "cacheline_contention",
        "SELECT cache_line,
                COUNT(*)::BIGINT AS sample_count,
                COUNT(DISTINCT tid)::BIGINT AS distinct_threads,
                COUNT(DISTINCT cpu)::BIGINT AS distinct_cpus,
                SUM(CASE WHEN snoop = 'hitm' THEN 1 ELSE 0 END)::BIGINT AS hitm_count,
                AVG(latency_cycles) AS avg_latency_cycles,
                MAX(latency_cycles)::BIGINT AS max_latency_cycles
         FROM mem_samples
         GROUP BY cache_line
         HAVING COUNT(DISTINCT tid) > 1 OR SUM(CASE WHEN snoop = 'hitm' THEN 1 ELSE 0 END) > 0
         ORDER BY hitm_count DESC, distinct_threads DESC, sample_count DESC",
    )
}

/// Live allocation ranges from the libc shim's sampled `malloc`/`free` trace
/// points: `flow_id` carries the pointer and the attached stack identifies the
/// site. The unthrottled `memory-allocations.txt` stream carries no call stack,
/// so it cannot attribute anything to a site.
fn load_allocations(tables: &Tables, res_dir: &Path) -> Result<AllocationMap> {
    if !tables.has_table("events") || !tables.has_table("payloads") {
        return Ok((Vec::new(), HashMap::new()));
    }
    // Stack metadata is only present when the collector captured frames; without
    // it every allocation still joins, under a single unattributed site.
    let stack_id = if tables.has_table("event_meta") {
        "COALESCE(m.value_int, 0)"
    } else {
        "0"
    };
    let stack_join = if tables.has_table("event_meta") {
        "LEFT JOIN event_meta m
           ON m.event_instance = e.instance
          AND m.key_id = (SELECT id FROM strings WHERE string = 'stack_id' LIMIT 1)"
    } else {
        ""
    };

    let connection = tables.connection();
    let mut statement = connection.prepare(&format!(
        "SELECT e.timestamp, e.flow_id, e.value, s.string, {stack_id}
         FROM events e
         JOIN payloads p ON p.event_id = e.event_id
         JOIN strings s ON s.id = p.name_id
         {stack_join}
         WHERE e.\"type\" = 2 AND s.string IN ('malloc', 'free') AND e.flow_id <> 0
         ORDER BY e.timestamp"
    ))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut open = HashMap::<u64, (i64, i64, u64)>::new();
    let mut allocations = Vec::new();
    for (timestamp, pointer, size, name, stack_id) in rows {
        if name == "malloc" {
            if let Some((start, size, site)) =
                open.insert(pointer, (timestamp, size, stack_id as u64))
            {
                allocations.push(Allocation {
                    start: pointer,
                    end: pointer.saturating_add(size.max(0) as u64),
                    from_ns: start,
                    to_ns: timestamp,
                    site,
                });
            }
        } else if let Some((start, size, site)) = open.remove(&pointer) {
            allocations.push(Allocation {
                start: pointer,
                end: pointer.saturating_add(size.max(0) as u64),
                from_ns: start,
                to_ns: timestamp,
                site,
            });
        }
    }
    for (pointer, (start, size, site)) in open {
        allocations.push(Allocation {
            start: pointer,
            end: pointer.saturating_add(size.max(0) as u64),
            from_ns: start,
            to_ns: i64::MAX,
            site,
        });
    }

    Ok((allocations, collector_stacks(res_dir)?))
}

/// Call stacks captured inside the profiled process by the collector, keyed by
/// stack id. `samples` postprocessing replaces the shared `stacks` view with
/// the PMU stacks alone, so read the collector's segments straight from disk.
fn collector_stacks(res_dir: &Path) -> Result<HashMap<u64, Vec<u64>>> {
    let mut stacks = HashMap::new();
    for entry in std::fs::read_dir(res_dir)? {
        let path = entry?.path();
        let is_collector_stack = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("stacks-") && name.ends_with(".parquet"));
        if !is_collector_stack {
            continue;
        }
        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("failed to read {}", path.display()))?
            .with_batch_size(BATCH_ROWS)
            .build()?;
        for batch in reader {
            let batch = batch?;
            let ids = column::<UInt64Array>(&batch, "stack_id")?;
            let frames = column::<ListArray>(&batch, "frames")?;
            for index in 0..batch.num_rows() {
                stacks.insert(ids.value(index), list_values(frames, index)?);
            }
        }
    }
    Ok(stacks)
}

fn raw_segments(res_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(res_dir)? {
        let path = entry?.path();
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("mem_samples_raw-") && name.ends_with(".parquet"));
        if is_segment {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn column<'a, T: 'static>(
    batch: &'a store::arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<T>())
        .with_context(|| format!("mem_samples_raw is missing column '{name}'"))
}

fn list_values(list: &ListArray, index: usize) -> Result<Vec<u64>> {
    let values = list.value(index);
    let values = values
        .as_any()
        .downcast_ref::<UInt64Array>()
        .context("expected a list of unsigned 64-bit values")?;
    Ok(values.values().to_vec())
}
