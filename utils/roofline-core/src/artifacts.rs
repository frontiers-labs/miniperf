//! Writers for the three roofline artifacts. The file formats are the
//! contract between the instrumentation backends (QEMU plugin, DynamoRIO
//! client) and `mperf` postprocessing — keep them stable.

use crate::cfg::DynamicCfg;
use crate::memory::MemoryArtifact;
use std::{fs::File, io::Write, path::Path};

#[derive(Clone, Copy, Debug, Default)]
pub struct CounterSnapshot {
    pub scalar_int_ops: u64,
    pub scalar_float_ops: u64,
    pub scalar_double_ops: u64,
    pub vector_int_ops: u64,
    pub vector_float_ops: u64,
    pub vector_double_ops: u64,
    pub bytes_load: u64,
    pub bytes_store: u64,
    pub dram_bytes_load: u64,
    pub dram_bytes_store: u64,
    pub rvv_state_errors: u64,
    pub unclassified_instructions: u64,
    pub instructions: u64,
    pub child_process_seen: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CacheDescription {
    pub line_size: u64,
    pub capacity: u64,
    pub associativity: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ImageInfo {
    pub start: u64,
    pub end: u64,
    pub entry: u64,
}

pub fn write_counts(path: &Path, counters: &CounterSnapshot) {
    let Ok(mut file) = File::create(path) else {
        return;
    };
    let values = [
        ("scalar_int_ops", counters.scalar_int_ops),
        ("scalar_float_ops", counters.scalar_float_ops),
        ("scalar_double_ops", counters.scalar_double_ops),
        ("vector_int_ops", counters.vector_int_ops),
        ("vector_float_ops", counters.vector_float_ops),
        ("vector_double_ops", counters.vector_double_ops),
        ("bytes_load", counters.bytes_load),
        ("bytes_store", counters.bytes_store),
        ("dram_bytes_load", counters.dram_bytes_load),
        ("dram_bytes_store", counters.dram_bytes_store),
        ("rvv_state_errors", counters.rvv_state_errors),
        (
            "unclassified_instructions",
            counters.unclassified_instructions,
        ),
        ("instructions", counters.instructions),
    ];
    for (name, value) in values {
        let _ = writeln!(file, "{name}={value}");
    }
    let _ = writeln!(
        file,
        "child_process_seen={}",
        u64::from(counters.child_process_seen)
    );
}

pub fn write_cfg(
    path: &Path,
    cache: Option<CacheDescription>,
    image: Option<ImageInfo>,
    cfg: &DynamicCfg,
) {
    let Ok(mut file) = File::create(path) else {
        return;
    };
    let _ = writeln!(file, "miniperf-qemu-cfg=5");
    if let Some(cache) = cache {
        let _ = writeln!(
            file,
            "cache {} {} {} write-back-write-allocate",
            cache.line_size, cache.capacity, cache.associativity
        );
    }
    if let Some(image) = image {
        let _ = writeln!(
            file,
            "image {:#x} {:#x} {:#x}",
            image.start, image.end, image.entry
        );
    }
    for entry in &cfg.entries {
        let _ = writeln!(file, "entry {entry:#x}");
    }
    for (&(from, to), &executions) in &cfg.edges {
        let _ = writeln!(file, "edge {from:#x} {to:#x} {executions}");
    }
    for (&address, counts) in &cfg.blocks {
        let _ = writeln!(
            file,
            "block {address:#x} {:#x} {} {} {} {} {} {} {} {} {} {} {} {}",
            counts.end_vaddr,
            counts.executions,
            counts.scalar_int,
            counts.scalar_float,
            counts.scalar_double,
            counts.vector_int,
            counts.vector_float,
            counts.vector_double,
            counts.bytes_load,
            counts.bytes_store,
            counts.arch_bytes_load,
            counts.arch_bytes_store,
            counts.unclassified,
        );
        for &(thread, executions) in &counts.threads {
            let _ = writeln!(file, "tblock {address:#x} {thread} {executions}");
        }
    }
}

pub fn write_memory(path: &Path, artifact: &MemoryArtifact) {
    if let Ok(file) = File::create(path) {
        let _ = serde_json::to_writer(file, artifact);
    }
}
