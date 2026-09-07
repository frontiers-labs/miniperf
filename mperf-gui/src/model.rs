use std::collections::HashMap;

use anyhow::{Context, Result};
use mperf_data::ScenarioInfo;
use pmu_data::TmaMetric;

use crate::sql::{Connection, SqlResult};

#[derive(Debug, Clone, PartialEq)]
pub struct TmaSummaryData {
    pub rows: Vec<TmaSummaryRow>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TmaSummaryRow {
    pub name: String,
    pub description: String,
    pub level: usize,
    pub value: Option<f64>,
    pub dominant: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SummaryStats {
    pub cycles: u64,
    pub instructions: u64,
}

impl TmaSummaryData {
    pub fn for_scenario(scenario: &ScenarioInfo, connection: &Connection) -> Option<Self> {
        let ScenarioInfo::TMA(info) = scenario else {
            return None;
        };
        Some(Self::load(connection, &info.metrics))
    }

    fn load(connection: &Connection, metrics: &[TmaMetric]) -> Self {
        match load_persisted_tma_summary(connection) {
            Ok(persisted) => Self {
                rows: join_tma_summary(metrics, &persisted),
                error: None,
            },
            Err(error) => Self {
                rows: join_tma_summary(metrics, &HashMap::new()),
                error: Some(format!("{error:#}")),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PersistedTmaSummary {
    value: Option<f64>,
    dominant: bool,
}

fn load_persisted_tma_summary(
    connection: &Connection,
) -> Result<HashMap<String, PersistedTmaSummary>> {
    let mut statement = connection
        .prepare("SELECT metric, value, verdict FROM tma_summary;")
        .context("TMA summary is unavailable: failed to query table `tma_summary`")?;
    let rows = statement
        .query_map([], |row| {
            let metric = row.get::<_, String>(0)?;
            let value = finite_tma_value(row.get::<_, Option<f64>>(1)?);
            let dominant = row
                .get::<_, Option<String>>(2)?
                .is_some_and(|verdict| verdict.trim().eq_ignore_ascii_case("dominant"));
            Ok((metric, PersistedTmaSummary { value, dominant }))
        })
        .context("failed to read table `tma_summary`")?;

    rows.collect::<SqlResult<HashMap<_, _>>>()
        .context("failed to read a row from table `tma_summary`")
}

fn join_tma_summary(
    metrics: &[TmaMetric],
    persisted: &HashMap<String, PersistedTmaSummary>,
) -> Vec<TmaSummaryRow> {
    metrics
        .iter()
        .filter_map(|metric| {
            let level = tma_hierarchy_level(&metric.name);
            (1..=3).contains(&level).then(|| {
                let summary = persisted.get(&metric.name).copied().unwrap_or_default();
                TmaSummaryRow {
                    name: metric.name.clone(),
                    description: metric.desc.clone(),
                    level,
                    value: summary.value,
                    dominant: summary.dominant,
                }
            })
        })
        .collect()
}

fn tma_hierarchy_level(name: &str) -> usize {
    name.bytes().filter(|byte| *byte == b'.').count() + 1
}

fn finite_tma_value(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

impl SummaryStats {
    pub fn load(connection: &Connection) -> Result<Self> {
        let mut statement = connection
            .prepare(
                "SELECT CAST(SUM(pmu_cycles) AS BIGINT) AS pmu_cycles,
                        CAST(SUM(pmu_instructions) AS BIGINT) AS pmu_instructions
                 FROM pmu_counters;",
            )
            .context("failed to prepare summary query")?;
        let mut rows = statement.query([]).context("failed to run summary query")?;
        let row = rows
            .next()
            .context("failed to read summary row")?
            .context("summary query returned no rows")?;

        let read = |name| -> Result<u64> {
            Ok(row
                .get::<_, Option<i64>>(name)
                .with_context(|| format!("failed to read {name}"))?
                .unwrap_or_default() as u64)
        };

        Ok(Self {
            cycles: read("pmu_cycles")?,
            instructions: read("pmu_instructions")?,
        })
    }
}
