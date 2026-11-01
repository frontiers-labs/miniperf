//! Lazy per-function disassembly. The session drops its database connection
//! after loading, so each listing re-opens the session on the background
//! executor and joins `assembly_lines` against `assembly_samples`.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};

use crate::sql::{Connection, Value, as_i64, as_text};

#[derive(Clone, Debug)]
pub struct AsmInstruction {
    pub address: u64,
    pub text: String,
    pub line: Option<usize>,
    pub samples: u64,
    pub llc_misses: u64,
}

/// One function's instructions plus the per-source-line heat derived from them.
#[derive(Clone, Debug)]
pub struct AsmListing {
    pub instructions: Vec<AsmInstruction>,
    pub source_file: Option<String>,
    pub line_samples: BTreeMap<usize, u64>,
    pub total_samples: u64,
    pub max_line_samples: u64,
    pub max_instruction_samples: u64,
    pub total_llc_misses: u64,
}

/// Whether this recording carries disassembly at all.
pub fn has_assembly(connection: &Connection) -> bool {
    connection
        .prepare("SELECT 1 FROM assembly_lines LIMIT 1;")
        .and_then(|mut statement| Ok(statement.query([])?.next()?.is_some()))
        .unwrap_or(false)
}

/// Loads the listing for `function` in `module`. Symbols are resolved through
/// the sampled addresses first, because the disassembler's symbol names and
/// the profile's `func_name` do not always agree.
pub fn load(result_directory: &Path, module: &str, function: &str) -> Result<AsmListing> {
    let session = store::Session::open(result_directory)
        .with_context(|| format!("failed to open {}", result_directory.display()))?;
    let connection = session.connection();

    let mut symbols = query_symbols(connection, module, function)?;
    if symbols.is_empty() {
        symbols.push(function.to_owned());
    }

    let mut instructions = Vec::new();
    let mut source_files = BTreeMap::<String, usize>::new();
    for symbol in &symbols {
        let mut statement = connection.prepare(
            "SELECT l.runtime_address, l.instruction, l.source_file, l.source_line,
                    COALESCE(s.samples, 0), COALESCE(s.llc_misses, 0)
             FROM assembly_lines l
             LEFT JOIN assembly_samples s
               ON s.module_path = l.module_path AND s.address = l.runtime_address
             WHERE l.module_path = ? AND l.symbol = ?
             ORDER BY l.runtime_address;",
        )?;
        let mut rows = statement.query(store::duckdb::params![module, symbol])?;
        while let Some(row) = rows.next()? {
            let source_file = row.get::<_, Value>(2)?;
            if let Some(file) = as_text(&source_file) {
                *source_files.entry(file.to_owned()).or_default() += 1;
            }
            let line = row.get::<_, Value>(3)?;
            instructions.push(AsmInstruction {
                address: as_i64(&row.get::<_, Value>(0)?).unwrap_or_default().max(0) as u64,
                text: as_text(&row.get::<_, Value>(1)?)
                    .unwrap_or_default()
                    .to_owned(),
                line: as_i64(&line)
                    .filter(|line| *line > 0)
                    .map(|line| line as usize),
                samples: as_i64(&row.get::<_, Value>(4)?).unwrap_or_default().max(0) as u64,
                llc_misses: as_i64(&row.get::<_, Value>(5)?).unwrap_or_default().max(0) as u64,
            });
        }
    }
    instructions.sort_by_key(|instruction| instruction.address);

    let source_file = source_files
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(file, _)| file);

    let mut line_samples = BTreeMap::<usize, u64>::new();
    let mut total_samples = 0u64;
    let mut total_llc_misses = 0u64;
    for instruction in &instructions {
        total_samples = total_samples.saturating_add(instruction.samples);
        total_llc_misses = total_llc_misses.saturating_add(instruction.llc_misses);
        if let Some(line) = instruction.line {
            *line_samples.entry(line).or_default() += instruction.samples;
        }
    }

    Ok(AsmListing {
        max_line_samples: line_samples.values().copied().max().unwrap_or(0),
        max_instruction_samples: instructions
            .iter()
            .map(|instruction| instruction.samples)
            .max()
            .unwrap_or(0),
        instructions,
        source_file,
        line_samples,
        total_samples,
        total_llc_misses,
    })
}

fn query_symbols(connection: &Connection, module: &str, function: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT l.symbol
         FROM assembly_lines l
         JOIN assembly_samples s
           ON s.module_path = l.module_path AND s.address = l.runtime_address
         WHERE l.module_path = ? AND s.func_name = ? AND l.symbol IS NOT NULL;",
    )?;
    let rows = statement.query_map(store::duckdb::params![module, function], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<store::duckdb::Result<Vec<_>>>()
        .context("failed to read symbols")
}
