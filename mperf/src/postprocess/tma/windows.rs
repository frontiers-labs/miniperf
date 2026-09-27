//! Windows ETW TMA interval and sampled-function postprocessing.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
#[cfg(test)]
use mperf_data::EventType;
use serde::Deserialize;

use super::super::metric_expression;
use crate::postprocess::tables::{Columns, Tables, quote_identifier};

#[derive(Deserialize)]
struct WindowsRecording {
    qpc_frequency: u64,
    coverage: serde_json::Value,
    loss: serde_json::Value,
    intervals: Vec<WindowsInterval>,
}

#[derive(Deserialize)]
struct WindowsInterval {
    cpu: u16,
    start: i64,
    end: i64,
    tid: u32,
    pid: u32,
    deltas: BTreeMap<String, u64>,
}

pub(super) fn process_windows(
    tables: &Tables,
    info: &mperf_data::TMAInfo,
    path: &Path,
) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let recording: WindowsRecording = serde_json::from_slice(&data)
        .with_context(|| format!("invalid Windows TMA interval file {}", path.display()))?;
    if recording.qpc_frequency == 0 {
        bail!("Windows TMA qpc_frequency must be greater than zero");
    }
    if has_nonzero_etw_loss(&recording.loss) {
        bail!(
            "Windows TMA recording reports nonzero ETW event or buffer loss: {}",
            recording.loss
        );
    }
    let mut accepted = Vec::new();
    let mut required = Vec::new();
    for metric in &info.metrics {
        let expr = pmu_data::arith_parser::try_parse_expr(&metric.formula)
            .map_err(|e| anyhow::anyhow!("invalid TMA formula '{}': {e}", metric.name))?;
        collect_variables(&expr, &mut required);
    }
    required.sort();
    required.dedup();
    let required = required
        .into_iter()
        .filter(|name| {
            !info.metrics.iter().any(|m| m.name == *name)
                && !info.constants.iter().any(|c| c.name == *name)
        })
        .collect::<Vec<_>>();
    for (idx, interval) in recording.intervals.into_iter().enumerate() {
        if interval.end <= interval.start {
            continue;
        }
        for event in &required {
            if !interval.deltas.contains_key(event) {
                bail!("Windows TMA interval {idx} is missing required event '{event}'");
            }
        }
        accepted.push(interval);
    }
    if accepted.is_empty() {
        bail!("Windows TMA recording contains zero accepted intervals");
    }

    let mut cols = Columns::default();
    cols.u64(
        "qpc_frequency",
        vec![recording.qpc_frequency; accepted.len()],
    );
    cols.text(
        "coverage",
        vec![recording.coverage.to_string(); accepted.len()],
    );
    cols.text("loss", vec![recording.loss.to_string(); accepted.len()]);
    cols.i64(
        "start_ns",
        accepted
            .iter()
            .map(|r| qpc_to_ns(r.start, recording.qpc_frequency))
            .collect(),
    );
    cols.i64(
        "end_ns",
        accepted
            .iter()
            .map(|r| qpc_to_ns(r.end, recording.qpc_frequency))
            .collect(),
    );
    cols.i64("cpu", accepted.iter().map(|r| r.cpu as i64).collect());
    cols.u64("tid", accepted.iter().map(|r| r.tid as u64).collect());
    cols.u64("pid", accepted.iter().map(|r| r.pid as u64).collect());
    let event_names = accepted
        .iter()
        .flat_map(|r| r.deltas.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    for event in &event_names {
        cols.u64(
            &format!("pmu_{}", event.replace('.', "_")),
            accepted
                .iter()
                .map(|r| r.deltas.get(event).copied().unwrap_or(0))
                .collect(),
        );
    }
    tables.write("pmu_intervals", cols.finish()?)?;

    // A timer sample identifies a function that ran in one coherent CSwitch
    // interval. Split that interval's measured vector across its samples.
    // This is statistical function attribution; unsampled intervals remain
    // solely in the measured process summary and time series below.
    let event_select = event_names
        .iter()
        .map(|event| {
            let column = format!("pmu_{}", event.replace('.', "_"));
            let quoted = quote_identifier(&column);
            format!("i.{quoted} * 1.0 / sample_count AS {quoted}")
        })
        .collect::<Vec<_>>()
        .join(",\n");
    tables.write_query(
        "tma_attributed",
        &format!(
            "WITH matched AS (
                 SELECT s.ip, s.cpu, s.timestamp, i.tid, i.pid,
                        COUNT(*) OVER (PARTITION BY i.cpu, i.start_ns, i.end_ns, i.tid) AS sample_count,
                        i.*
                 FROM pmu_intervals i
                 INNER JOIN samples s ON s.pid = i.pid AND s.tid = i.tid AND s.cpu = i.cpu
                     AND s.timestamp > i.start_ns AND s.timestamp <= i.end_ns
                 INNER JOIN proc_map p ON p.ip = s.ip
             )
             SELECT ROW_NUMBER() OVER () AS unique_id, pid AS process_id,
                    tid AS thread_id, cpu, timestamp, ip,
                    '[' || CAST(ip AS VARCHAR) || ']' AS call_stack,
                    1 AS time_enabled, 1 AS time_running, 1.0 AS confidence,
                    {event_select}
             FROM matched i"
        ),
    )?;
    let attributed_samples: i64 =
        tables
            .connection()
            .query_row("SELECT COUNT(*) FROM tma_attributed", [], |row| row.get(0))?;
    if attributed_samples == 0 {
        bail!("Windows TMA trace has no symbolized timer samples inside measured PMC intervals");
    }
    tables.write_query(
        "tma_attribution",
        "SELECT COUNT(*) AS measured_intervals,
                COUNT(*) FILTER (WHERE EXISTS (
                    SELECT 1 FROM tma_attributed a
                    WHERE a.cpu = i.cpu AND a.thread_id = i.tid
                      AND a.timestamp > i.start_ns AND a.timestamp <= i.end_ns
                )) AS sampled_intervals,
                (SELECT COUNT(*) FROM tma_attributed) AS samples,
                'sampled_interval' AS method
         FROM pmu_intervals i",
    )?;
    let metric_columns = info
        .metrics
        .iter()
        .map(|metric| {
            let sql = metric_expression(info, metric)?;
            Ok::<String, anyhow::Error>(format!("{} AS {}", sql, metric.name.replace('.', "_")))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",\n");
    let metric_columns = if metric_columns.is_empty() {
        String::new()
    } else {
        format!(",\n{metric_columns}")
    };
    tables.write_query("tma", &format!("SELECT proc_map.func_name AS func_name, COUNT(pmu_counters.pmu_cycles) AS num_samples, SUM(pmu_counters.pmu_cycles) * 1.0 / NULLIF((SELECT SUM(pmu_cycles) FROM pmu_intervals), 0) AS total, CAST(SUM(pmu_counters.pmu_cycles) AS BIGINT) AS cycles, CAST(SUM(pmu_counters.pmu_instructions) AS BIGINT) AS instructions, SUM(pmu_counters.pmu_instructions) * 1.0 / NULLIF(SUM(pmu_counters.pmu_cycles), 0) AS ipc{metric_columns}, 'sampled_interval' AS attribution FROM tma_attributed AS pmu_counters INNER JOIN proc_map ON pmu_counters.ip = proc_map.ip GROUP BY proc_map.func_name"))?;

    let mut sums: BTreeMap<String, u64> = BTreeMap::new();
    for row in &accepted {
        for (name, value) in &row.deltas {
            let total = sums.entry(name.clone()).or_default();
            *total = total.saturating_add(*value);
        }
    }
    let mut buckets: BTreeMap<i64, BTreeMap<String, u64>> = BTreeMap::new();
    for row in &accepted {
        let bucket = bucket_start_ns(qpc_to_ns(row.start, recording.qpc_frequency));
        let counters = buckets.entry(bucket).or_default();
        for (name, value) in &row.deltas {
            let total = counters.entry(name.clone()).or_default();
            *total = total.saturating_add(*value);
        }
    }
    let mut summary_names = Vec::new();
    let mut summary_values = Vec::new();
    let mut interval_names = Vec::new();
    let mut interval_values = Vec::new();
    let mut starts = Vec::new();
    for metric in &info.metrics {
        let expr = pmu_data::arith_parser::try_parse_expr(&metric.formula)
            .map_err(|e| anyhow::anyhow!("invalid TMA formula '{}': {e}", metric.name))?;
        let value = eval_expr(&expr, &sums, info)?;
        summary_names.push(metric.name.clone());
        summary_values.push(value);
        for (bucket, counters) in &buckets {
            let val = eval_expr(&expr, counters, info)?;
            interval_names.push(metric.name.clone());
            interval_values.push(val);
            starts.push(*bucket);
        }
    }
    let maximum = summary_values.iter().flatten().copied().reduce(f64::max);
    let verdicts = summary_values
        .iter()
        .map(|value| {
            if value.is_some() && value == &maximum {
                Some("dominant".to_owned())
            } else {
                None
            }
        })
        .collect();
    let mut summary = Columns::default();
    summary.text("metric", summary_names);
    summary.f64_opt("value", summary_values);
    summary.text_opt("verdict", verdicts);
    tables.write("tma_summary", summary.finish()?)?;
    let mut intervals = Columns::default();
    intervals.i64("start_ns", starts);
    intervals.text("metric", interval_names);
    intervals.f64_opt("value", interval_values);
    tables.write("tma_intervals", intervals.finish()?)?;
    // The GUI's hotspot analysis reads this established table when it reopens
    // the session. Replace the empty timer-only counter table with the
    // interval-weighted vectors, preserving the persisted Parquet filename.
    tables
        .connection()
        .execute_batch("DROP VIEW IF EXISTS pmu_counters")?;
    let attributed_path = path.with_file_name("tma_attributed.parquet");
    let counter_path = path.with_file_name("pmu_counters.parquet");
    std::fs::copy(&attributed_path, &counter_path).with_context(|| {
        format!(
            "could not save attributed Windows TMA counters to {}",
            counter_path.display()
        )
    })?;
    tables.register("pmu_counters", &[counter_path])?;
    Ok(())
}

fn qpc_to_ns(value: i64, frequency: u64) -> i64 {
    ((value as i128) * 1_000_000_000i128 / frequency as i128)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn bucket_start_ns(timestamp_ns: i64) -> i64 {
    ((timestamp_ns as i128).div_euclid(1_000_000_000) * 1_000_000_000)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn has_nonzero_etw_loss(loss: &serde_json::Value) -> bool {
    let Some(object) = loss.as_object() else {
        return false;
    };
    ["events", "log_buffers", "realtime_buffers"]
        .iter()
        .any(|key| {
            object
                .get(*key)
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|n| n > 0)
        })
}

fn collect_variables(expr: &pmu_data::arith_parser::Expr, out: &mut Vec<String>) {
    use pmu_data::arith_parser::Expr;
    match expr {
        Expr::Variable(v) => out.push(v.clone()),
        Expr::Binary { lhs, rhs, .. } => {
            collect_variables(lhs, out);
            collect_variables(rhs, out);
        }
        Expr::Call { args, .. } => {
            for arg in args {
                collect_variables(arg, out);
            }
        }
        _ => {}
    }
}

fn eval_expr(
    expr: &pmu_data::arith_parser::Expr,
    events: &BTreeMap<String, u64>,
    info: &mperf_data::TMAInfo,
) -> Result<Option<f64>> {
    use pmu_data::arith_parser::{BinOp, Expr};
    Ok(match expr {
        Expr::Num(n) => Some(*n),
        Expr::Constant(name) => info
            .constants
            .iter()
            .find(|c| c.name == *name)
            .map(|c| c.value as f64),
        Expr::Variable(name) => {
            if let Some(metric) = info.metrics.iter().find(|m| m.name == *name) {
                let nested = pmu_data::arith_parser::try_parse_expr(&metric.formula)
                    .map_err(|e| anyhow::anyhow!("invalid TMA formula '{}': {e}", metric.name))?;
                eval_expr(&nested, events, info)?
            } else {
                events.get(name).map(|v| *v as f64)
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            match (eval_expr(lhs, events, info)?, eval_expr(rhs, events, info)?) {
                (Some(a), Some(b)) => match op {
                    BinOp::Add => Some(a + b),
                    BinOp::Sub => Some(a - b),
                    BinOp::Mul => Some(a * b),
                    BinOp::Div => {
                        if b == 0.0 {
                            None
                        } else {
                            Some(a / b)
                        }
                    }
                    BinOp::Eq => Some((a == b) as u8 as f64),
                    BinOp::Lt => Some((a < b) as u8 as f64),
                    BinOp::Le => Some((a <= b) as u8 as f64),
                    BinOp::Gt => Some((a > b) as u8 as f64),
                    BinOp::Ge => Some((a >= b) as u8 as f64),
                },
                _ => None,
            }
        }
        Expr::Call { name, args } => {
            let values = args
                .iter()
                .map(|arg| eval_expr(arg, events, info))
                .collect::<Result<Vec<_>>>()?;
            if values.iter().any(Option::is_none) {
                None
            } else {
                let v = values.into_iter().flatten().collect::<Vec<_>>();
                match name.to_ascii_lowercase().as_str() {
                    "min" if v.len() == 2 => Some(v[0].min(v[1])),
                    "max" if v.len() == 2 => Some(v[0].max(v[1])),
                    "abs" if v.len() == 1 => Some(v[0].abs()),
                    "if" if v.len() == 3 => Some(if v[0] != 0.0 { v[1] } else { v[2] }),
                    _ => None,
                }
            }
        }
    })
}

#[cfg(test)]
mod windows_tests {
    use super::*;

    #[test]
    fn converts_qpc_ticks_to_nanoseconds() {
        assert_eq!(qpc_to_ns(15, 3), 5_000_000_000);
        assert_eq!(qpc_to_ns(-3, 2), -1_500_000_000);
        assert_eq!(qpc_to_ns(i64::MAX, 1), i64::MAX);
        assert_eq!(bucket_start_ns(-1), -1_000_000_000);
    }

    #[test]
    fn evaluates_formula_against_coherent_interval_deltas() {
        let info = mperf_data::TMAInfo {
            pid: 0,
            metrics: vec![],
            counters: vec![],
            constants: vec![],
            groups: vec![],
            precise_attribution: false,
            ui: None,
        };
        let expr = pmu_data::arith_parser::try_parse_expr("a / (a + b)").unwrap();
        let events = BTreeMap::from([("a".to_owned(), 3), ("b".to_owned(), 1)]);
        assert_eq!(eval_expr(&expr, &events, &info).unwrap(), Some(0.75));
    }

    #[test]
    fn materializes_windows_summary_and_sampled_function_attribution() {
        let directory = tempfile::tempdir().unwrap();
        let tables = Tables::open(directory.path()).unwrap();
        let mut samples = Columns::default();
        samples.u64("ip", vec![]);
        samples.u64("pmu_cycles", vec![]);
        samples.u64("pmu_instructions", vec![]);
        samples.u64("pmu_TOPDOWN_SLOTS_P", vec![]);
        samples.f64("confidence", vec![]);
        samples.u64("cpu", vec![]);
        tables
            .write("pmu_counters", samples.finish().unwrap())
            .unwrap();
        let mut maps = Columns::default();
        maps.u64("ip", vec![0x1000, 0x2000]);
        maps.text("func_name", vec!["function_a".into(), "function_b".into()]);
        tables.write("proc_map", maps.finish().unwrap()).unwrap();
        let mut profile_samples = Columns::default();
        profile_samples.u64("ip", vec![0x1000, 0x2000]);
        profile_samples.u64("pid", vec![42, 42]);
        profile_samples.u64("tid", vec![7, 7]);
        profile_samples.u64("cpu", vec![0, 0]);
        profile_samples.i64("timestamp", vec![1_040_000_000, 1_050_000_000]);
        tables
            .write("samples", profile_samples.finish().unwrap())
            .unwrap();

        let info = mperf_data::TMAInfo {
            pid: 42,
            counters: vec![
                (EventType::PmuCycles, "cycles".to_owned()),
                (EventType::PmuInstructions, "instructions".to_owned()),
                (EventType::PmuCustom, "TOPDOWN.SLOTS_P".to_owned()),
            ],
            groups: vec![pmu_data::TmaGroup {
                name: "retiring".to_owned(),
                events: vec![
                    "cycles".to_owned(),
                    "instructions".to_owned(),
                    "TOPDOWN.SLOTS_P".to_owned(),
                ],
            }],
            precise_attribution: false,
            metrics: vec![pmu_data::TmaMetric {
                name: "slots_per_cycle".to_owned(),
                desc: "test metric".to_owned(),
                formula: "TOPDOWN.SLOTS_P / cycles".to_owned(),
                group: Some("retiring".to_owned()),
                cpus: None,
            }],
            constants: vec![],
            ui: None,
        };
        let path = directory.path().join("windows-tma-intervals.json");
        std::fs::write(
            &path,
            r#"{"qpc_frequency":1000,"coverage":{"accepted":2},"loss":{"events":0,"log_buffers":0,"realtime_buffers":0},"intervals":[{"cpu":0,"start":1000,"end":1100,"tid":7,"pid":42,"deltas":{"cycles":10,"instructions":5,"TOPDOWN.SLOTS_P":40}},{"cpu":0,"start":1200,"end":1300,"tid":7,"pid":42,"deltas":{"cycles":20,"instructions":10,"TOPDOWN.SLOTS_P":80}}]}"#,
        )
        .unwrap();
        process_windows(&tables, &info, &path).unwrap();
        let value = tables
            .scalar_f64("SELECT value FROM tma_summary WHERE metric = 'slots_per_cycle'")
            .unwrap();
        assert_eq!(value, 4.0);
        let interval_count: i64 = tables
            .connection()
            .query_row("SELECT COUNT(*) FROM tma_intervals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(interval_count, 1);
        let function_count: i64 = tables
            .connection()
            .query_row("SELECT COUNT(*) FROM tma", [], |row| row.get(0))
            .unwrap();
        assert_eq!(function_count, 2);
        assert_eq!(
            tables.scalar_f64("SELECT slots_per_cycle FROM tma WHERE func_name = 'function_a'"),
            Some(4.0)
        );
        let attributed_share = tables.scalar_f64("SELECT SUM(total) FROM tma").unwrap();
        assert!((attributed_share - 1.0 / 3.0).abs() < 1e-12);
        let sampled_intervals: i64 = tables
            .connection()
            .query_row("SELECT sampled_intervals FROM tma_attribution", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sampled_intervals, 1);
        let saved_samples: i64 = tables
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM pmu_counters WHERE call_stack IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved_samples, 2);
    }
}
