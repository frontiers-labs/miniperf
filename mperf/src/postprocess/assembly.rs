use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use kdam::BarExt;
use object::{Object, ObjectSymbol, SymbolKind};

use super::tables::{Columns, Tables, quote_identifier};
use crate::disassembly::{DisassembleRequest, DisassembleTarget, default_disassembler};
use crate::utils;

/// Aggregate per-address sample counts and disassemble every sampled function.
pub(crate) fn process(tables: &Tables, pb: &mut kdam::Bar) -> Result<()> {
    write_assembly_samples(tables)?;

    let modules = if tables.has_table("modules") {
        utils::load_modules(tables.connection())?
    } else {
        Vec::new()
    };
    let mut module_bias = HashMap::<String, i64>::new();
    for entry in modules {
        let load_bias = entry.address as i64 - entry.offset as i64;
        module_bias
            .entry(entry.filename)
            .and_modify(|bias| {
                if load_bias < *bias {
                    *bias = load_bias;
                }
            })
            .or_insert(load_bias);
    }

    let mut lines = AssemblyLines::default();
    let mut metadata = Vec::<(String, i64)>::new();
    let disassembler = match default_disassembler() {
        Ok(disassembler) => Some(disassembler),
        Err(err) => {
            eprintln!("skipping assembly extraction: {err}");
            None
        }
    };

    if let Some(disassembler) = disassembler {
        let mut statement = tables.connection().prepare(
            "SELECT module_path, address FROM assembly_samples ORDER BY module_path, address",
        )?;
        let sampled = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<store::duckdb::Result<Vec<_>>>()?;
        let mut sampled_addresses = HashMap::<String, Vec<u64>>::new();
        for (module_path, address) in sampled {
            sampled_addresses
                .entry(module_path)
                .or_default()
                .push(address);
        }
        let mut modules = sampled_addresses.into_iter().collect::<Vec<_>>();
        modules.sort_unstable_by(|left, right| left.0.cmp(&right.0));

        if !modules.is_empty() {
            pb.reset(Some(modules.len()));
            pb.write("Extracting assembly")?;
        }
        for (idx, (module_path, addresses)) in modules.iter().enumerate() {
            pb.update_to(idx + 1)?;
            let module_file = Path::new(module_path);
            if !module_file.exists() {
                continue;
            }

            let load_bias = module_bias.get(module_path).copied().unwrap_or(0);
            metadata.push((module_path.clone(), load_bias));

            let (targets, address_base) =
                sampled_disassembly_targets(module_file, load_bias, addresses)?;
            let request = DisassembleRequest {
                module_path: module_file.to_path_buf(),
                load_bias,
                targets,
            };
            let disassembled = match disassembler.disassemble(&request) {
                Ok(disassembled) => disassembled,
                Err(err) => {
                    eprintln!("failed to disassemble {}: {err}", module_path);
                    continue;
                }
            };

            for line in disassembled {
                let rel_address = line.rel_address.saturating_sub(address_base);
                let Some(runtime_address) = apply_load_bias(rel_address, load_bias) else {
                    continue;
                };
                lines.push(
                    module_path,
                    line.symbol,
                    rel_address,
                    runtime_address,
                    line.instruction,
                );
            }
        }
    }

    tables.write("assembly_lines", lines.finish()?)?;

    let mut columns = Columns::default();
    columns.text(
        "module_path",
        metadata.iter().map(|(path, _)| path.clone()).collect(),
    );
    columns.i64(
        "load_bias",
        metadata.iter().map(|(_, bias)| *bias).collect(),
    );
    tables.write("assembly_module_metadata", columns.finish()?)?;

    tables.write_query(
        "assembly_address_stats",
        "SELECT module_path, func_name, address,
                CAST(SUM(samples) AS BIGINT) AS samples,
                CAST(SUM(cycles) AS BIGINT) AS cycles,
                CAST(SUM(instructions) AS BIGINT) AS instructions,
                CAST(SUM(branch_misses) AS BIGINT) AS branch_misses,
                CAST(SUM(branch_instructions) AS BIGINT) AS branch_instructions,
                CAST(SUM(llc_misses) AS BIGINT) AS llc_misses,
                CAST(SUM(llc_references) AS BIGINT) AS llc_references
         FROM assembly_samples
         GROUP BY module_path, func_name, address",
    )
}

fn write_assembly_samples(tables: &Tables) -> Result<()> {
    let available = tables.columns("pmu_counters");
    let metric = |column: &str| {
        if available.iter().any(|name| name == column) {
            format!("COALESCE(p.{}, 0)", quote_identifier(column))
        } else {
            "0".to_owned()
        }
    };
    tables.write_query(
        "assembly_samples",
        &format!(
            "SELECT
                m.module_path AS module_path,
                COALESCE(m.func_name, '[unknown]') AS func_name,
                p.ip AS address,
                COUNT(*) AS samples,
                CAST(SUM({}) AS BIGINT) AS cycles,
                CAST(SUM({}) AS BIGINT) AS instructions,
                CAST(SUM({}) AS BIGINT) AS branch_misses,
                CAST(SUM({}) AS BIGINT) AS branch_instructions,
                CAST(SUM({}) AS BIGINT) AS llc_misses,
                CAST(SUM({}) AS BIGINT) AS llc_references
             FROM pmu_counters p
             INNER JOIN proc_map m ON m.ip = p.ip
             WHERE m.module_path IS NOT NULL AND m.module_path <> ''
             GROUP BY m.module_path, COALESCE(m.func_name, '[unknown]'), p.ip",
            metric("pmu_cycles"),
            metric("pmu_instructions"),
            metric("pmu_branch_misses"),
            metric("pmu_branch_instructions"),
            metric("pmu_llc_misses"),
            metric("pmu_llc_references"),
        ),
    )
}

/// The disassembled instructions of every sampled module, keyed like the SQLite
/// table it replaces: one row per `(module_path, runtime_address)`.
#[derive(Default)]
struct AssemblyLines {
    seen: HashSet<(String, u64)>,
    module_path: Vec<String>,
    symbol: Vec<Option<String>>,
    rel_address: Vec<u64>,
    runtime_address: Vec<u64>,
    instruction: Vec<String>,
}

impl AssemblyLines {
    fn push(
        &mut self,
        module_path: &str,
        symbol: Option<String>,
        rel_address: u64,
        runtime_address: u64,
        instruction: String,
    ) {
        if !self.seen.insert((module_path.to_owned(), runtime_address)) {
            return;
        }
        self.module_path.push(module_path.to_owned());
        self.symbol.push(symbol);
        self.rel_address.push(rel_address);
        self.runtime_address.push(runtime_address);
        self.instruction.push(instruction);
    }

    fn finish(self) -> Result<store::arrow::record_batch::RecordBatch> {
        let rows = self.module_path.len();
        let mut columns = Columns::default();
        columns.text("module_path", self.module_path);
        columns.text_opt("symbol", self.symbol);
        columns.u64("rel_address", self.rel_address);
        columns.u64("runtime_address", self.runtime_address);
        columns.text("instruction", self.instruction);
        // Source annotations are intentionally omitted from the eager path;
        // loading full DWARF line tables dominates targeted disassembly.
        columns.text_opt("source_file", vec![None; rows]);
        columns.i64_opt("source_line", vec![None; rows]);
        columns.finish()
    }
}

#[derive(Clone)]
struct ObjectTextSymbol {
    start: u64,
    end: u64,
    raw_name: String,
    display_name: String,
}

fn sampled_disassembly_targets(
    module_path: &Path,
    load_bias: i64,
    runtime_addresses: &[u64],
) -> Result<(Vec<DisassembleTarget>, u64)> {
    let bytes = std::fs::read(module_path)?;
    let object = object::File::parse(bytes.as_slice())?;
    let mut symbols = object
        .symbols()
        .chain(object.dynamic_symbols())
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.address() != 0)
        .filter_map(|symbol| {
            let raw_name = symbol.name().ok()?.to_owned();
            Some((symbol.address(), symbol.size(), raw_name))
        })
        .collect::<Vec<_>>();
    symbols.sort_unstable_by_key(|symbol| symbol.0);
    symbols.dedup_by(|left, right| left.0 == right.0 && left.2 == right.2);
    let address_base = if load_bias > 0
        && symbols
            .first()
            .is_some_and(|symbol| symbol.0 >= load_bias as u64)
    {
        load_bias as u64
    } else {
        0
    };

    let mut text_symbols = Vec::with_capacity(symbols.len());
    for (index, (start, size, raw_name)) in symbols.iter().enumerate() {
        let next_start = symbols
            .iter()
            .skip(index + 1)
            .find_map(|candidate| (candidate.0 > *start).then_some(candidate.0))
            .unwrap_or(u64::MAX);
        let end = if *size > 0 {
            start.saturating_add(*size)
        } else {
            next_start
        };
        text_symbols.push(ObjectTextSymbol {
            start: start.saturating_sub(address_base),
            end: end.saturating_sub(address_base),
            raw_name: raw_name.clone(),
            display_name: addr2line::demangle_auto(Cow::Borrowed(raw_name), None).into_owned(),
        });
    }

    let mut selected = HashMap::<(u64, String), ObjectTextSymbol>::new();
    let mut fallback = Vec::<(u64, u64)>::new();
    for runtime_address in runtime_addresses.iter().copied() {
        let Some(relative) = remove_load_bias(runtime_address, load_bias) else {
            continue;
        };
        let insertion = text_symbols.partition_point(|symbol| symbol.start <= relative);
        let symbol = text_symbols[..insertion]
            .iter()
            .rev()
            .find(|symbol| relative < symbol.end);
        if let Some(symbol) = symbol {
            selected.insert((symbol.start, symbol.raw_name.clone()), symbol.clone());
        } else {
            fallback.push((relative.saturating_sub(256), relative.saturating_add(257)));
        }
    }

    let mut targets = selected
        .into_values()
        .map(|symbol| DisassembleTarget {
            raw_symbol: Some(symbol.raw_name),
            owner_symbol: symbol.display_name,
            start_address: symbol.start.saturating_add(address_base),
            end_address: symbol.end.saturating_add(address_base),
        })
        .collect::<Vec<_>>();
    fallback.sort_unstable();
    let mut merged = Vec::<(u64, u64)>::new();
    for (start, end) in fallback {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    targets.extend(merged.into_iter().map(|(start, end)| DisassembleTarget {
        raw_symbol: None,
        owner_symbol: format!("[sampled@0x{start:x}]"),
        start_address: start.saturating_add(address_base),
        end_address: end.saturating_add(address_base),
    }));
    targets.sort_unstable_by(|left, right| {
        left.raw_symbol
            .is_none()
            .cmp(&right.raw_symbol.is_none())
            .then_with(|| {
                left.start_address
                    .cmp(&right.start_address)
                    .then_with(|| left.owner_symbol.cmp(&right.owner_symbol))
            })
    });
    Ok((targets, address_base))
}

fn apply_load_bias(relative: u64, load_bias: i64) -> Option<u64> {
    if load_bias >= 0 {
        relative.checked_add(load_bias as u64)
    } else {
        relative.checked_sub(load_bias.unsigned_abs())
    }
}

fn remove_load_bias(runtime: u64, load_bias: i64) -> Option<u64> {
    if load_bias >= 0 {
        runtime.checked_sub(load_bias as u64)
    } else {
        runtime.checked_add(load_bias.unsigned_abs())
    }
}
