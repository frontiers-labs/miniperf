//! Top-Down datasets: the metric hierarchy, the per-second interval shares and
//! the per-function level-1 breakdown. All three are recording-wide and load
//! once, with the session's database connection.

use std::collections::{BTreeMap, HashMap};

use mperf_data::ScenarioInfo;

use crate::model::{TmaSummaryData, TmaSummaryRow};
use crate::sql::{Connection, Value, as_f64, as_i64, as_text};

/// One second of pipeline-slot shares, aligned with [`TmaData::level1`].
#[derive(Clone, Debug, PartialEq)]
pub struct TmaInterval {
    pub start_ns: u64,
    pub values: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct TmaData {
    /// Hierarchy rows (levels 1..3) in the order the vendor spec declares them.
    pub rows: Vec<TmaSummaryRow>,
    /// Indices into `rows` of the level-1 metrics.
    pub level1: Vec<usize>,
    pub intervals: Vec<TmaInterval>,
    /// Level-1 shares per function name, aligned with `level1`.
    pub functions: HashMap<String, Vec<f64>>,
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

        Some(Self {
            intervals: load_intervals(connection, &names),
            functions: load_functions(connection, &names),
            level1,
            rows: summary.rows,
            error: summary.error,
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

fn load_intervals(connection: &Connection, names: &[&str]) -> Vec<TmaInterval> {
    let Ok(mut statement) =
        connection.prepare("SELECT start_ns, metric, value FROM tma_intervals ORDER BY start_ns;")
    else {
        return Vec::new();
    };
    let Ok(mut rows) = statement.query([]) else {
        return Vec::new();
    };
    let mut by_start = BTreeMap::<u64, Vec<f64>>::new();
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            _ => break,
        };
        let Ok(metric) = row.get::<_, Value>(1) else {
            continue;
        };
        let Some(metric) = as_text(&metric) else {
            continue;
        };
        let Some(column) = names.iter().position(|name| *name == metric) else {
            continue;
        };
        let Ok(value) = row.get::<_, Value>(2) else {
            continue;
        };
        let Some(value) = as_f64(&value).filter(|value| value.is_finite()) else {
            continue;
        };
        let Ok(start_ns) = row.get::<_, Value>(0) else {
            continue;
        };
        let start_ns = as_i64(&start_ns).unwrap_or_default().max(0) as u64;
        by_start
            .entry(start_ns)
            .or_insert_with(|| vec![0.0; names.len()])[column] = value.clamp(0.0, 1.0);
    }
    by_start
        .into_iter()
        .map(|(start_ns, values)| TmaInterval { start_ns, values })
        .collect()
}

/// Level-1 shares per function from `VIEW tma`, whose columns spell the metric
/// names with underscores.
fn load_functions(connection: &Connection, names: &[&str]) -> HashMap<String, Vec<f64>> {
    let Ok(mut statement) = connection.prepare("SELECT * FROM tma;") else {
        return HashMap::new();
    };
    let Ok(mut rows) = statement.query([]) else {
        return HashMap::new();
    };
    let columns: Vec<String> = names
        .iter()
        .map(|name| name.replace('.', "_"))
        .collect::<Vec<_>>();
    let mut functions = HashMap::new();
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            _ => break,
        };
        let Ok(function) = row.get::<_, Value>("func_name") else {
            continue;
        };
        let Some(function) = as_text(&function) else {
            continue;
        };
        let values: Vec<f64> = columns
            .iter()
            .map(|column| {
                row.get::<_, Value>(column.as_str())
                    .ok()
                    .as_ref()
                    .and_then(as_f64)
                    .filter(|value| value.is_finite())
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
            })
            .collect();
        if values.iter().sum::<f64>() > 0.0 {
            functions.insert(function.to_owned(), values);
        }
    }
    functions
}
