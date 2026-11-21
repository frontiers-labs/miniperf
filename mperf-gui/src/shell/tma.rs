//! Top-Down datasets: the metric hierarchy, the per-second values and
//! the per-function hierarchy values. All three are recording-wide and load
//! once, with the session's database connection.

use std::collections::{BTreeMap, HashMap};

use mperf_data::ScenarioInfo;
use store::query::quote_identifier;

use crate::model::{TmaSummaryData, TmaSummaryRow};
use crate::profile::ProfileFrame;
use crate::sql::{Connection, table_columns};

const INTERVAL_NS: u64 = 1_000_000_000;

type FunctionValues = HashMap<Option<String>, HashMap<String, FunctionMetrics>>;

#[derive(Clone, Debug)]
struct FunctionMetrics {
    hierarchy: Vec<Option<f64>>,
    level1: Option<Vec<Option<f64>>>,
}

/// One second of metric values, aligned with [`TmaData::level1`].
#[derive(Clone, Debug, PartialEq)]
pub struct TmaInterval {
    pub start_ns: u64,
    pub end_ns: u64,
    pub values: Vec<Option<f64>>,
}

#[derive(Clone, Debug)]
pub struct TmaData {
    /// Hierarchy rows (levels 1..3) in the order the vendor spec declares them.
    pub rows: Vec<TmaSummaryRow>,
    /// Indices into `rows` of the level-1 metrics.
    pub level1: Vec<usize>,
    pub intervals: Vec<TmaInterval>,
    functions: FunctionValues,
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
        let level1_names: Vec<&str> = level1
            .iter()
            .map(|index| summary.rows[*index].name.as_str())
            .collect();

        Some(Self {
            intervals: load_intervals(connection, &level1_names),
            functions: load_functions(connection, &summary.rows, &level1),
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

    /// Whether any sampled function has values for the level-1 columns.
    pub fn has_functions(&self) -> bool {
        self.functions
            .values()
            .any(|module| module.values().any(|function| function.level1.is_some()))
    }

    /// Recording-wide values for a sampled function, aligned with `level1`.
    pub fn function_values(&self, frame: &ProfileFrame) -> Option<&[Option<f64>]> {
        self.function_metrics(frame)?.level1.as_deref()
    }

    /// Recording-wide values for a sampled function, aligned with `rows`.
    pub fn function_hierarchy_values(&self, frame: &ProfileFrame) -> Option<&[Option<f64>]> {
        Some(&self.function_metrics(frame)?.hierarchy)
    }

    fn function_metrics(&self, frame: &ProfileFrame) -> Option<&FunctionMetrics> {
        let symbol = frame.symbol_name.as_deref().unwrap_or(&frame.name);
        self.functions.get(&frame.module)?.get(symbol)
    }

    /// The chain of dominant metrics: the largest level-1 metric, then the
    /// largest of its children, and so on. Vendors expose different depths, so
    /// this walks the dotted names rather than a fixed tree.
    pub fn dominant_path(&self, value: impl Fn(usize) -> Option<f64>) -> Vec<usize> {
        let mut path = Vec::new();
        let mut parent = None;
        loop {
            let next = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| match parent {
                    None => row.level == 1,
                    Some(parent) => is_child_of(&row.name, parent),
                })
                .filter_map(|(index, _)| Some((index, value(index)?)))
                .max_by(|left, right| left.1.total_cmp(&right.1));
            let Some((index, _)) = next else {
                return path;
            };
            path.push(index);
            parent = Some(self.rows[index].name.as_str());
        }
    }
}

pub fn is_child_of(name: &str, parent: &str) -> bool {
    name.strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|rest| !rest.contains('.'))
}

fn load_intervals(connection: &Connection, names: &[&str]) -> Vec<TmaInterval> {
    let Ok(mut statement) = connection.prepare(
        "SELECT TRY_CAST(start_ns AS UBIGINT), metric, TRY_CAST(value AS DOUBLE)
         FROM tma_intervals",
    ) else {
        return Vec::new();
    };
    let Ok(mut rows) = statement.query([]) else {
        return Vec::new();
    };
    let mut by_start = BTreeMap::<u64, Vec<Option<f64>>>::new();
    while let Ok(Some(row)) = rows.next() {
        let Ok(metric) = row.get::<_, String>(1) else {
            continue;
        };
        let Some(column) = names.iter().position(|name| *name == metric) else {
            continue;
        };
        let Some(value) = row
            .get::<_, Option<f64>>(2)
            .ok()
            .flatten()
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let Ok(Some(start_ns)) = row.get::<_, Option<u64>>(0) else {
            continue;
        };
        by_start
            .entry(start_ns)
            .or_insert_with(|| vec![None; names.len()])[column] = Some(value);
    }
    by_start
        .into_iter()
        .map(|(start_ns, values)| TmaInterval {
            start_ns,
            end_ns: start_ns.saturating_add(INTERVAL_NS),
            values,
        })
        .collect()
}

fn load_functions(
    connection: &Connection,
    hierarchy: &[TmaSummaryRow],
    level1: &[usize],
) -> FunctionValues {
    let columns = table_columns(connection, "tma");
    if hierarchy.is_empty() || columns.is_empty() {
        return HashMap::new();
    }
    let has_column = |name: &str| columns.iter().any(|column| column.name == name);
    let metrics = hierarchy
        .iter()
        .map(|metric| {
            let column = metric.name.replace('.', "_");
            if has_column(&column) {
                format!("TRY_CAST(tma.{} AS DOUBLE)", quote_identifier(&column))
            } else {
                "NULL::DOUBLE".to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = if has_column("symbol_name") && has_column("module_path") {
        format!("SELECT symbol_name, module_path, {metrics} FROM tma")
    } else {
        legacy_functions_query(connection, &metrics)
    };
    let Ok(mut statement) = connection.prepare(&sql) else {
        return HashMap::new();
    };
    let Ok(mut rows) = statement.query([]) else {
        return HashMap::new();
    };
    let mut functions = FunctionValues::new();
    while let Ok(Some(row)) = rows.next() {
        let (Ok(symbol), Ok(module)) = (row.get::<_, String>(0), row.get::<_, Option<String>>(1))
        else {
            continue;
        };
        let values: Vec<_> = (2..2 + hierarchy.len())
            .map(|column| {
                row.get::<_, Option<f64>>(column)
                    .ok()
                    .flatten()
                    .filter(|value| value.is_finite())
            })
            .collect();
        if values.iter().any(Option::is_some) {
            let roots: Vec<_> = level1.iter().map(|&index| values[index]).collect();
            functions.entry(module).or_default().insert(
                symbol,
                FunctionMetrics {
                    hierarchy: values,
                    level1: roots.iter().any(Option::is_some).then_some(roots),
                },
            );
        }
    }
    functions
}

fn legacy_functions_query(connection: &Connection, metrics: &str) -> String {
    let columns = table_columns(connection, "proc_map");
    let has_column = |name: &str| columns.iter().any(|column| column.name == name);
    let symbol_column = if has_column("symbol_name") {
        "symbol_name"
    } else {
        "NULL"
    };
    let module_column = if has_column("module_path") {
        "module_path"
    } else {
        "NULL"
    };
    // Older captures grouped metrics by display name. Only resolve rows whose
    // sampled name and function identity still have a one-to-one mapping.
    format!(
        "WITH identities AS (
             SELECT DISTINCT func_name,
                    COALESCE({symbol_column}, CASE WHEN func_name = '[unknown]'
                             THEN printf('0x%x', ip) ELSE func_name END) AS symbol,
                    {module_column} AS module
             FROM proc_map WHERE ip IN (SELECT ip FROM pmu_counters)
         )
         SELECT identities.symbol, identities.module, {metrics}
         FROM tma JOIN identities USING (func_name)
         QUALIFY COUNT(*) OVER (PARTITION BY func_name) = 1
             AND COUNT(*) OVER (PARTITION BY identities.symbol, identities.module) = 1"
    )
}
