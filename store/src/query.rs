//! Schema inspection and SQL identifier handling shared by recording readers.

use duckdb::Connection;

/// One column exposed by a recording table or view.
pub struct Column {
    /// Column identifier as stored in the table schema.
    pub name: String,
    /// DuckDB's declared SQL type.
    pub declared_type: String,
}

/// Inspect a table's columns in schema order, propagating query and decode errors.
pub fn table_columns(connection: &Connection, table: &str) -> duckdb::Result<Vec<Column>> {
    let escaped = table.replace('\'', "''");
    let mut statement = connection.prepare(&format!("PRAGMA table_info('{escaped}')"))?;
    statement
        .query_map([], |row| {
            Ok(Column {
                name: row.get("name")?,
                declared_type: row.get("type")?,
            })
        })?
        .collect()
}

/// Quote a SQL identifier, including any embedded double quotes.
pub fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
