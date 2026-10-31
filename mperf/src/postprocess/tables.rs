use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use store::Session;
use store::arrow::record_batch::RecordBatch;
use store::duckdb::Connection;

/// The session directory being materialized: an in-memory DuckDB with one view
/// per Parquet table, and the directory the derived tables are written into.
pub(crate) struct Tables {
    dir: PathBuf,
    session: Session,
}

impl Tables {
    pub(crate) fn open(dir: &Path) -> Result<Tables> {
        Ok(Tables {
            dir: dir.to_owned(),
            session: Session::open(dir)?,
        })
    }

    pub(crate) fn connection(&self) -> &Connection {
        self.session.connection()
    }

    pub(crate) fn has_table(&self, name: &str) -> bool {
        self.connection()
            .query_row(
                "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = ?",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .is_ok_and(|count| count > 0)
    }

    /// Write one table as `<name>.parquet` and expose it as a view.
    pub(crate) fn write(&self, name: &str, batch: RecordBatch) -> Result<()> {
        let path = store::write_table(&self.dir, name, &[batch])?;
        self.register(name, std::slice::from_ref(&path))
    }

    /// Materialize a query as `<name>.parquet` and expose it as a view.
    pub(crate) fn write_query(&self, name: &str, select: &str) -> Result<()> {
        let path = self.dir.join(format!("{name}.parquet"));
        self.connection()
            .execute_batch(&format!(
                "COPY ({select}) TO '{}' (FORMAT PARQUET);",
                escape(&path)
            ))
            .with_context(|| format!("failed to materialize table '{name}'"))?;
        self.register(name, std::slice::from_ref(&path))
    }

    pub(crate) fn register(&self, name: &str, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let list = files
            .iter()
            .map(|file| format!("'{}'", escape(file)))
            .collect::<Vec<_>>()
            .join(", ");
        self.connection()
            .execute_batch(&format!(
                "CREATE OR REPLACE VIEW \"{name}\" AS SELECT * FROM read_parquet([{list}]);"
            ))
            .with_context(|| format!("failed to expose table '{name}'"))?;
        Ok(())
    }

    pub(crate) fn columns(&self, table: &str) -> Vec<String> {
        let Ok(mut statement) = self.connection().prepare(&format!(
            "PRAGMA table_info('{}')",
            table.replace('\'', "''")
        )) else {
            return Vec::new();
        };
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn scalar_f64(&self, sql: &str) -> Option<f64> {
        let connection = self.connection();
        connection
            .query_row(sql, [], |row| row.get::<_, Option<f64>>(0))
            .or_else(|_| {
                connection.query_row(sql, [], |row| {
                    row.get::<_, Option<i64>>(0)
                        .map(|value| value.map(|v| v as f64))
                })
            })
            .ok()
            .flatten()
    }
}

fn escape(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

/// SQL identifier quoting, shared by every generated statement.
pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
