pub(crate) use store::duckdb::types::Value;
pub(crate) use store::duckdb::{Connection, Result as SqlResult, Row};

pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) declared_type: String,
}

/// Columns of `table`, empty when the table or view does not exist.
pub(crate) fn table_columns(connection: &Connection, table: &str) -> Vec<Column> {
    let escaped = table.replace('\'', "''");
    let Ok(mut statement) = connection.prepare(&format!("PRAGMA table_info('{escaped}')")) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok(Column {
            name: row.get("name")?,
            declared_type: row.get("type")?,
        })
    }) else {
        return Vec::new();
    };
    rows.filter_map(|row| row.ok()).collect()
}

/// The first `count` columns of `row` as dynamically typed values.
pub(crate) fn row_values(row: &Row<'_>, count: usize) -> SqlResult<Vec<Value>> {
    (0..count).map(|index| row.get::<_, Value>(index)).collect()
}

pub(crate) fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::TinyInt(value) => Some(*value as i64),
        Value::SmallInt(value) => Some(*value as i64),
        Value::Int(value) => Some(*value as i64),
        Value::BigInt(value) => Some(*value),
        Value::HugeInt(value) => i64::try_from(*value).ok(),
        Value::UTinyInt(value) => Some(*value as i64),
        Value::USmallInt(value) => Some(*value as i64),
        Value::UInt(value) => Some(*value as i64),
        Value::UBigInt(value) => Some(*value as i64),
        Value::UHugeInt(value) => i64::try_from(*value).ok(),
        Value::Float(value) => Some(*value as i64),
        Value::Double(value) => Some(*value as i64),
        _ => None,
    }
}

/// Raw 64-bit payloads (instruction pointers) survive both the signed and the
/// unsigned DuckDB integer types they may arrive in.
pub(crate) fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::UBigInt(value) => Some(*value),
        Value::UHugeInt(value) => u64::try_from(*value).ok(),
        _ => as_i64(value).map(|value| value as u64),
    }
}

pub(crate) fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Float(value) => Some(*value as f64),
        Value::Double(value) => Some(*value),
        Value::UBigInt(value) => Some(*value as f64),
        Value::UHugeInt(value) => Some(*value as f64),
        Value::HugeInt(value) => Some(*value as f64),
        Value::Decimal(value) => value.to_string().parse().ok(),
        _ => as_i64(value).map(|value| value as f64),
    }
}

pub(crate) fn as_text(value: &Value) -> Option<&str> {
    match value {
        Value::Text(value) | Value::Enum(value) => Some(value),
        _ => None,
    }
}
