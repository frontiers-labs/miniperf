use std::collections::HashMap;

use anyhow::Result;
use libprof::Counter;
use mperf_data::{EventType, ProcMapEntry};
use store::duckdb::Connection;
use symbolize::{ProcessMap, Resolver};

pub fn counter_to_event_ty(counter: &Counter) -> EventType {
    match counter {
        Counter::Cycles => EventType::PmuCycles,
        Counter::Instructions => EventType::PmuInstructions,
        Counter::LLCReferences => EventType::PmuLlcReferences,
        Counter::LLCMisses => EventType::PmuLlcMisses,
        Counter::BranchInstructions => EventType::PmuBranchInstructions,
        Counter::BranchMisses => EventType::PmuBranchMisses,
        Counter::StalledCyclesFrontend => EventType::PmuStalledCyclesFrontend,
        Counter::StalledCyclesBackend => EventType::PmuStalledCyclesBackend,
        Counter::CpuClock => EventType::OsCpuClock,
        Counter::PageFaults => EventType::OsPageFaults,
        Counter::CpuMigrations => EventType::OsCpuMigrations,
        Counter::ContextSwitches => EventType::OsContextSwitches,
        Counter::Custom(_) => EventType::PmuCustom,
        Counter::Internal {
            name: _,
            desc: _,
            code: _,
        } => EventType::PmuCustom,
    }
}

/// The event type whose recorded name is `name`, or `None` for a custom counter.
pub fn event_type_from_name(name: &str) -> Option<EventType> {
    const TYPES: [EventType; 26] = [
        EventType::PmuCycles,
        EventType::PmuInstructions,
        EventType::PmuLlcReferences,
        EventType::PmuLlcMisses,
        EventType::PmuBranchInstructions,
        EventType::PmuBranchMisses,
        EventType::PmuStalledCyclesFrontend,
        EventType::PmuStalledCyclesBackend,
        EventType::PmuCustom,
        EventType::OsCpuClock,
        EventType::OsCpuMigrations,
        EventType::OsPageFaults,
        EventType::OsContextSwitches,
        EventType::OsTotalTime,
        EventType::OsUserTime,
        EventType::OsSystemTime,
        EventType::RooflineBytesLoad,
        EventType::RooflineBytesStore,
        EventType::RooflineScalarIntOps,
        EventType::RooflineScalarFloatOps,
        EventType::RooflineScalarDoubleOps,
        EventType::RooflineVectorIntOps,
        EventType::RooflineVectorFloatOps,
        EventType::RooflineVectorDoubleOps,
        EventType::RooflineLoopStart,
        EventType::RooflineLoopEnd,
    ];
    TYPES
        .into_iter()
        .find(|ty| ty.to_string().eq_ignore_ascii_case(name))
}

/// Executable mappings recorded in the session's `modules` table.
pub fn load_modules(connection: &Connection) -> Result<Vec<ProcMapEntry>> {
    let mut statement =
        connection.prepare("SELECT pid, path, address, size, \"offset\" FROM modules")?;
    let modules = statement
        .query_map([], |row| {
            Ok(ProcMapEntry {
                pid: row.get::<_, u32>(0)?,
                filename: row.get::<_, String>(1)?,
                address: row.get::<_, u64>(2)? as usize,
                size: row.get::<_, u64>(3)? as usize,
                offset: row.get::<_, u64>(4)? as usize,
            })
        })?
        .collect::<store::duckdb::Result<Vec<_>>>()?;
    Ok(modules)
}

/// The interned string dictionary of a session.
pub fn load_strings(connection: &Connection) -> Result<HashMap<u64, String>> {
    let mut statement = connection.prepare("SELECT id, string FROM strings")?;
    let strings = statement
        .query_map([], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<store::duckdb::Result<HashMap<_, _>>>()?;
    Ok(strings)
}

pub fn resolve_proc_maps(proc_maps: &[ProcMapEntry]) -> Resolver {
    Resolver::new(proc_maps.iter().map(|map| ProcessMap {
        pid: map.pid,
        path: map.filename.clone().into(),
        start: map.address as u64,
        end: map.address.saturating_add(map.size) as u64,
        offset: map.offset as u64,
    }))
}

/// The module maps libprof's unwinder needs, from a recording's `modules`
/// table.
pub fn proc_addrs(modules: &[mperf_data::ProcMapEntry]) -> Vec<libprof::ProcAddr> {
    modules
        .iter()
        .map(|module| libprof::ProcAddr {
            pid: module.pid,
            addr: module.address as u64,
            len: module.size as u64,
            pgoff: module.offset as u64,
            filename: module.filename.clone(),
        })
        .collect()
}
