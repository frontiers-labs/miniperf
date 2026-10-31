//! Inspect actual object symbols and inline frames using the shared name model.
use std::{collections::HashSet, path::PathBuf};

use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use symbolize::{FunctionName, ProcessMap, Resolver};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(std::env::args_os().nth(1).ok_or("expected object path")?);
    let bytes = std::fs::read(&path)?;
    let object = object::File::parse(bytes.as_slice())?;
    let resolver = Resolver::new([ProcessMap {
        pid: 1,
        path,
        start: 0,
        end: u64::MAX,
        offset: 0,
    }]);
    let mut seen = HashSet::new();
    let mut symbols = object
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.is_definition())
        .collect::<Vec<_>>();
    symbols.sort_unstable_by_key(|symbol| symbol.address());
    for symbol in &symbols {
        // Walk the fixture's real instruction ranges to include optimized inline frames.
        let raw = symbol.name()?;
        if !raw.contains("rust_names") && !matches!(raw, "plain_c_name" | "_plain_c_name") {
            continue;
        }
        let start = symbol.address();
        let end = if symbol.size() > 0 {
            start.saturating_add(symbol.size())
        } else {
            // Mach-O has no symbol sizes; inline bodies can start after the entry instruction.
            let section_end = symbol
                .section_index()
                .and_then(|index| object.section_by_index(index).ok())
                .map(|section| section.address().saturating_add(section.size()))
                .unwrap_or(start);
            let next = symbols.partition_point(|candidate| candidate.address() <= start);
            symbols
                .get(next)
                .map_or(section_end, |next| next.address().min(section_end))
        };
        for address in start..end {
            for (depth, frame) in resolver.resolve(1, address).into_iter().enumerate() {
                let name = FunctionName::new(&frame.function);
                let row = format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    frame.linkage_name.as_deref().unwrap_or(""),
                    name.full(),
                    name.friendly(),
                    depth,
                    frame.file.as_deref().unwrap_or(""),
                    frame.line.unwrap_or(0)
                );
                if seen.insert(row.clone()) {
                    println!("{row}");
                }
            }
        }
    }
    Ok(())
}
