//! Inspect actual object symbols and inline frames using the shared name model.
use std::{collections::HashSet, path::PathBuf};

use object::{Object, ObjectSymbol, SymbolKind};
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
    for symbol in object
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.is_definition())
    {
        // Walk the fixture's real instruction ranges to include optimized inline frames.
        let raw = symbol.name()?;
        if !raw.contains("rust_names") && raw != "plain_c_name" {
            continue;
        }
        for address in symbol.address()..symbol.address() + symbol.size().max(1) {
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
