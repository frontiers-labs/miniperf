use super::tables::Tables;
use crate::utils;
use anyhow::Result;
use shmem::allocation::Operation;
use std::collections::HashMap;
use store::Columns;

struct Boundary {
    pid: u64,
    process_epoch: u64,
    sequence: u64,
    timestamp_ns: u64,
    tid: u64,
    operation: Operation,
    operation_id: u64,
    old_address: u64,
    address: u64,
    requested_bytes: u64,
    retires_old: bool,
    stack_id: u64,
}
struct Lifetime {
    allocation_id: u64,
    pid: u64,
    process_epoch: u64,
    tid: u64,
    address: u64,
    requested_bytes: u64,
    allocated_ns: u64,
    freed_ns: Option<u64>,
    freed_sequence: Option<u64>,
    stack_id: u64,
    realloc_growth_bytes: u64,
    reallocated: bool,
}

/// Replay ownership boundaries. Realloc outcomes are matched before retirement,
/// because another thread can reuse the old address before realloc returns.
pub(crate) fn process(tables: &Tables) -> Result<()> {
    if !tables.has_table("allocation_capture") {
        return Ok(());
    }
    let mut statement = tables.connection().prepare(
        "SELECT pid,
                process_epoch,
                sequence,
                timestamp_ns,
                tid,
                operation,
                operation_id,
                old_address,
                address,
                requested_bytes,
                disposition,
                stack_id
         FROM allocation_operations
         ORDER BY timestamp_ns, sequence",
    )?;
    let records = statement
        .query_map([], |r| {
            Ok(Boundary {
                pid: r.get(0)?,
                process_epoch: r.get(1)?,
                sequence: r.get(2)?,
                timestamp_ns: r.get(3)?,
                tid: r.get(4)?,
                operation: Operation::try_from(r.get::<_, u64>(5)?).map_err(|error| {
                    store::duckdb::Error::FromSqlConversionFailure(
                        5,
                        store::duckdb::types::Type::UBigInt,
                        Box::new(error),
                    )
                })?,
                operation_id: r.get(6)?,
                old_address: r.get(7)?,
                address: r.get(8)?,
                requested_bytes: r.get(9)?,
                retires_old: r.get::<_, u64>(10)? != 0,
                stack_id: r.get(11)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let outcomes = records
        .iter()
        .filter(|r| r.operation == Operation::ReallocEnd)
        .map(|r| ((r.pid, r.process_epoch, r.operation_id), r.retires_old))
        .collect::<HashMap<_, _>>();
    let mut live = HashMap::new();
    let mut old_sizes = HashMap::new();
    let mut lifetimes: Vec<Lifetime> = Vec::new();
    let mut uncertain = 0;
    let mut conflicts = 0;
    for record in &records {
        let key = (record.pid, record.process_epoch, record.old_address);
        let retire = record.operation == Operation::Freed
            || (record.operation == Operation::ReallocBegin
                && outcomes.get(&(record.pid, record.process_epoch, record.sequence))
                    == Some(&true));
        if record.operation == Operation::ReallocBegin
            && !outcomes.contains_key(&(record.pid, record.process_epoch, record.sequence))
        {
            uncertain += 1;
        }
        if retire && let Some(index) = live.remove(&key) {
            let lifetime: &mut Lifetime = &mut lifetimes[index];
            lifetime.freed_ns = Some(record.timestamp_ns);
            lifetime.freed_sequence = Some(record.sequence);
            if record.operation == Operation::ReallocBegin {
                old_sizes.insert(
                    (record.pid, record.process_epoch, record.sequence),
                    lifetime.requested_bytes,
                );
            }
        }
        if (record.operation == Operation::Allocated || record.operation == Operation::ReallocEnd)
            && record.address != 0
        {
            let key = (record.pid, record.process_epoch, record.address);
            if live.contains_key(&key) {
                conflicts += 1;
            }
            live.insert(key, lifetimes.len());
            let old_size = old_sizes
                .get(&(record.pid, record.process_epoch, record.operation_id))
                .copied()
                .unwrap_or(0);
            lifetimes.push(Lifetime {
                allocation_id: record.sequence,
                pid: record.pid,
                process_epoch: record.process_epoch,
                tid: record.tid,
                address: record.address,
                requested_bytes: record.requested_bytes,
                allocated_ns: record.timestamp_ns,
                freed_ns: None,
                freed_sequence: None,
                stack_id: record.stack_id,
                realloc_growth_bytes: if record.operation == Operation::ReallocEnd {
                    record.requested_bytes.saturating_sub(old_size)
                } else {
                    0
                },
                reallocated: record.operation == Operation::ReallocEnd,
            });
        }
    }
    let mut columns = Columns::default();
    macro_rules! u {
        ($name:literal,$field:ident) => {
            columns.u64($name, lifetimes.iter().map(|r| r.$field).collect());
        };
    }
    u!("allocation_id", allocation_id);
    u!("pid", pid);
    u!("process_epoch", process_epoch);
    u!("tid", tid);
    u!("address", address);
    u!("requested_bytes", requested_bytes);
    u!("allocated_ns", allocated_ns);
    u!("stack_id", stack_id);
    u!("realloc_growth_bytes", realloc_growth_bytes);
    columns.i64(
        "reallocated",
        lifetimes.iter().map(|r| i64::from(r.reallocated)).collect(),
    );
    columns.i64_opt(
        "freed_sequence",
        lifetimes
            .iter()
            .map(|r| r.freed_sequence.map(|value| value as i64))
            .collect(),
    );
    columns.i64_opt(
        "freed_ns",
        lifetimes
            .iter()
            .map(|r| r.freed_ns.map(|v| v as i64))
            .collect(),
    );
    columns.i64_opt(
        "lifetime_ns",
        lifetimes
            .iter()
            .map(|r| {
                r.freed_ns
                    .map(|freed_ns| freed_ns.saturating_sub(r.allocated_ns) as i64)
            })
            .collect(),
    );
    tables.write("allocations", columns.finish()?)?;
    let mut points = Vec::with_capacity(lifetimes.len() * 2);
    for lifetime in &lifetimes {
        points.push((
            lifetime.allocated_ns,
            lifetime.allocation_id,
            lifetime.requested_bytes as i64,
            lifetime.stack_id,
        ));
        if let Some(freed_ns) = lifetime.freed_ns {
            points.push((
                freed_ns,
                lifetime.freed_sequence.unwrap(),
                -(lifetime.requested_bytes as i64),
                lifetime.stack_id,
            ));
        }
    }
    points.sort_unstable_by_key(|r| (r.0, r.1));
    let mut total = 0i64;
    let mut columns = Columns::default();
    columns.u64("timestamp_ns", points.iter().map(|r| r.0).collect());
    columns.u64("operation_sequence", points.iter().map(|r| r.1).collect());
    columns.i64("delta_bytes", points.iter().map(|r| r.2).collect());
    columns.u64("stack_id", points.iter().map(|r| r.3).collect());
    columns.i64(
        "observed_live_bytes",
        points
            .iter()
            .map(|r| {
                total += r.2;
                total
            })
            .collect(),
    );
    tables.write("allocation_timeline", columns.finish()?)?;
    tables.write_query(
        "allocation_realloc_intervals",
        "SELECT b.pid,
                b.process_epoch,
                b.sequence AS operation_id,
                b.timestamp_ns AS begin_ns,
                e.timestamp_ns AS end_ns,
                b.old_address,
                e.address,
                e.requested_bytes,
                e.disposition
         FROM allocation_operations b
         LEFT JOIN allocation_operations e
             ON e.pid = b.pid
             AND e.process_epoch = b.process_epoch
             AND e.operation_id = b.sequence
             AND e.operation = 4
         WHERE b.operation = 3",
    )?;

    tables.write_query(
        "allocation_summary",
        &format!(
            "SELECT COUNT(*)::BIGINT AS observed_count,
                COALESCE(SUM(requested_bytes), 0)::HUGEINT AS requested_bytes,
                COALESCE(SUM(CASE WHEN freed_ns IS NULL THEN requested_bytes ELSE 0 END), 0)::HUGEINT AS live_at_end_bytes,
                (SELECT COALESCE(MAX(observed_live_bytes), 0) FROM allocation_timeline)::BIGINT AS peak_observed_bytes,
                COUNT(*) FILTER (WHERE stack_id <> 0)::BIGINT AS attributed_count,
                COALESCE(SUM(realloc_growth_bytes), 0)::HUGEINT AS realloc_growth_bytes,
                ((SELECT MIN(complete) FROM allocation_capture) = 1
                    AND {uncertain} = 0
                    AND {conflicts} = 0)::INTEGER AS complete,
                {uncertain}::BIGINT AS unresolved_reallocs,
                {conflicts}::BIGINT AS address_conflicts
         FROM allocations"
        )
    )?;
    tables.write_query(
        "allocation_sites",
        "WITH peaks AS (
             SELECT stack_id,
                    MAX(live)::BIGINT AS peak_observed_bytes
             FROM (
                 SELECT stack_id,
                        SUM(delta_bytes) OVER (
                            PARTITION BY stack_id
                            ORDER BY timestamp_ns, operation_sequence
                            ROWS UNBOUNDED PRECEDING
                        ) AS live
                 FROM allocation_timeline
             )
             GROUP BY stack_id
         )
         SELECT a.stack_id,
                COUNT(*)::BIGINT AS allocation_count,
                SUM(requested_bytes)::HUGEINT AS requested_bytes,
                SUM(CASE WHEN freed_ns IS NULL THEN requested_bytes ELSE 0 END)::HUGEINT AS live_at_end_bytes,
                COALESCE(MAX(p.peak_observed_bytes), 0)::BIGINT AS peak_observed_bytes,
                AVG(lifetime_ns)::DOUBLE AS mean_lifetime_ns,
                SUM(realloc_growth_bytes)::HUGEINT AS realloc_growth_bytes,
                (SELECT complete FROM allocation_summary) AS complete
         FROM allocations a
         LEFT JOIN peaks p ON a.stack_id = p.stack_id
         GROUP BY a.stack_id
         ORDER BY requested_bytes DESC"
    )?;
    symbolize_stacks(tables)?;
    Ok(())
}

fn symbolize_stacks(tables: &Tables) -> Result<()> {
    let modules = utils::load_modules(tables.connection())?;
    let resolver = utils::resolve_proc_maps(&modules);

    let mut statement = tables.connection().prepare(
        "SELECT pid, stack_id, frame_index, pc
         FROM allocation_stack_raw
         ORDER BY stack_id, frame_index",
    )?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, u64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let symbols = rows
        .into_iter()
        .flat_map(|row| {
            let frames = resolver.resolve(row.0 as u32, row.3);
            let frames = if frames.is_empty() {
                vec![symbolize::Frame {
                    function: "[unknown]".into(),
                    linkage_name: None,
                    file: None,
                    line: None,
                    module: None,
                }]
            } else {
                frames
            };
            frames
                .into_iter()
                .enumerate()
                .map(move |(inline, frame)| (row, inline, frame))
        })
        .collect::<Vec<_>>();
    let mut columns = Columns::default();
    columns.u64("pid", symbols.iter().map(|r| r.0.0).collect());
    columns.u64("stack_id", symbols.iter().map(|r| r.0.1).collect());
    columns.i64("frame_index", symbols.iter().map(|r| r.0.2).collect());
    columns.u64("pc", symbols.iter().map(|r| r.0.3).collect());
    columns.i64("inline_index", symbols.iter().map(|r| r.1 as i64).collect());
    columns.text_opt(
        "linkage_name",
        symbols.iter().map(|r| r.2.linkage_name.clone()).collect(),
    );
    columns.text(
        "function",
        symbols.iter().map(|r| r.2.function.clone()).collect(),
    );
    columns.text(
        "file",
        symbols
            .iter()
            .map(|r| r.2.file.clone().unwrap_or_default())
            .collect(),
    );
    columns.i64(
        "line",
        symbols
            .iter()
            .map(|r| r.2.line.unwrap_or(0) as i64)
            .collect(),
    );
    columns.text_opt(
        "module",
        symbols
            .iter()
            .map(|r| r.2.module.as_ref().map(|p| p.display().to_string()))
            .collect(),
    );
    tables.write("allocation_stacks", columns.finish()?)
}
