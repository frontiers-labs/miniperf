//! Top-Down datasets: the metric hierarchy, the per-second interval shares and
//! the per-function level-1 breakdown. All three are recording-wide and load
//! once, with the session's database connection.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use mperf_data::ScenarioInfo;

use crate::model::{TmaSummaryData, TmaSummaryRow};
use crate::sql::{Connection, SqlResult};

/// One second of pipeline-slot shares, aligned with [`TmaData::level1`].
#[derive(Clone, Debug, PartialEq)]
pub struct TmaInterval {
    pub start_ns: u64,
    /// Missing or non-finite measurements remain unavailable.
    pub values: Vec<Option<f64>>,
}

#[derive(Clone, Debug)]
pub struct TmaData {
    /// Hierarchy rows (levels 1..3) in the order the vendor spec declares them.
    pub rows: Vec<TmaSummaryRow>,
    /// Indices into `rows` of the level-1 metrics.
    pub level1: Vec<usize>,
    pub intervals: Vec<TmaInterval>,
    /// Level-1 shares per function name, aligned with `level1`; `None` is unavailable.
    pub functions: HashMap<String, Vec<Option<f64>>>,
    pub error: Option<String>,
}

impl TmaData {
    /// Returns `None` for recordings whose scenario carries no TMA metrics.
    pub fn load(scenario: &ScenarioInfo, connection: &Connection) -> Option<Self> {
        let summary = TmaSummaryData::for_scenario(scenario, connection)?;
        let level1: Vec<usize> = summary
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.level == 1)
            .map(|(index, _)| index)
            .collect();
        let names: Vec<&str> = level1
            .iter()
            .map(|index| summary.rows[*index].name.as_str())
            .collect();

        let mut errors: Vec<String> = summary.error.into_iter().collect();
        let intervals = load_intervals(connection, &names).unwrap_or_else(|error| {
            errors.push(format!("{error:#}"));
            Vec::new()
        });
        let functions = load_functions(connection, &names).unwrap_or_else(|error| {
            errors.push(format!("{error:#}"));
            HashMap::new()
        });

        Some(Self {
            intervals,
            functions,
            level1,
            rows: summary.rows,
            error: (!errors.is_empty()).then(|| errors.join("\n")),
        })
    }

    pub fn has_hierarchy(&self) -> bool {
        self.rows.iter().any(|row| row.value.is_some())
    }

    pub fn level1_rows(&self) -> impl Iterator<Item = &TmaSummaryRow> {
        self.level1.iter().map(|index| &self.rows[*index])
    }

    /// The chain of dominant metrics: the largest level-1 metric, then the
    /// largest of its children, and so on. Vendors expose different depths, so
    /// this walks the dotted names rather than a fixed tree.
    pub fn dominant_path(&self) -> Vec<usize> {
        let mut path = Vec::new();
        let mut prefix: Option<String> = None;
        loop {
            let next = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| match prefix.as_deref() {
                    None => row.level == 1,
                    Some(parent) => is_child_of(&row.name, parent),
                })
                .filter_map(|(index, row)| Some((index, row.value?)))
                .max_by(|left, right| left.1.total_cmp(&right.1));
            match next {
                Some((index, _)) => {
                    path.push(index);
                    prefix = Some(self.rows[index].name.clone());
                }
                None => return path,
            }
        }
    }
}

pub fn is_child_of(name: &str, parent: &str) -> bool {
    name.strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|rest| !rest.contains('.'))
}

fn load_intervals(connection: &Connection, names: &[&str]) -> Result<Vec<TmaInterval>> {
    let mut statement = connection
        .prepare("SELECT start_ns, metric, value FROM tma_intervals ORDER BY start_ns;")
        .context("Per-interval TMA data is unavailable: failed to query `tma_intervals`")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                finite_share(row.get::<_, Option<f64>>(2)?),
            ))
        })
        .context("failed to read `tma_intervals`")?;
    let mut by_start = BTreeMap::<u64, Vec<Option<f64>>>::new();
    for row in rows {
        let (start_ns, metric, value) = row.context("failed to decode a `tma_intervals` row")?;
        let Some(column) = names.iter().position(|name| *name == metric) else {
            continue;
        };
        by_start
            .entry(start_ns)
            .or_insert_with(|| vec![None; names.len()])[column] = value;
    }
    Ok(by_start
        .into_iter()
        .map(|(start_ns, values)| TmaInterval { start_ns, values })
        .collect())
}

/// Level-1 shares per function from `VIEW tma`, whose columns spell the metric
/// names with underscores.
fn load_functions(
    connection: &Connection,
    names: &[&str],
) -> Result<HashMap<String, Vec<Option<f64>>>> {
    let mut statement = connection
        .prepare("SELECT * FROM tma;")
        .context("Per-function TMA data is unavailable: failed to query `tma`")?;
    let columns: Vec<String> = names.iter().map(|name| name.replace('.', "_")).collect();
    let rows = statement
        .query_map([], |row| {
            let function = row.get::<_, String>("func_name")?;
            let values = columns
                .iter()
                .map(|column| Ok(finite_share(row.get::<_, Option<f64>>(column.as_str())?)))
                .collect::<SqlResult<Vec<_>>>()?;
            Ok((function, values))
        })
        .context("failed to read `tma`")?;
    rows.collect::<SqlResult<HashMap<_, _>>>()
        .context("failed to decode a `tma` row")
}

fn finite_share(value: Option<f64>) -> Option<f64> {
    value
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
}
