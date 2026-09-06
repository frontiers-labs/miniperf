//! C ABI for the DynamoRIO client. The client's C glue forwards
//! instrumentation events here; all accounting and artifact writing is shared
//! with the QEMU plugin.
//!
//! Mirrored by `utils/dr-roofline/roofline_core.h` — keep in sync.

use crate::artifacts::{self, CacheDescription, CounterSnapshot, ImageInfo};
use crate::cache::CacheModel;
use crate::cfg::{DynamicCfg, VectorClass};
use crate::classify::{self, BlockCost, FlowKind, RiscvClassification, RiscvKind, Target};
use crate::memory::MemoryAnalysis;
use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::Mutex;

pub const RC_TARGET_X86: u32 = 0;
pub const RC_TARGET_RISCV: u32 = 1;
pub const RC_TARGET_AARCH64: u32 = 2;

pub const RC_KIND_NONE: u32 = 0;
pub const RC_KIND_STATIC: u32 = 1;
pub const RC_KIND_RVV: u32 = 2;
pub const RC_KIND_UNCLASSIFIED: u32 = 3;

pub const RC_FLOW_NORMAL: u32 = 0;
pub const RC_FLOW_CALL: u32 = 1;
pub const RC_FLOW_RETURN: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RcCost {
    pub scalar_int: u64,
    pub scalar_float: u64,
    pub scalar_double: u64,
    pub vector_int: u64,
    pub vector_float: u64,
    pub vector_double: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RcClassification {
    pub kind: u32,
    pub cost: RcCost,
    pub rvv_is_float: u32,
    pub rvv_masked: u32,
    pub rvv_factor: u64,
    pub rvv_sew_scale: u64,
}

pub const RC_RECORD_BLOCK_EXEC: u32 = 0;
pub const RC_RECORD_MEM: u32 = 1;
pub const RC_RECORD_UNCLASSIFIED: u32 = 2;

/// One buffered instrumentation event. `desc` packs a registered handle in the
/// upper 30 bits and an `RC_RECORD_*` kind in the low 2 bits. `address` is only
/// meaningful for `RC_RECORD_MEM` records (other records leave it
/// uninitialized).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcRecord {
    pub desc: u32,
    pub _pad: u32,
    pub address: u64,
}

#[derive(Clone, Copy)]
struct RegisteredMem {
    block: u64,
    size: u64,
    store: bool,
}

#[derive(Clone)]
struct RegisteredBlock {
    cost: BlockCost,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
}

struct Inner {
    counters: CounterSnapshot,
    cfg: DynamicCfg,
    cache: CacheModel,
    memory: Option<MemoryAnalysis>,
    image: Option<ImageInfo>,
    output: PathBuf,
    blocks: Vec<RegisteredBlock>,
    mems: Vec<RegisteredMem>,
}

pub struct Session {
    inner: Mutex<Inner>,
}

fn target_from(raw: u32) -> Option<Target> {
    match raw {
        RC_TARGET_X86 => Some(Target::X86),
        RC_TARGET_RISCV => Some(Target::Riscv),
        RC_TARGET_AARCH64 => Some(Target::Aarch64),
        _ => None,
    }
}

/// # Safety
/// `output` must be a valid NUL-terminated path string.
#[no_mangle]
pub unsafe extern "C" fn rc_session_new(
    output: *const c_char,
    cache_line: u64,
    llc_size: u64,
    llc_associativity: u64,
    memory_profile: u32,
) -> *mut Session {
    if output.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(associativity) = usize::try_from(llc_associativity) else {
        return std::ptr::null_mut();
    };
    let Some(cache) = CacheModel::new(cache_line, llc_size, associativity) else {
        return std::ptr::null_mut();
    };
    let path = PathBuf::from(
        unsafe { CStr::from_ptr(output) }
            .to_string_lossy()
            .into_owned(),
    );
    let line_size = cache.line_size();
    let session = Session {
        inner: Mutex::new(Inner {
            counters: CounterSnapshot::default(),
            cfg: DynamicCfg::new(),
            cache,
            memory: (memory_profile != 0).then(|| MemoryAnalysis::new(line_size)),
            image: None,
            output: path,
            blocks: Vec::new(),
            mems: Vec::new(),
        }),
    };
    Box::into_raw(Box::new(session))
}

/// # Safety
/// `session` must be a live pointer from `rc_session_new`.
#[no_mangle]
pub unsafe extern "C" fn rc_session_set_image(
    session: *mut Session,
    start: u64,
    end: u64,
    entry: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    session.inner.lock().unwrap().image = Some(ImageInfo { start, end, entry });
}

/// Classifies one instruction from its disassembly text.
///
/// # Safety
/// `disassembly` must be a valid NUL-terminated string and `out` a valid
/// pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_classify(
    target: u32,
    disassembly: *const c_char,
    out: *mut RcClassification,
) {
    let Some(out) = (unsafe { out.as_mut() }) else {
        return;
    };
    *out = RcClassification::default();
    if disassembly.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(disassembly) }.to_string_lossy();
    match target_from(target) {
        Some(Target::Riscv) => match classify::classify_riscv(&text) {
            RiscvClassification::Counted(cost) => match cost.kind {
                RiscvKind::VectorInteger | RiscvKind::VectorFloat => {
                    out.kind = RC_KIND_RVV;
                    out.rvv_is_float = u32::from(cost.kind == RiscvKind::VectorFloat);
                    out.rvv_masked = u32::from(classify::is_masked(&text));
                    out.rvv_factor = cost.factor;
                    out.rvv_sew_scale = cost.sew_scale;
                }
                RiscvKind::ScalarInteger => {
                    out.kind = RC_KIND_STATIC;
                    out.cost.scalar_int = cost.factor;
                }
                RiscvKind::ScalarFloat => {
                    out.kind = RC_KIND_STATIC;
                    out.cost.scalar_float = cost.factor;
                }
                RiscvKind::ScalarDouble => {
                    out.kind = RC_KIND_STATIC;
                    out.cost.scalar_double = cost.factor;
                }
            },
            RiscvClassification::NonCompute => out.kind = RC_KIND_NONE,
            RiscvClassification::Unclassified => out.kind = RC_KIND_UNCLASSIFIED,
        },
        Some(Target::X86) => {
            let cost = classify::classify_x86(&classify::mnemonic(&text), &text);
            fill_static(out, &cost);
        }
        Some(Target::Aarch64) => {
            let cost = classify::classify_aarch64(&classify::mnemonic(&text), &text);
            fill_static(out, &cost);
        }
        None => {}
    }
}

fn fill_static(out: &mut RcClassification, cost: &BlockCost) {
    out.kind = RC_KIND_STATIC;
    out.cost = RcCost {
        scalar_int: cost.scalar_int,
        scalar_float: cost.scalar_float,
        scalar_double: cost.scalar_double,
        vector_int: cost.vector_int,
        vector_float: cost.vector_float,
        vector_double: cost.vector_double,
    };
}

/// # Safety
/// `disassembly` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn rc_flow_kind(target: u32, disassembly: *const c_char) -> u32 {
    if disassembly.is_null() {
        return RC_FLOW_NORMAL;
    }
    let text = unsafe { CStr::from_ptr(disassembly) }.to_string_lossy();
    match classify::classify_flow(target_from(target), &text) {
        FlowKind::Normal => RC_FLOW_NORMAL,
        FlowKind::Call => RC_FLOW_CALL,
        FlowKind::Return => RC_FLOW_RETURN,
    }
}

/// Records one execution of a block.
///
/// # Safety
/// `session` and `cost` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn rc_block_exec(
    session: *mut Session,
    thread: u32,
    vaddr: u64,
    end_vaddr: u64,
    flow: u32,
    cost: *const RcCost,
    instructions: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let Some(cost) = (unsafe { cost.as_ref() }) else {
        return;
    };
    let block = BlockCost {
        vaddr,
        end_vaddr,
        flow: match flow {
            RC_FLOW_CALL => FlowKind::Call,
            RC_FLOW_RETURN => FlowKind::Return,
            _ => FlowKind::Normal,
        },
        scalar_int: cost.scalar_int,
        scalar_float: cost.scalar_float,
        scalar_double: cost.scalar_double,
        vector_int: cost.vector_int,
        vector_float: cost.vector_float,
        vector_double: cost.vector_double,
        instructions,
    };
    let mut inner = session.inner.lock().unwrap();
    add_exec_counters(&mut inner.counters, &block, 1);
    inner.cfg.record_block(thread as usize, &block);
}

fn add_exec_counters(counters: &mut CounterSnapshot, cost: &BlockCost, executions: u64) {
    counters.instructions = counters
        .instructions
        .saturating_add(cost.instructions.saturating_mul(executions));
    counters.scalar_int_ops = counters
        .scalar_int_ops
        .saturating_add(cost.scalar_int.saturating_mul(executions));
    counters.scalar_float_ops = counters
        .scalar_float_ops
        .saturating_add(cost.scalar_float.saturating_mul(executions));
    counters.scalar_double_ops = counters
        .scalar_double_ops
        .saturating_add(cost.scalar_double.saturating_mul(executions));
    counters.vector_int_ops = counters
        .vector_int_ops
        .saturating_add(cost.vector_int.saturating_mul(executions));
    counters.vector_float_ops = counters
        .vector_float_ops
        .saturating_add(cost.vector_float.saturating_mul(executions));
    counters.vector_double_ops = counters
        .vector_double_ops
        .saturating_add(cost.vector_double.saturating_mul(executions));
}

/// Registers a translated block for use in `RC_RECORD_BLOCK_EXEC` /
/// `RC_RECORD_UNCLASSIFIED` records and returns its handle.
///
/// # Safety
/// `session` and `cost` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn rc_register_block(
    session: *mut Session,
    vaddr: u64,
    end_vaddr: u64,
    flow: u32,
    cost: *const RcCost,
    instructions: u64,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
) -> u32 {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return u32::MAX;
    };
    let Some(cost) = (unsafe { cost.as_ref() }) else {
        return u32::MAX;
    };
    let block = BlockCost {
        vaddr,
        end_vaddr,
        flow: match flow {
            RC_FLOW_CALL => FlowKind::Call,
            RC_FLOW_RETURN => FlowKind::Return,
            _ => FlowKind::Normal,
        },
        scalar_int: cost.scalar_int,
        scalar_float: cost.scalar_float,
        scalar_double: cost.scalar_double,
        vector_int: cost.vector_int,
        vector_float: cost.vector_float,
        vector_double: cost.vector_double,
        instructions,
    };
    let mut inner = session.inner.lock().unwrap();
    if inner.blocks.len() >= (u32::MAX >> 2) as usize {
        return u32::MAX;
    }
    inner.blocks.push(RegisteredBlock {
        cost: block,
        arch_bytes_load,
        arch_bytes_store,
    });
    (inner.blocks.len() - 1) as u32
}

/// Registers a static memory operand (issuing block, access size, direction)
/// for use in `RC_RECORD_MEM` records and returns its handle.
///
/// # Safety
/// `session` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_register_mem(
    session: *mut Session,
    block: u64,
    size: u64,
    is_store: u32,
) -> u32 {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return u32::MAX;
    };
    let mut inner = session.inner.lock().unwrap();
    if inner.mems.len() >= (u32::MAX >> 2) as usize {
        return u32::MAX;
    }
    inner.mems.push(RegisteredMem {
        block,
        size,
        store: is_store != 0,
    });
    (inner.mems.len() - 1) as u32
}

/// Applies an aggregate execution count and static successors for a block.
///
/// # Safety
/// `session` must be live and `handle` must come from `rc_register_block`.
#[no_mangle]
pub unsafe extern "C" fn rc_counted_block(
    session: *mut Session,
    handle: u32,
    executions: u64,
    successor_a: u64,
    successor_b: u64,
    successor_count: u32,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    if executions == 0 {
        return;
    }
    let mut inner = session.inner.lock().unwrap();
    let Some(block) = inner.blocks.get(handle as usize).cloned() else {
        return;
    };
    add_exec_counters(&mut inner.counters, &block.cost, executions);
    let successors = [successor_a, successor_b];
    inner.cfg.record_counted_block(
        &block.cost,
        executions,
        &successors[..(successor_count as usize).min(successors.len())],
    );
    add_static_memory(&mut inner, &block, executions);
}

/// Memory attribution for one run of consecutive accesses issued by the same
/// block, folded into a single CFG update.
#[derive(Default)]
struct PendingMemory {
    block: u64,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
    dram_bytes_load: u64,
    dram_bytes_store: u64,
}

impl PendingMemory {
    fn flush(&mut self, cfg: &mut DynamicCfg) {
        if self.arch_bytes_load == 0
            && self.arch_bytes_store == 0
            && self.dram_bytes_load == 0
            && self.dram_bytes_store == 0
        {
            return;
        }
        cfg.attribute_memory(
            self.block,
            self.arch_bytes_load,
            self.arch_bytes_store,
            self.dram_bytes_load,
            self.dram_bytes_store,
        );
        self.arch_bytes_load = 0;
        self.arch_bytes_store = 0;
        self.dram_bytes_load = 0;
        self.dram_bytes_store = 0;
    }
}

fn flush_repeats(inner: &mut Inner, thread: usize, run_handle: u32, run_count: &mut u64) {
    if *run_count == 0 {
        return;
    }
    if let Some(block) = inner.blocks.get(run_handle as usize) {
        let block = block.clone();
        add_exec_counters(&mut inner.counters, &block.cost, *run_count);
        inner.cfg.record_repeats(thread, &block.cost, *run_count);
        add_static_memory(inner, &block, *run_count);
    }
    *run_count = 0;
}

fn add_static_memory(inner: &mut Inner, block: &RegisteredBlock, executions: u64) {
    let bytes_load = block.arch_bytes_load.saturating_mul(executions);
    let bytes_store = block.arch_bytes_store.saturating_mul(executions);
    inner.counters.bytes_load = inner.counters.bytes_load.saturating_add(bytes_load);
    inner.counters.bytes_store = inner.counters.bytes_store.saturating_add(bytes_store);
    if bytes_load != 0 || bytes_store != 0 {
        inner
            .cfg
            .attribute_memory(block.cost.vaddr, bytes_load, bytes_store, 0, 0);
    }
}

/// Processes a batch of buffered instrumentation records for one thread.
/// Records must be in program order for that thread; handles referenced by
/// `desc` must come from `rc_register_block` / `rc_register_mem` on the same
/// session (out-of-range handles are ignored).
///
/// # Safety
/// `session` must be a valid pointer and `records` must point to `count`
/// readable records when non-null.
#[no_mangle]
pub unsafe extern "C" fn rc_process_batch(
    session: *mut Session,
    thread: u32,
    records: *const RcRecord,
    count: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    if records.is_null() || count == 0 {
        return;
    }
    let records = unsafe { std::slice::from_raw_parts(records, count as usize) };
    let thread = thread as usize;
    let mut guard = session.inner.lock().unwrap();
    let inner = &mut *guard;
    // Back-to-back executions of the same fall-through block (a self loop, the
    // hot pattern in tight loops) are run-length aggregated: the run is applied
    // as one edge/block update when it ends. Interleaved mem/unclassified
    // records do not break a run because they do not affect edge bookkeeping.
    let mut run_handle = u32::MAX;
    let mut run_count = 0u64;
    let mut pending = PendingMemory::default();
    for record in records {
        let kind = record.desc & 3;
        let handle = (record.desc >> 2) as usize;
        match kind {
            RC_RECORD_MEM => {
                let Some(mem) = inner.mems.get(handle) else {
                    continue;
                };
                let mem = *mem;
                if let Some(memory) = inner.memory.as_mut() {
                    memory.access(thread, record.address, mem.size, mem.store);
                }
                if mem.store {
                    inner.counters.bytes_store =
                        inner.counters.bytes_store.saturating_add(mem.size);
                } else {
                    inner.counters.bytes_load = inner.counters.bytes_load.saturating_add(mem.size);
                }
                let traffic = inner.cache.access(record.address, mem.size, mem.store);
                if traffic.bytes_load != 0 || traffic.bytes_store != 0 {
                    inner.counters.dram_bytes_load = inner
                        .counters
                        .dram_bytes_load
                        .saturating_add(traffic.bytes_load);
                    inner.counters.dram_bytes_store = inner
                        .counters
                        .dram_bytes_store
                        .saturating_add(traffic.bytes_store);
                }
                // Accesses issued by one block arrive consecutively, so the
                // per-block CFG update is coalesced across the run instead of
                // doing a BTreeMap lookup per access.
                if pending.block != mem.block {
                    pending.flush(&mut inner.cfg);
                    pending.block = mem.block;
                }
                if mem.store {
                    pending.arch_bytes_store = pending.arch_bytes_store.saturating_add(mem.size);
                } else {
                    pending.arch_bytes_load = pending.arch_bytes_load.saturating_add(mem.size);
                }
                pending.dram_bytes_load =
                    pending.dram_bytes_load.saturating_add(traffic.bytes_load);
                pending.dram_bytes_store =
                    pending.dram_bytes_store.saturating_add(traffic.bytes_store);
            }
            RC_RECORD_BLOCK_EXEC => {
                if handle as u32 == run_handle {
                    run_count += 1;
                    continue;
                }
                flush_repeats(inner, thread, run_handle, &mut run_count);
                let Some(block) = inner.blocks.get(handle) else {
                    run_handle = u32::MAX;
                    continue;
                };
                let block = block.clone();
                add_exec_counters(&mut inner.counters, &block.cost, 1);
                inner.cfg.record_block(thread, &block.cost);
                add_static_memory(inner, &block, 1);
                run_handle = if block.cost.flow == FlowKind::Normal {
                    handle as u32
                } else {
                    u32::MAX
                };
            }
            RC_RECORD_UNCLASSIFIED => {
                let Some(vaddr) = inner.blocks.get(handle).map(|block| block.cost.vaddr) else {
                    continue;
                };
                inner.counters.unclassified_instructions =
                    inner.counters.unclassified_instructions.saturating_add(1);
                inner.cfg.attribute_unclassified(vaddr);
            }
            _ => {}
        }
    }
    flush_repeats(inner, thread, run_handle, &mut run_count);
    pending.flush(&mut inner.cfg);
}

/// Records one memory access issued by `block`.
///
/// # Safety
/// `session` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_mem_access(
    session: *mut Session,
    thread: u32,
    block: u64,
    address: u64,
    size: u64,
    is_store: u32,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let store = is_store != 0;
    let mut inner = session.inner.lock().unwrap();
    if let Some(memory) = inner.memory.as_mut() {
        memory.access(thread as usize, address, size, store);
    }
    if store {
        inner.counters.bytes_store = inner.counters.bytes_store.saturating_add(size);
    } else {
        inner.counters.bytes_load = inner.counters.bytes_load.saturating_add(size);
    }
    let traffic = inner.cache.access(address, size, store);
    inner.counters.dram_bytes_load = inner
        .counters
        .dram_bytes_load
        .saturating_add(traffic.bytes_load);
    inner.counters.dram_bytes_store = inner
        .counters
        .dram_bytes_store
        .saturating_add(traffic.bytes_store);
    let (arch_load, arch_store) = if store { (0, size) } else { (size, 0) };
    inner.cfg.attribute_memory(
        block,
        arch_load,
        arch_store,
        traffic.bytes_load,
        traffic.bytes_store,
    );
}

/// Records dynamically-counted RVV operations for `block`.
///
/// # Safety
/// `session` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_rvv_exec(
    session: *mut Session,
    block: u64,
    is_float: u32,
    sew_bits: u64,
    operations: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let mut inner = session.inner.lock().unwrap();
    let class = if is_float == 0 {
        inner.counters.vector_int_ops = inner.counters.vector_int_ops.saturating_add(operations);
        VectorClass::Integer
    } else if sew_bits == 64 {
        inner.counters.vector_double_ops =
            inner.counters.vector_double_ops.saturating_add(operations);
        VectorClass::Double
    } else {
        inner.counters.vector_float_ops =
            inner.counters.vector_float_ops.saturating_add(operations);
        VectorClass::Float
    };
    inner.cfg.attribute_vector(block, class, operations);
}

/// # Safety
/// `session` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_unclassified(session: *mut Session, block: u64) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let mut inner = session.inner.lock().unwrap();
    inner.counters.unclassified_instructions =
        inner.counters.unclassified_instructions.saturating_add(1);
    inner.cfg.attribute_unclassified(block);
}

/// # Safety
/// `session` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn rc_rvv_state_error(session: *mut Session) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let mut inner = session.inner.lock().unwrap();
    inner.counters.rvv_state_errors = inner.counters.rvv_state_errors.saturating_add(1);
}

/// Returns the number of guest instructions recorded so far.
///
/// # Safety
/// `session` must be a live pointer from `rc_session_new`.
#[no_mangle]
pub unsafe extern "C" fn rc_instruction_count(session: *mut Session) -> u64 {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return 0;
    };
    session.inner.lock().unwrap().counters.instructions
}

/// Counts active elements in [vstart, vl) under an optional v0 mask.
/// Returns -1 when the mask is too short for vl.
///
/// # Safety
/// `mask` must point to `mask_len` readable bytes when non-null.
#[no_mangle]
pub unsafe extern "C" fn rc_active_elements(
    vstart: u64,
    vl: u64,
    mask: *const u8,
    mask_len: u64,
) -> i64 {
    let mask = if mask.is_null() {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(mask, mask_len as usize) })
    };
    match classify::active_elements(vstart, vl, mask) {
        Some(elements) => elements as i64,
        None => -1,
    }
}

/// Decodes SEW bits from vtype. Returns -1 when vtype is invalid (vill).
#[no_mangle]
pub extern "C" fn rc_rvv_sew(vtype: u64, xlen: u32) -> i64 {
    match classify::rvv_sew(vtype, xlen) {
        Some(sew) => sew as i64,
        None => -1,
    }
}

/// Writes the three artifacts and frees the session. `session` is invalid
/// afterwards.
///
/// # Safety
/// `session` must be a live pointer from `rc_session_new`, not used again.
#[no_mangle]
pub unsafe extern "C" fn rc_finalize(session: *mut Session) -> i32 {
    if session.is_null() {
        return -1;
    }
    let session = unsafe { Box::from_raw(session) };
    let mut inner = session.inner.lock().unwrap();
    let flush = inner.cache.flush();
    inner.counters.dram_bytes_load = inner
        .counters
        .dram_bytes_load
        .saturating_add(flush.bytes_load);
    inner.counters.dram_bytes_store = inner
        .counters
        .dram_bytes_store
        .saturating_add(flush.bytes_store);

    let output = inner.output.clone();
    artifacts::write_counts(&output, &inner.counters);
    artifacts::write_cfg(
        &output.with_extension("cfg"),
        Some(CacheDescription {
            line_size: inner.cache.line_size(),
            capacity: inner.cache.capacity(),
            associativity: inner.cache.associativity(),
        }),
        inner.image,
        &inner.cfg,
    );
    if let Some(memory) = inner.memory.as_ref() {
        artifacts::write_memory(&output.with_extension("memory.json"), &memory.artifact());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_session(dir: &std::path::Path, name: &str) -> *mut Session {
        let path = std::ffi::CString::new(dir.join(name).to_str().unwrap().to_owned() + ".counts")
            .unwrap();
        let session = unsafe { rc_session_new(path.as_ptr(), 64, 4096, 2, 1) };
        assert!(!session.is_null());
        session
    }

    /// The batched path (registration + rc_process_batch, including self-loop
    /// run aggregation and the zero-traffic fast path) must produce artifacts
    /// byte-identical to the per-event path.
    #[test]
    fn batch_processing_matches_per_event_processing() {
        let dir = std::env::temp_dir().join(format!("rc-batch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cost_a = RcCost {
            scalar_int: 2,
            scalar_double: 1,
            ..Default::default()
        };
        let cost_b = RcCost {
            scalar_float: 3,
            ..Default::default()
        };
        // Block A self-loops with two memory accesses per iteration, then
        // calls block B, which is also the unclassified one.
        let accesses = [(0x1000_u64, 8_u64, 0_u32), (0x2000, 8, 1)];

        let reference = new_session(&dir, "reference");
        let batched = new_session(&dir, "batched");

        let block_a =
            unsafe { rc_register_block(batched, 0x400, 0x420, RC_FLOW_NORMAL, &cost_a, 5, 0, 0) };
        let block_b =
            unsafe { rc_register_block(batched, 0x500, 0x510, RC_FLOW_CALL, &cost_b, 3, 0, 0) };
        let mem_handles: Vec<u32> = accesses
            .iter()
            .map(|&(_, size, store)| unsafe { rc_register_mem(batched, 0x400, size, store) })
            .collect();

        let mut records = Vec::new();
        for iteration in 0..100_u64 {
            unsafe {
                rc_block_exec(reference, 0, 0x400, 0x420, RC_FLOW_NORMAL, &cost_a, 5);
            }
            records.push(RcRecord {
                desc: block_a << 2 | RC_RECORD_BLOCK_EXEC,
                _pad: 0,
                address: 0,
            });
            for (index, &(base, size, store)) in accesses.iter().enumerate() {
                let address = base + (iteration % 16) * 64;
                unsafe {
                    rc_mem_access(reference, 0, 0x400, address, size, store);
                }
                records.push(RcRecord {
                    desc: mem_handles[index] << 2 | RC_RECORD_MEM,
                    _pad: 0,
                    address,
                });
            }
        }
        unsafe {
            rc_block_exec(reference, 0, 0x500, 0x510, RC_FLOW_CALL, &cost_b, 3);
            rc_unclassified(reference, 0x500);
        }
        records.push(RcRecord {
            desc: block_b << 2 | RC_RECORD_BLOCK_EXEC,
            _pad: 0,
            address: 0,
        });
        records.push(RcRecord {
            desc: block_b << 2 | RC_RECORD_UNCLASSIFIED,
            _pad: 0,
            address: 0,
        });

        // Split the batch to also exercise run state across batch boundaries.
        let (first, second) = records.split_at(records.len() / 2);
        unsafe {
            rc_process_batch(batched, 0, first.as_ptr(), first.len() as u64);
            rc_process_batch(batched, 0, second.as_ptr(), second.len() as u64);
            assert_eq!(
                rc_instruction_count(reference),
                rc_instruction_count(batched)
            );
            assert_eq!(rc_finalize(reference), 0);
            assert_eq!(rc_finalize(batched), 0);
        }

        for extension in ["counts", "cfg", "memory.json"] {
            let reference_bytes =
                std::fs::read(dir.join(format!("reference.{extension}"))).unwrap();
            let batched_bytes = std::fs::read(dir.join(format!("batched.{extension}"))).unwrap();
            assert_eq!(
                reference_bytes, batched_bytes,
                "artifact mismatch: {extension}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
