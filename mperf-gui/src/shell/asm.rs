//! Lazy per-function disassembly. The session drops its database connection
//! after loading, so each listing re-opens the session on the background
//! executor and joins `assembly_lines` against `assembly_samples`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    source::SourceLocation,
    sql::{Connection, Row, Value, as_i64, as_text, as_u64},
};

#[derive(Clone, Debug)]
pub struct AsmInstruction {
    pub address: u64,
    pub text: String,
    source: Option<InstructionSource>,
    pub samples: u64,
    pub llc_misses: u64,
}

#[derive(Clone, Debug)]
pub(super) struct InstructionSource {
    pub location: SourceLocation,
    pub inline_callsite: bool,
}

impl AsmInstruction {
    pub fn line_in(&self, file: &Path) -> Option<usize> {
        let source = &self.source.as_ref()?.location;
        (source.path == file).then_some(source.line)
    }

    pub(super) fn source(&self) -> Option<&InstructionSource> {
        self.source.as_ref()
    }
}

/// One function's instructions plus the per-source-line heat derived from them.
#[derive(Clone, Debug)]
pub struct AsmListing {
    pub instructions: Vec<AsmInstruction>,
    pub source_file: Option<PathBuf>,
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
pub fn load(
    result_directory: &Path,
    module: &str,
    function: &str,
    preferred_source: Option<&Path>,
) -> Result<AsmListing> {
    let session = store::Session::open(result_directory)
        .with_context(|| format!("failed to open {}", result_directory.display()))?;
    let connection = session.connection();

    let mut symbols = query_symbols(connection, module, function)?;
    if symbols.is_empty() {
        symbols.push(function.to_owned());
    }

    let mut instructions = Vec::new();
    let mut source_lines = None;
    for symbol in &symbols {
        let mut statement = connection.prepare(
            "SELECT l.runtime_address, l.rel_address, l.instruction, l.source_file, l.source_line,
                    p.file_name, p.line,
                    COALESCE(s.samples, 0), COALESCE(s.llc_misses, 0)
             FROM assembly_lines l
             LEFT JOIN assembly_samples s
               ON s.module_path = l.module_path AND s.address = l.runtime_address
             LEFT JOIN proc_map p
               ON p.module_path = l.module_path AND p.ip = l.runtime_address
             WHERE l.module_path = ? AND l.symbol = ?
             ORDER BY l.runtime_address;",
        )?;
        let mut rows = statement.query(store::duckdb::params![module, symbol])?;
        while let Some(row) = rows.next()? {
            let address = as_u64(&row.get::<_, Value>(0)?).unwrap_or_default();
            let relative_address = as_u64(&row.get::<_, Value>(1)?);
            let recorded = recorded_source(row, 3)?
                .or(recorded_source(row, 5)?)
                .map(|location| InstructionSource {
                    location,
                    inline_callsite: false,
                });
            let matches_file = |source: &InstructionSource| {
                preferred_source.is_none_or(|file| source.location.path == file)
            };
            let source = if recorded.as_ref().is_some_and(matches_file) {
                recorded
            } else {
                let resolved = (|| {
                    let loader = source_lines
                        .get_or_insert_with(|| symbolize::SourceLines::open(Path::new(module)))
                        .as_ref()?;
                    let source = relative_address
                        .and_then(|relative| loader.find(relative, symbol, preferred_source))
                        .or_else(|| loader.find(address, symbol, preferred_source))?;
                    Some(InstructionSource {
                        location: SourceLocation {
                            path: PathBuf::from(source.file),
                            line: source.line as usize,
                        },
                        inline_callsite: source.inline_callsite,
                    })
                })();
                resolved
                    .filter(|source| recorded.is_none() || matches_file(source))
                    .or(recorded)
            };
            instructions.push(AsmInstruction {
                address,
                text: as_text(&row.get::<_, Value>(2)?)
                    .unwrap_or_default()
                    .to_owned(),
                source,
                samples: as_i64(&row.get::<_, Value>(7)?).unwrap_or_default().max(0) as u64,
                llc_misses: as_i64(&row.get::<_, Value>(8)?).unwrap_or_default().max(0) as u64,
            });
        }
    }
    instructions.sort_by_key(|instruction| instruction.address);

    let source_file = preferred_source.map(Path::to_path_buf).or_else(|| {
        let mut counts = BTreeMap::<&Path, usize>::new();
        for source in instructions
            .iter()
            .filter_map(|instruction| instruction.source.as_ref())
        {
            *counts.entry(&source.location.path).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(file, _)| file.to_path_buf())
    });

    let mut line_samples = BTreeMap::<usize, u64>::new();
    let mut total_samples = 0u64;
    let mut total_llc_misses = 0u64;
    for instruction in &instructions {
        total_samples = total_samples.saturating_add(instruction.samples);
        total_llc_misses = total_llc_misses.saturating_add(instruction.llc_misses);
        if let Some(line) = source_file
            .as_deref()
            .and_then(|file| instruction.line_in(file))
        {
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

fn recorded_source(
    row: &Row<'_>,
    file_column: usize,
) -> store::duckdb::Result<Option<SourceLocation>> {
    let file = row.get::<_, Value>(file_column)?;
    let line = as_i64(&row.get::<_, Value>(file_column + 1)?)
        .and_then(|line| usize::try_from(line).ok())
        .filter(|line| *line > 0);
    Ok(as_text(&file)
        .filter(|file| !file.is_empty())
        .zip(line)
        .map(|(file, line)| SourceLocation {
            path: PathBuf::from(file),
            line,
        }))
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
