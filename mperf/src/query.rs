use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use comfy_table::{
    Attribute, Cell, CellAlignment, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED,
};
use mperf_data::{RecordInfo, ScenarioInfo};
use num_format::{Locale, ToFormattedString};
use pmu_data::ValueFormat;
use serde::Serialize;
use serde_json::{Map, Value as JsonValue};
use store::duckdb::{Connection, types::Value as DuckValue};

pub const MAX_ROWS: usize = 10_000;

pub fn parse_max_rows(value: &str) -> std::result::Result<usize, String> {
    let rows = value
        .parse::<usize>()
        .map_err(|_| format!("'{value}' is not a positive row count"))?;
    if !(1..=MAX_ROWS).contains(&rows) {
        return Err(format!("row count must be between 1 and {MAX_ROWS}"));
    }
    Ok(rows)
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum QueryFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug)]
struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
}

/// Rendering-oriented projection of a DuckDB result value.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
    Binary(Vec<u8>),
}

impl From<DuckValue> for Value {
    fn from(value: DuckValue) -> Self {
        match value {
            DuckValue::Null => Value::Null,
            DuckValue::Boolean(value) => Value::Integer(value as i64),
            DuckValue::TinyInt(value) => Value::Integer(value as i64),
            DuckValue::SmallInt(value) => Value::Integer(value as i64),
            DuckValue::Int(value) => Value::Integer(value as i64),
            DuckValue::BigInt(value) => Value::Integer(value),
            DuckValue::UTinyInt(value) => Value::Integer(value as i64),
            DuckValue::USmallInt(value) => Value::Integer(value as i64),
            DuckValue::UInt(value) => Value::Integer(value as i64),
            DuckValue::UBigInt(value) => Value::Unsigned(value),
            DuckValue::HugeInt(value) => i64::try_from(value)
                .map_or_else(|_| Value::String(value.to_string()), Value::Integer),
            DuckValue::UHugeInt(value) => u64::try_from(value)
                .map_or_else(|_| Value::String(value.to_string()), Value::Unsigned),
            DuckValue::Float(value) => Value::Float(value as f64),
            DuckValue::Double(value) => Value::Float(value),
            DuckValue::Text(value) | DuckValue::Enum(value) => Value::String(value),
            DuckValue::Blob(bytes) | DuckValue::Geometry(bytes) => Value::Binary(bytes),
            other => Value::String(format!("{other:?}")),
        }
    }
}

#[derive(Serialize)]
struct JsonEnvelope {
    schema_version: u32,
    record_format_version: u32,
    columns: Vec<JsonColumn>,
    rows: Vec<Map<String, JsonValue>>,
    row_count: usize,
    max_rows: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct JsonColumn {
    name: String,
    #[serde(rename = "type")]
    ty: &'static str,
}

pub fn do_query(
    arguments: Vec<String>,
    file: Option<String>,
    format: QueryFormat,
    max_rows: usize,
) -> Result<()> {
    if arguments.as_slice() == ["help"] && file.is_none() {
        print!("{QUERY_GUIDE}");
        return Ok(());
    }

    let (result_directory, inline_sql) = match arguments.as_slice() {
        [result_directory] => (result_directory.as_str(), None),
        [result_directory, sql] => (result_directory.as_str(), Some(sql.as_str())),
        [] => {
            bail!("missing result directory and SQL statement; run `mperf query help` for examples")
        }
        _ => unreachable!("clap limits query positional arguments"),
    };
    if file.is_some() && inline_sql.is_some() {
        bail!("SQL must be supplied either inline or with --file, not both");
    }
    let sql = match (inline_sql, file.as_deref()) {
        (Some(sql), None) => sql.to_owned(),
        (None, Some("-")) => {
            let mut sql = String::new();
            io::stdin()
                .read_to_string(&mut sql)
                .context("failed to read SQL from stdin")?;
            sql
        }
        (None, Some(path)) => {
            fs::read_to_string(path).with_context(|| format!("failed to read SQL file '{path}'"))?
        }
        (None, None) => bail!(
            "missing SQL statement; provide quoted SQL or --file PATH, or run `mperf query help`"
        ),
        (Some(_), Some(_)) => unreachable!(),
    };

    let result_directory = Path::new(result_directory);
    let info = load_record_info(result_directory)?;
    let session = store::Session::open(result_directory)
        .with_context(|| format!("failed to open {}", result_directory.display()))?;

    let result = execute(session.connection(), &sql, max_rows)?;
    match format {
        QueryFormat::Text => {
            let formats = known_formats(&info);
            print!("{}", render_text(&result, max_rows, &formats));
        }
        QueryFormat::Json => {
            let envelope = json_envelope(result, max_rows, info.format_version)?;
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
    }
    Ok(())
}

fn load_record_info(result_directory: &Path) -> Result<RecordInfo> {
    let info_path = result_directory.join("info.json");
    let data = fs::read_to_string(&info_path)
        .with_context(|| format!("failed to read {}", info_path.display()))?;
    let info: RecordInfo = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", info_path.display()))?;
    info.ensure_supported_format()?;
    Ok(info)
}

fn execute(connection: &Connection, sql: &str, max_rows: usize) -> Result<QueryResult> {
    validate_single_statement(sql)?;
    let mut statement = connection
        .prepare(sql)
        .with_context(|| format!("failed to prepare query: {}", one_line(sql)))?;
    let mut result_rows = statement.query([]).context("query execution failed")?;
    let columns = result_rows
        .as_ref()
        .map(|statement| statement.column_names())
        .unwrap_or_default();
    if columns.is_empty() {
        bail!("statement does not return rows; only read-only row-producing SQL is accepted");
    }
    let mut seen = HashSet::with_capacity(columns.len());
    if let Some(duplicate) = columns.iter().find(|name| !seen.insert((*name).clone())) {
        bail!(
            "duplicate result column '{duplicate}'; give every selected expression a unique AS alias"
        );
    }

    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = result_rows.next().context("query execution failed")? {
        if rows.len() == max_rows {
            truncated = true;
            break;
        }
        let row = (0..columns.len())
            .map(|index| row.get::<_, DuckValue>(index).map(Value::from))
            .collect::<store::duckdb::Result<Vec<_>>>()
            .context("failed to read query result")?;
        rows.push(row);
    }
    Ok(QueryResult {
        columns,
        rows,
        truncated,
    })
}

fn validate_single_statement(sql: &str) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
        Backtick,
        Bracket,
    }

    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut statement_ended = false;
    let mut has_sql = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            let closing = match active {
                Quote::Single => b'\'',
                Quote::Double => b'"',
                Quote::Backtick => b'`',
                Quote::Bracket => b']',
            };
            if byte == closing {
                if !matches!(active, Quote::Bracket)
                    && bytes.get(index + 1).copied() == Some(closing)
                {
                    index += 2;
                    continue;
                }
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            let mut closed = false;
            while index + 1 < bytes.len() {
                if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    index += 2;
                    closed = true;
                    break;
                }
                index += 1;
            }
            if !closed {
                bail!("unterminated SQL block comment");
            }
            continue;
        }
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if statement_ended {
            bail!("multiple SQL statements are not accepted");
        }
        match byte {
            b'\'' => quote = Some(Quote::Single),
            b'"' => quote = Some(Quote::Double),
            b'`' => quote = Some(Quote::Backtick),
            b'[' => quote = Some(Quote::Bracket),
            b';' => statement_ended = true,
            _ => has_sql = true,
        }
        index += 1;
    }
    if quote.is_some() {
        bail!("unterminated quoted SQL value or identifier");
    }
    if !has_sql {
        bail!("SQL statement is empty");
    }
    Ok(())
}

fn one_line(sql: &str) -> String {
    const MAX_CHARS: usize = 160;
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        format!("{}…", compact.chars().take(MAX_CHARS).collect::<String>())
    }
}

fn render_text(
    result: &QueryResult,
    max_rows: usize,
    formats: &HashMap<String, ValueFormat>,
) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    if io::stdout().is_terminal()
        && let Ok((width, _)) = crossterm::terminal::size()
    {
        table.set_width(width);
    }
    table.set_header(result.columns.iter().map(|column| {
        Cell::new(column)
            .add_attribute(Attribute::Bold)
            .set_alignment(CellAlignment::Center)
    }));
    for row in &result.rows {
        table.add_row(row.iter().enumerate().map(|(index, value)| {
            let format = formats
                .get(&result.columns[index])
                .cloned()
                .unwrap_or_default();
            let alignment = if matches!(
                value,
                Value::Integer(_) | Value::Unsigned(_) | Value::Float(_)
            ) {
                CellAlignment::Right
            } else {
                CellAlignment::Left
            };
            Cell::new(format_value(value, &format)).set_alignment(alignment)
        }));
    }

    let mut output = table.to_string();
    output.push('\n');
    if result.truncated {
        output.push_str(&format!(
            "{} rows shown (truncated at --max-rows {})\n",
            result.rows.len(),
            max_rows
        ));
    } else {
        output.push_str(&format!("{} rows\n", result.rows.len()));
    }
    output
}

fn format_value(value: &Value, format: &ValueFormat) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Binary(bytes) => encode_blob(bytes),
        Value::String(value) => value.clone(),
        Value::Integer(value) => value.to_formatted_string(&Locale::en),
        Value::Unsigned(value) => value.to_formatted_string(&Locale::en),
        Value::Float(value) => match format {
            ValueFormat::Percent => format!("{:.2}%", value * 100.0),
            ValueFormat::Percent1 => format!("{:.1}%", value * 100.0),
            ValueFormat::Percent2 => format!("{:.2}%", value * 100.0),
            ValueFormat::Percent3 => format!("{:.3}%", value * 100.0),
            ValueFormat::Float1 => format!("{value:.1}"),
            ValueFormat::Float2 => format!("{value:.2}"),
            ValueFormat::Float3 => format!("{value:.3}"),
            _ => format!("{value:.6}"),
        },
    }
}

fn known_formats(info: &RecordInfo) -> HashMap<String, ValueFormat> {
    let mut formats = HashMap::from([
        ("total".to_owned(), ValueFormat::Percent2),
        ("branch_miss_rate".to_owned(), ValueFormat::Percent2),
        ("cache_miss_rate".to_owned(), ValueFormat::Percent2),
        ("ipc".to_owned(), ValueFormat::Float2),
        ("branch_mpki".to_owned(), ValueFormat::Float2),
        ("cache_mpki".to_owned(), ValueFormat::Float2),
    ]);
    if let ScenarioInfo::TMA(tma) = &info.scenario_info {
        if let Some(ui) = &tma.ui {
            for tab in &ui.tabs {
                if let pmu_data::TabSpec::MetricsTable(table) = tab {
                    for column in &table.columns {
                        formats.insert(column.key.clone(), column.format.clone());
                    }
                }
            }
        } else {
            for metric in &tma.metrics {
                formats.insert(metric.name.replace('.', "_"), ValueFormat::Percent2);
            }
        }
    }
    formats
}

fn json_envelope(
    result: QueryResult,
    max_rows: usize,
    record_format_version: u32,
) -> Result<JsonEnvelope> {
    let column_types = (0..result.columns.len())
        .map(|index| infer_column_type(&result.rows, index))
        .collect::<Vec<_>>();
    let rows = result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .zip(row)
                .map(|(name, value)| Ok((name.clone(), json_value(value)?)))
                .collect::<Result<Map<_, _>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    let columns = result
        .columns
        .into_iter()
        .zip(column_types)
        .map(|(name, ty)| JsonColumn { name, ty })
        .collect();
    Ok(JsonEnvelope {
        schema_version: 1,
        record_format_version,
        columns,
        row_count: rows.len(),
        rows,
        max_rows,
        truncated: result.truncated,
    })
}

fn infer_column_type(rows: &[Vec<Value>], index: usize) -> &'static str {
    let mut inferred = None;
    for value in rows {
        let ty = match value[index] {
            Value::Null => continue,
            Value::Binary(_) => "binary",
            Value::Float(_) => "number",
            Value::Integer(_) | Value::Unsigned(_) => "integer",
            Value::String(_) => "string",
        };
        inferred = match inferred {
            None => Some(ty),
            Some("integer") if ty == "number" => Some("number"),
            Some("number") if ty == "integer" => Some("number"),
            Some(previous) if previous == ty => Some(previous),
            Some(_) => return "mixed",
        };
    }
    inferred.unwrap_or("null")
}

fn json_value(value: &Value) -> Result<JsonValue> {
    Ok(match value {
        Value::Null => JsonValue::Null,
        Value::Binary(bytes) => JsonValue::String(encode_blob(bytes)),
        Value::String(value) => JsonValue::String(value.clone()),
        Value::Integer(value) => JsonValue::from(*value),
        Value::Unsigned(value) => JsonValue::from(*value),
        Value::Float(value) => {
            JsonValue::Number(serde_json::Number::from_f64(*value).ok_or_else(|| {
                anyhow::anyhow!("query returned a non-finite floating-point value")
            })?)
        }
    })
}

fn encode_blob(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        use std::fmt::Write;
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

const QUERY_GUIDE: &str = r#"mperf query — read-only SQL guide

USAGE
  mperf query [--format text|json] [--max-rows N] RESULTS 'SQL'
  mperf query [--format text|json] [--max-rows N] RESULTS --file PATH
  mperf query RESULTS --file -
  mperf query help

The command opens the RESULTS session directory as an in-memory DuckDB
database with one read-only view per Parquet table and accepts one
row-producing DuckDB statement. SELECT, VALUES, WITH ... SELECT, EXPLAIN, and
read-only PRAGMA statements are supported. Writes and multiple statements are
rejected. The default output cap is 50 rows; --max-rows accepts values up to
10000.

Every dataset below is a RESULTS/<name>.parquet file, so read_parquet() and
the other DuckDB file functions work on the session directory directly.

DISCOVER THE RECORDING SCHEMA
  mperf query ./results "SELECT view_name AS name FROM duckdb_views()
                         WHERE NOT internal ORDER BY name"

  mperf query ./results "PRAGMA table_info('hotspots')"

  mperf query ./results "SELECT * FROM read_parquet('./results/tma.parquet')
                         LIMIT 5"

COMMON DATASETS
  hotspots              Snapshot/Roofline function-level performance
  pmu_counters           Processed PMU sample groups and recorded counters
  cpu_observations       Stack-independent logical-CPU activity observations
  derived_metrics        Recording-wide derived values and their formulas
  tma                    TMA function-level metrics
  tma_summary            TMA recording-wide values and dominant verdict
  tma_intervals          TMA values grouped into one-second intervals
  roofline               Roofline loop throughput and arithmetic intensity
  assembly_address_stats Sample counts and counters by assembly address
  proc_map               Symbol and source information by instruction pointer
  snapshot_processes     Observed process-tree membership and coverage quality
  snapshot_resource_samples  Normalized CPU/memory/disk/network/uncore timeline
  snapshot_summary       Recording-wide peaks and totals, per resource_id
  snapshot_findings      Ranked USE findings and next-measurement guidance
  snapshot_collectors    Collector availability, provenance, and remediation
  capture_fidelity       Chosen capture strategy and why better ones were rejected
  mem_samples            Precise memory samples: data address, level, latency
  alloc_site_memory      Miss counts and latency per allocation site
  cacheline_contention   Cache lines shared across threads (false-sharing candidates)

EXAMPLES
  # Largest function hotspots
  mperf query ./results \
    'SELECT func_name, total, cycles, instructions, ipc
     FROM hotspots ORDER BY total DESC LIMIT 20'

  # Snapshot diagnosis and unavailable collectors
  mperf query ./results \
    'SELECT rank, severity, resource, finding, evidence, recommendation
     FROM snapshot_findings ORDER BY rank'

  mperf query ./results \
    "SELECT name, status, source, message FROM snapshot_collectors
     WHERE status <> 'available'"

  # Capture fidelity of the recording
  mperf query ./results \
    'SELECT scenario, rung, status, reason FROM capture_fidelity'

  # Filter function names
  mperf query ./results \
    "SELECT func_name, total FROM hotspots
     WHERE func_name LIKE '%parse%' ORDER BY total DESC"

  # Recording-wide metrics, including formulas
  mperf query ./results \
    'SELECT name, value, unit, expression
     FROM derived_metrics ORDER BY name'

  # TMA verdict and timeline
  mperf query --format json ./results \
    'SELECT metric, value, verdict
     FROM tma_summary ORDER BY value DESC'

  mperf query ./results \
    "SELECT start_ns, metric, value FROM tma_intervals
     WHERE metric = 'retiring' ORDER BY start_ns"

  # Roofline loop ranking
  mperf query ./results \
    'SELECT function_name, file_name, line, vector_double_ops, vector_double_ai
     FROM roofline ORDER BY vector_double_ops DESC LIMIT 20'

  # Hottest assembly addresses with a window total
  mperf query ./results \
    'SELECT module_path, func_name, address, cycles,
            SUM(cycles) OVER (PARTITION BY func_name) AS function_cycles
     FROM assembly_address_stats ORDER BY cycles DESC LIMIT 30'

AUTOMATION
  JSON output is an object containing schema_version, record_format_version,
  columns, rows, row_count, max_rows, and truncated. Values remain unformatted.
  Every output expression must have a unique name; use AS aliases when needed.
  Diagnostics are written to stderr and successful results to stdout.

SHELL QUOTING
  Quote inline SQL so the shell does not interpret *, >, <, or parentheses.
  For long queries, use --file query.sql. Use --file - to read SQL from stdin.
"#;
