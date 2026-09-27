use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use mperf_data::{EventType, ScenarioInfo};
use serde::Deserialize;

use super::event_column_name;
use super::tables::{Columns, Tables};

/// Materialize the per-function `tma` table plus the interval and summary
/// tables the UI reads without re-expanding formulas.
pub(crate) fn process(tables: &Tables, info: &ScenarioInfo, session_dir: &Path) -> Result<()> {
    let ScenarioInfo::TMA(info) = info else {
        unreachable!("TMA tables require TMA recording metadata");
    };

    let windows_file = session_dir.join("windows-tma-intervals.json");
    if windows_file.exists() {
        return process_windows(tables, info, &windows_file);
    }

    let columns = info
        .metrics
        .iter()
        .map(|metric| {
            let sql = metric_expression(info, metric)?;
            Ok::<String, anyhow::Error>(format!("{} AS {}", sql, metric.name.replace('.', "_")))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",\n");

    tables.write_query(
        "tma",
        &format!(
            "SELECT
                 proc_map.func_name AS func_name,
                 COUNT(pmu_counters.pmu_cycles) AS num_samples,
                 SUM(pmu_counters.pmu_cycles) * 1.0 /
                     NULLIF((SELECT SUM(pmu_cycles) FROM pmu_counters), 0) AS total,
                 CAST(SUM(pmu_counters.pmu_cycles) AS BIGINT) AS cycles,
                 CAST(SUM(pmu_counters.pmu_instructions) AS BIGINT) AS instructions,
                 SUM(pmu_counters.pmu_instructions) * 1.0 /
                     NULLIF(SUM(pmu_counters.pmu_cycles), 0) AS ipc,
                 {columns}
             FROM pmu_counters
             INNER JOIN proc_map ON pmu_counters.ip = proc_map.ip
             GROUP BY proc_map.func_name"
        ),
    )?;

    let mut intervals = Vec::new();
    let mut summaries = Vec::new();
    for metric in &info.metrics {
        let sql = metric_expression(info, metric)?;
        let name = metric.name.replace('\'', "''");
        intervals.push(format!(
            "SELECT (timestamp // 1000000000) * 1000000000 AS start_ns, '{name}' AS metric,
                    CAST({sql} AS DOUBLE) AS value
             FROM pmu_counters GROUP BY timestamp // 1000000000"
        ));
        summaries.push(format!(
            "SELECT '{name}' AS metric, CAST({sql} AS DOUBLE) AS value FROM pmu_counters"
        ));
    }
    tables.write_query(
        "tma_intervals",
        &if intervals.is_empty() {
            "SELECT CAST(NULL AS BIGINT) AS start_ns, CAST(NULL AS VARCHAR) AS metric,
                    CAST(NULL AS DOUBLE) AS value WHERE FALSE"
                .to_owned()
        } else {
            intervals.join("\nUNION ALL\n")
        },
    )?;
    tables.write_query(
        "tma_summary",
        &if summaries.is_empty() {
            "SELECT CAST(NULL AS VARCHAR) AS metric, CAST(NULL AS DOUBLE) AS value,
                    CAST(NULL AS VARCHAR) AS verdict WHERE FALSE"
                .to_owned()
        } else {
            format!(
                "WITH metrics AS ({})
                 SELECT metric, value,
                        CASE WHEN metric = (SELECT metric FROM metrics WHERE value IS NOT NULL
                                            ORDER BY value DESC LIMIT 1)
                             THEN 'dominant' END AS verdict
                 FROM metrics",
                summaries.join("\nUNION ALL\n")
            )
        },
    )
}

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

fn process_windows(tables: &Tables, info: &mperf_data::TMAInfo, path: &Path) -> Result<()> {
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
    for event in event_names {
        cols.u64(
            &format!("pmu_{}", event.replace('.', "_")),
            accepted
                .iter()
                .map(|r| r.deltas.get(&event).copied().unwrap_or(0))
                .collect(),
        );
    }
    tables.write("pmu_intervals", cols.finish()?)?;

    // Windows CSwitch data has no sampled instruction pointer, so there is no
    // honest per-function attribution. Preserve the established empty schema.
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
    tables.write_query("tma", &format!("SELECT proc_map.func_name AS func_name, COUNT(pmu_counters.pmu_cycles) AS num_samples, SUM(pmu_counters.pmu_cycles) * 1.0 / NULLIF((SELECT SUM(pmu_cycles) FROM pmu_counters), 0) AS total, CAST(SUM(pmu_counters.pmu_cycles) AS BIGINT) AS cycles, CAST(SUM(pmu_counters.pmu_instructions) AS BIGINT) AS instructions, SUM(pmu_counters.pmu_instructions) * 1.0 / NULLIF(SUM(pmu_counters.pmu_cycles), 0) AS ipc{metric_columns} FROM pmu_counters INNER JOIN proc_map ON pmu_counters.ip = proc_map.ip WHERE FALSE GROUP BY proc_map.func_name"))?;

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
    fn materializes_windows_summary_without_inventing_function_attribution() {
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
        maps.u64("ip", vec![]);
        maps.text("func_name", vec![]);
        tables.write("proc_map", maps.finish().unwrap()).unwrap();

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
        assert_eq!(function_count, 0);
    }
}

fn metric_expression(info: &mperf_data::TMAInfo, metric: &pmu_data::TmaMetric) -> Result<String> {
    let expression = pmu_data::arith_parser::try_parse_expr(&metric.formula)
        .map_err(|error| anyhow::anyhow!("invalid TMA formula '{}': {error}", metric.name))?;
    let marker = metric
        .group
        .as_ref()
        .and_then(|group| {
            info.groups
                .iter()
                .find(|candidate| &candidate.name == group)
                .and_then(|group| {
                    group.events.iter().find(|event| {
                        event.as_str() != "cycles" && event.as_str() != "instructions"
                    })
                })
        })
        .map(|event| tma_marker_column(&info.counters, event));
    let mut conditions = Vec::new();
    if let Some(marker) = marker {
        conditions.push(format!("pmu_counters.{marker} IS NOT NULL"));
    }
    if let Some(cpus) = &metric.cpus {
        conditions.push(cpu_predicate(cpus));
    }
    let filter = (!conditions.is_empty()).then(|| conditions.join(" AND "));
    Ok(build_tma_sql_expr(
        &info.metrics,
        &info.counters,
        &info.constants,
        &expression,
        filter.as_deref(),
    ))
}

/// A predicate restricting a metric to one core cluster, from its sysfs
/// cpumask (`0,5-11`). Metrics on a heterogeneous host are only meaningful
/// within one core type, whose topdown parameters they were written for.
fn cpu_predicate(cpus: &str) -> String {
    let ranges = cpus
        .split(',')
        .filter_map(|part| match part.trim().split_once('-') {
            Some((low, high)) => Some((low.trim().parse::<u32>().ok()?, high.trim().parse().ok()?)),
            None => part.trim().parse::<u32>().ok().map(|cpu| (cpu, cpu)),
        })
        .map(|(low, high)| format!("pmu_counters.cpu BETWEEN {low} AND {high}"))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return "TRUE".to_owned();
    }
    format!("({})", ranges.join(" OR "))
}

fn tma_marker_column(events: &[(EventType, String)], event: &str) -> String {
    events
        .iter()
        .find(|(_, name)| name == event)
        .map(event_column_name)
        .unwrap_or_else(|| format!("pmu_{}", event.replace('.', "_")))
}

fn build_tma_sql_expr(
    metrics: &[pmu_data::TmaMetric],
    events: &[(EventType, String)],
    constants: &[pmu_data::TmaConstant],
    expression: &pmu_data::arith_parser::Expr,
    filter: Option<&str>,
) -> String {
    use pmu_data::arith_parser::{BinOp, Expr};

    match expression {
        Expr::Variable(variable) => events
            .iter()
            .find_map(|(event_type, name)| {
                (name == variable).then(|| {
                    let column = event_column_name(&(*event_type, name.clone()));
                    let value = if matches!(
                        event_type,
                        EventType::PmuCycles | EventType::PmuInstructions
                    ) {
                        format!("SUM(pmu_counters.{column})")
                    } else {
                        format!("SUM(pmu_counters.{column} / pmu_counters.confidence)")
                    };
                    filter.map_or(value.clone(), |filter| {
                        format!(
                            "SUM(CASE WHEN {filter} THEN ({}) END)",
                            value.trim_start_matches("SUM(").trim_end_matches(')')
                        )
                    })
                })
            })
            .unwrap_or_else(|| {
                let metric = metrics
                    .iter()
                    .find(|metric| metric.name == *variable)
                    .unwrap_or_else(|| panic!("unknown TMA variable '{variable}'"));
                let nested = pmu_data::arith_parser::parse_expr(&metric.formula);
                format!(
                    "({})",
                    build_tma_sql_expr(metrics, events, constants, &nested, filter)
                )
            }),
        Expr::Constant(name) => constants
            .iter()
            .find(|constant| constant.name == *name)
            // A missing constant must make the metric unavailable, never turn
            // into a plausible-looking zero-valued result.
            .map_or_else(|| "NULL".to_string(), |constant| constant.value.to_string()),
        Expr::Binary { op, lhs, rhs } => {
            let lhs = build_tma_sql_expr(metrics, events, constants, lhs, filter);
            let rhs = build_tma_sql_expr(metrics, events, constants, rhs, filter);
            match op {
                BinOp::Add => format!("({lhs}) + ({rhs})"),
                BinOp::Sub => format!("({lhs}) - ({rhs})"),
                BinOp::Mul => format!("({lhs}) * ({rhs})"),
                BinOp::Div => {
                    format!("CAST(({lhs}) AS DOUBLE) / NULLIF(CAST(({rhs}) AS DOUBLE), 0)")
                }
                BinOp::Eq => format!("({lhs}) = ({rhs})"),
                BinOp::Lt => format!("({lhs}) < ({rhs})"),
                BinOp::Le => format!("({lhs}) <= ({rhs})"),
                BinOp::Gt => format!("({lhs}) > ({rhs})"),
                BinOp::Ge => format!("({lhs}) >= ({rhs})"),
            }
        }
        Expr::Call { name, args } => {
            let args = args
                .iter()
                .map(|arg| build_tma_sql_expr(metrics, events, constants, arg, filter))
                .collect::<Vec<_>>();
            match name.to_ascii_lowercase().as_str() {
                "min" if args.len() == 2 => format!("least({}, {})", args[0], args[1]),
                "max" if args.len() == 2 => format!("greatest({}, {})", args[0], args[1]),
                "abs" if args.len() == 1 => format!("ABS({})", args[0]),
                "if" if args.len() == 3 => format!(
                    "CASE WHEN ({}) <> 0 THEN ({}) ELSE ({}) END",
                    args[0], args[1], args[2]
                ),
                _ => "NULL".to_owned(),
            }
        }
        Expr::Num(number) => number.to_string(),
    }
}
