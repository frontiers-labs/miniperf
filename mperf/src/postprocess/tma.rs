use anyhow::Result;
use mperf_data::{EventType, ScenarioInfo};

use super::event_column_name;
use super::tables::Tables;

/// Materialize the per-function `tma` table plus the interval and summary
/// tables the UI reads without re-expanding formulas.
pub(crate) fn process(tables: &Tables, info: &ScenarioInfo) -> Result<()> {
    let ScenarioInfo::TMA(info) = info else {
        unreachable!("TMA tables require TMA recording metadata");
    };

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
