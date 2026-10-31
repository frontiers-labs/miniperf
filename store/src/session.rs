use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use duckdb::Connection;

use crate::arrow::record_batch::RecordBatch;
use crate::query::{quote_identifier, table_columns};

/// Logical table name of a session Parquet file: the file stem with every
/// trailing `-<number>` (pid, segment) component removed.
pub fn table_base_name(file_stem: &str) -> &str {
    let mut base = file_stem;
    while let Some((head, tail)) = base.rsplit_once('-') {
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            base = head;
        } else {
            break;
        }
    }
    base
}

/// A session directory opened for querying: an in-memory DuckDB with one view
/// per logical table found in the directory.
pub struct Session {
    connection: Connection,
    dir: PathBuf,
}

impl Session {
    pub fn open(dir: &Path) -> Result<Session> {
        let connection = Connection::open_in_memory().context("failed to open DuckDB")?;
        let mut tables: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read session directory {}", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "parquet") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            tables
                .entry(table_base_name(stem).to_owned())
                .or_default()
                .push(path);
        }
        let session = Session {
            connection,
            dir: dir.to_owned(),
        };
        for (table, mut files) in tables {
            files.sort();
            register_view(session.connection(), &table, &files, ViewMode::Create)
                .with_context(|| format!("failed to create view over table '{table}'"))?;
        }
        Ok(session)
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Whether the session currently exposes the named table view.
    pub fn has_table(&self, name: &str) -> bool {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = ?",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .is_ok_and(|count| count > 0)
    }

    /// Write a derived table into this session and expose its view.
    pub fn write(&self, name: &str, batch: RecordBatch) -> Result<()> {
        let path = crate::write_table(&self.dir, name, &[batch])?;
        self.register(name, std::slice::from_ref(&path))
    }

    /// Materialize a query as Parquet and expose its view in this session.
    pub fn write_query(&self, name: &str, select: &str) -> Result<()> {
        let path = self.dir.join(format!("{name}.parquet"));
        self.connection
            .execute_batch(&format!(
                "COPY ({select}) TO '{}' (FORMAT PARQUET);",
                escape(&path),
            ))
            .with_context(|| format!("failed to materialize table '{name}'"))?;
        self.register(name, std::slice::from_ref(&path))
    }

    /// Expose existing Parquet files as one logical table view.
    pub fn register(&self, name: &str, files: &[PathBuf]) -> Result<()> {
        register_view(&self.connection, name, files, ViewMode::Replace)
            .with_context(|| format!("failed to expose table '{name}'"))
    }

    /// Column names in schema order, empty when schema information is unavailable.
    pub fn columns(&self, table: &str) -> Vec<String> {
        table_columns(&self.connection, table)
            .unwrap_or_default()
            .into_iter()
            .map(|column| column.name)
            .collect()
    }

    /// Read an optional numeric scalar, treating query or conversion errors as unavailable.
    pub fn scalar_f64(&self, sql: &str) -> Option<f64> {
        self.connection
            .query_row(sql, [], |row| row.get::<_, Option<f64>>(0))
            .or_else(|_| {
                self.connection.query_row(sql, [], |row| {
                    row.get::<_, Option<i64>>(0)
                        .map(|value| value.map(|v| v as f64))
                })
            })
            .ok()
            .flatten()
    }
}

enum ViewMode {
    Create,
    Replace,
}

fn register_view(
    connection: &Connection,
    name: &str,
    files: &[PathBuf],
    mode: ViewMode,
) -> duckdb::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let files = files
        .iter()
        .map(|file| format!("'{}'", escape(file)))
        .collect::<Vec<_>>()
        .join(", ");
    let command = match mode {
        ViewMode::Create => "CREATE VIEW",
        ViewMode::Replace => "CREATE OR REPLACE VIEW",
    };
    connection.execute_batch(&format!(
        "{command} {} AS SELECT * FROM read_parquet([{files}]);",
        quote_identifier(name),
    ))
}

fn escape(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}
