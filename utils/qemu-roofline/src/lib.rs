//! QEMU TCG plugin for roofline/memory accounting. All analysis and artifact
//! writing lives in `miniperf-roofline-core`; this crate is the QEMU plugin
//! API adapter.

use roofline_core::{
    active_elements, artifacts, classify_flow, classify_riscv, classify_x86, is_masked, mnemonic,
    rvv_sew, BlockCost, CacheDescription, CacheModel, CounterSnapshot, DynamicCfg, ImageInfo,
    MemoryAnalysis, RiscvClassification, RiscvKind, RvvKind, Target, VectorClass,
};
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr},
    path::PathBuf,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

type PluginId = u64;
type MemInfo = u32;

#[repr(C)]
struct TranslationBlock {
    _private: [u8; 0],
}

#[repr(C)]
struct Instruction {
    _private: [u8; 0],
}

#[repr(C)]
pub struct QemuInfo {
    target_name: *const c_char,
    version: QemuApiVersion,
    system_emulation: bool,
}

#[repr(C)]
struct QemuApiVersion {
    min: c_int,
    current: c_int,
}

#[repr(C)]
struct QemuRegister {
    _private: [u8; 0],
}

#[repr(C)]
struct GArray {
    data: *mut c_char,
    len: c_uint,
}

#[repr(C)]
struct GByteArray {
    data: *mut u8,
    len: c_uint,
}

#[repr(C)]
struct RegisterDescriptor {
    handle: *mut QemuRegister,
    name: *const c_char,
    feature: *const c_char,
    is_readonly: bool,
}

type TranslationCallback = extern "C" fn(PluginId, *mut TranslationBlock);
type ExecutionCallback = extern "C" fn(c_uint, *mut c_void);
type MemoryCallback = extern "C" fn(c_uint, MemInfo, u64, *mut c_void);
type ExitCallback = extern "C" fn(PluginId, *mut c_void);
type VcpuInitCallback = extern "C" fn(PluginId, c_uint);

const CALLBACK_NO_REGS: c_int = 0;
const CALLBACK_READ_REGS: c_int = 1;
const MEMORY_READ_WRITE: c_int = 3;
const REQUIRED_QEMU_PLUGIN_API: c_int = 6;

unsafe extern "C" {
    fn qemu_plugin_register_vcpu_tb_trans_cb(id: PluginId, callback: TranslationCallback);
    fn qemu_plugin_tb_n_insns(tb: *const TranslationBlock) -> usize;
    fn qemu_plugin_tb_vaddr(tb: *const TranslationBlock) -> u64;
    fn qemu_plugin_tb_get_insn(tb: *const TranslationBlock, index: usize) -> *mut Instruction;
    fn qemu_plugin_insn_vaddr(instruction: *const Instruction) -> u64;
    fn qemu_plugin_insn_size(instruction: *const Instruction) -> usize;
    fn qemu_plugin_insn_disas(instruction: *const Instruction) -> *mut c_char;
    fn qemu_plugin_register_vcpu_tb_exec_cb(
        tb: *mut TranslationBlock,
        callback: ExecutionCallback,
        flags: c_int,
        userdata: *mut c_void,
    );
    fn qemu_plugin_register_vcpu_mem_cb(
        instruction: *mut Instruction,
        callback: MemoryCallback,
        flags: c_int,
        read_write: c_int,
        userdata: *mut c_void,
    );
    fn qemu_plugin_register_vcpu_insn_exec_cb(
        instruction: *mut Instruction,
        callback: ExecutionCallback,
        flags: c_int,
        userdata: *mut c_void,
    );
    fn qemu_plugin_register_vcpu_init_cb(id: PluginId, callback: VcpuInitCallback);
    fn qemu_plugin_start_code() -> u64;
    fn qemu_plugin_end_code() -> u64;
    fn qemu_plugin_entry_code() -> u64;
    fn qemu_plugin_get_registers() -> *mut GArray;
    fn qemu_plugin_read_register(handle: *mut QemuRegister, buffer: *mut GByteArray) -> bool;
    fn qemu_plugin_mem_size_shift(info: MemInfo) -> c_uint;
    fn qemu_plugin_mem_is_store(info: MemInfo) -> bool;
    fn qemu_plugin_register_atexit_cb(id: PluginId, callback: ExitCallback, userdata: *mut c_void);
    fn qemu_plugin_outs(message: *const c_char);
    fn g_array_free(array: *mut GArray, free_segment: bool) -> *mut c_char;
    fn g_byte_array_new() -> *mut GByteArray;
    fn g_byte_array_set_size(array: *mut GByteArray, length: c_uint) -> *mut GByteArray;
    fn g_byte_array_unref(array: *mut GByteArray);
    fn g_free(memory: *mut c_void);
}

#[unsafe(no_mangle)]
pub static qemu_plugin_version: c_int = REQUIRED_QEMU_PLUGIN_API;

static OUTPUT: OnceLock<PathBuf> = OnceLock::new();
static TARGET: OnceLock<Target> = OnceLock::new();
static IMAGE: OnceLock<ImageInfo> = OnceLock::new();
static BLOCK_COSTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static RVV_COSTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static RVV_REGISTERS: Mutex<Vec<RvvRegisters>> = Mutex::new(Vec::new());
static SCALAR_INT_OPS: AtomicU64 = AtomicU64::new(0);
static SCALAR_FLOAT_OPS: AtomicU64 = AtomicU64::new(0);
static SCALAR_DOUBLE_OPS: AtomicU64 = AtomicU64::new(0);
static VECTOR_INT_OPS: AtomicU64 = AtomicU64::new(0);
static VECTOR_FLOAT_OPS: AtomicU64 = AtomicU64::new(0);
static VECTOR_DOUBLE_OPS: AtomicU64 = AtomicU64::new(0);
static BYTES_LOAD: AtomicU64 = AtomicU64::new(0);
static BYTES_STORE: AtomicU64 = AtomicU64::new(0);
static DRAM_BYTES_LOAD: AtomicU64 = AtomicU64::new(0);
static DRAM_BYTES_STORE: AtomicU64 = AtomicU64::new(0);
static RVV_STATE_ERRORS: AtomicU64 = AtomicU64::new(0);
static UNCLASSIFIED_INSTRUCTIONS: AtomicU64 = AtomicU64::new(0);
static RETIRED_INSTRUCTIONS: AtomicU64 = AtomicU64::new(0);
static CHILD_PROCESS_SEEN: AtomicBool = AtomicBool::new(false);
static ROOT_PID: OnceLock<libc::pid_t> = OnceLock::new();
static RVV_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);
static UNCLASSIFIED_MNEMONICS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CACHE_MODEL: OnceLock<Mutex<CacheModel>> = OnceLock::new();
static MEMORY_ANALYSIS: OnceLock<Mutex<MemoryAnalysis>> = OnceLock::new();
static DYNAMIC_CFG: Mutex<DynamicCfg> = Mutex::new(DynamicCfg::new());

const DEFAULT_CACHE_LINE_SIZE: u64 = 64;
const DEFAULT_LLC_SIZE: u64 = 8 * 1024 * 1024;
const DEFAULT_LLC_ASSOCIATIVITY: usize = 16;

#[derive(Clone, Copy, Default)]
struct RvvRegisters {
    vl: usize,
    vtype: usize,
    vstart: usize,
    v0: usize,
}

impl RvvRegisters {
    fn has_required_state(self) -> bool {
        self.vl != 0 && self.vtype != 0
    }
}

struct RegisterBuffer(*mut GByteArray);

impl RegisterBuffer {
    fn new() -> Self {
        Self(unsafe { g_byte_array_new() })
    }
}

impl Drop for RegisterBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { g_byte_array_unref(self.0) };
        }
    }
}

thread_local! {
    static REGISTER_BUFFER: RegisterBuffer = RegisterBuffer::new();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RvvCost {
    block: u64,
    kind: RvvKind,
    factor: u64,
    masked: bool,
    sew_scale: u64,
}

extern "C" fn execute_block(vcpu: c_uint, userdata: *mut c_void) {
    if ROOT_PID
        .get()
        .is_some_and(|pid| *pid != unsafe { libc::getpid() })
    {
        CHILD_PROCESS_SEEN.store(true, Ordering::Relaxed);
        return;
    }
    let cost = unsafe { &*(userdata.cast::<BlockCost>()) };
    RETIRED_INSTRUCTIONS.fetch_add(cost.instructions, Ordering::Relaxed);
    SCALAR_INT_OPS.fetch_add(cost.scalar_int, Ordering::Relaxed);
    SCALAR_FLOAT_OPS.fetch_add(cost.scalar_float, Ordering::Relaxed);
    SCALAR_DOUBLE_OPS.fetch_add(cost.scalar_double, Ordering::Relaxed);
    VECTOR_INT_OPS.fetch_add(cost.vector_int, Ordering::Relaxed);
    VECTOR_FLOAT_OPS.fetch_add(cost.vector_float, Ordering::Relaxed);
    VECTOR_DOUBLE_OPS.fetch_add(cost.vector_double, Ordering::Relaxed);

    DYNAMIC_CFG
        .lock()
        .unwrap()
        .record_block(vcpu as usize, cost);
}

extern "C" fn execute_unclassified(_vcpu: c_uint, userdata: *mut c_void) {
    if ROOT_PID
        .get()
        .is_some_and(|pid| *pid != unsafe { libc::getpid() })
    {
        CHILD_PROCESS_SEEN.store(true, Ordering::Relaxed);
        return;
    }
    UNCLASSIFIED_INSTRUCTIONS.fetch_add(1, Ordering::Relaxed);
    let block_address = userdata as usize as u64;
    DYNAMIC_CFG
        .lock()
        .unwrap()
        .attribute_unclassified(block_address);
}

extern "C" fn memory_access(vcpu: c_uint, info: MemInfo, address: u64, userdata: *mut c_void) {
    if ROOT_PID
        .get()
        .is_some_and(|pid| *pid != unsafe { libc::getpid() })
    {
        CHILD_PROCESS_SEEN.store(true, Ordering::Relaxed);
        return;
    }
    let shift = unsafe { qemu_plugin_mem_size_shift(info) };
    let bytes = 1_u64.checked_shl(shift).unwrap_or_default();
    let store = unsafe { qemu_plugin_mem_is_store(info) };
    if let Some(analysis) = MEMORY_ANALYSIS.get() {
        analysis
            .lock()
            .unwrap()
            .access(vcpu as usize, address, bytes, store);
    }
    let counter = if store { &BYTES_STORE } else { &BYTES_LOAD };
    counter.fetch_add(bytes, Ordering::Relaxed);
    let traffic = CACHE_MODEL
        .get()
        .expect("cache model initialized before translation")
        .lock()
        .unwrap()
        .access(address, bytes, store);
    DRAM_BYTES_LOAD.fetch_add(traffic.bytes_load, Ordering::Relaxed);
    DRAM_BYTES_STORE.fetch_add(traffic.bytes_store, Ordering::Relaxed);
    let block_address = userdata as usize as u64;
    let (arch_load, arch_store) = if store { (0, bytes) } else { (bytes, 0) };
    DYNAMIC_CFG.lock().unwrap().attribute_memory(
        block_address,
        arch_load,
        arch_store,
        traffic.bytes_load,
        traffic.bytes_store,
    );
}

extern "C" fn initialize_vcpu(_id: PluginId, vcpu: c_uint) {
    IMAGE.get_or_init(|| ImageInfo {
        start: unsafe { qemu_plugin_start_code() },
        end: unsafe { qemu_plugin_end_code() },
        entry: unsafe { qemu_plugin_entry_code() },
    });
    if TARGET.get() != Some(&Target::Riscv) {
        return;
    }

    let array = unsafe { qemu_plugin_get_registers() };
    if array.is_null() {
        return;
    }

    let mut registers = RvvRegisters::default();
    let mut register_names = Vec::new();
    let descriptors = unsafe {
        std::slice::from_raw_parts(
            (*array).data.cast::<RegisterDescriptor>(),
            (*array).len as usize,
        )
    };
    for descriptor in descriptors {
        if descriptor.name.is_null() {
            continue;
        }
        let name = unsafe { CStr::from_ptr(descriptor.name) }
            .to_string_lossy()
            .to_ascii_lowercase();
        let name = name.trim_start_matches('$');
        register_names.push(name.to_string());
        let handle = descriptor.handle as usize;
        match name {
            "vl" => registers.vl = handle,
            "vtype" => registers.vtype = handle,
            "vstart" => registers.vstart = handle,
            "v0" => registers.v0 = handle,
            _ => {}
        }
    }
    unsafe {
        g_array_free(array, true);
    }

    if !registers.has_required_state() {
        eprintln!(
            "miniperf roofline: RVV registers not exposed by QEMU; available registers: {}",
            register_names.join(", ")
        );
    }
    let mut all_registers = RVV_REGISTERS.lock().unwrap();
    if all_registers.len() <= vcpu as usize {
        all_registers.resize(vcpu as usize + 1, RvvRegisters::default());
    }
    all_registers[vcpu as usize] = registers;
}

extern "C" fn execute_rvv(vcpu: c_uint, userdata: *mut c_void) {
    if ROOT_PID
        .get()
        .is_some_and(|pid| *pid != unsafe { libc::getpid() })
    {
        CHILD_PROCESS_SEEN.store(true, Ordering::Relaxed);
        return;
    }
    let cost = unsafe { &*(userdata.cast::<RvvCost>()) };
    let registers = {
        let registers = RVV_REGISTERS.lock().unwrap();
        registers.get(vcpu as usize).copied()
    };
    let Some(registers) = registers.filter(|registers| registers.has_required_state()) else {
        rvv_state_error("register handles are unavailable");
        return;
    };
    let Some((vl, _)) = read_register_value(registers.vl) else {
        rvv_state_error("reading vl failed");
        return;
    };
    let Some((vtype, xlen)) = read_register_value(registers.vtype) else {
        rvv_state_error("reading vtype failed");
        return;
    };
    let vstart = if registers.vstart == 0 {
        0
    } else {
        let Some((vstart, _)) = read_register_value(registers.vstart) else {
            rvv_state_error("reading vstart failed");
            return;
        };
        vstart
    };
    // LMUL and VLEN are already reflected in the architectural vl value. SEW
    // is still needed to select the output precision bucket.
    let Some(sew) = rvv_sew(vtype, xlen).and_then(|sew| sew.checked_mul(cost.sew_scale)) else {
        rvv_state_error("vtype contains vill or an unsupported SEW");
        return;
    };

    let elements = if cost.masked {
        if registers.v0 == 0 {
            rvv_state_error("the v0 mask register is unavailable");
            return;
        }
        let Some(elements) = read_mask_elements(registers.v0, vstart, vl) else {
            rvv_state_error("reading the v0 mask failed");
            return;
        };
        elements
    } else {
        active_elements(vstart, vl, None).unwrap()
    };
    let operations = elements.saturating_mul(cost.factor);
    let class = match cost.kind {
        RvvKind::Integer => {
            VECTOR_INT_OPS.fetch_add(operations, Ordering::Relaxed);
            VectorClass::Integer
        }
        RvvKind::Float if sew == 64 => {
            VECTOR_DOUBLE_OPS.fetch_add(operations, Ordering::Relaxed);
            VectorClass::Double
        }
        RvvKind::Float => {
            VECTOR_FLOAT_OPS.fetch_add(operations, Ordering::Relaxed);
            VectorClass::Float
        }
    };
    DYNAMIC_CFG
        .lock()
        .unwrap()
        .attribute_vector(cost.block, class, operations);
}

fn rvv_state_error(message: &str) {
    RVV_STATE_ERRORS.fetch_add(1, Ordering::Relaxed);
    if !RVV_ERROR_REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!("miniperf roofline: {message}");
    }
}

fn read_register_value(handle: usize) -> Option<(u64, u32)> {
    REGISTER_BUFFER.with(|buffer| {
        let buffer = buffer.0;
        if buffer.is_null()
            || unsafe {
                g_byte_array_set_size(buffer, 0);
                !qemu_plugin_read_register(handle as *mut QemuRegister, buffer)
            }
        {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts((*buffer).data, (*buffer).len as usize) };
        if bytes.is_empty() || bytes.len() > size_of::<u64>() {
            return None;
        }
        let mut value = [0_u8; size_of::<u64>()];
        value[..bytes.len()].copy_from_slice(bytes);
        Some((u64::from_le_bytes(value), bytes.len() as u32 * 8))
    })
}

fn read_mask_elements(handle: usize, start: u64, end: u64) -> Option<u64> {
    REGISTER_BUFFER.with(|buffer| {
        let buffer = buffer.0;
        if buffer.is_null()
            || unsafe {
                g_byte_array_set_size(buffer, 0);
                !qemu_plugin_read_register(handle as *mut QemuRegister, buffer)
            }
        {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts((*buffer).data, (*buffer).len as usize) };
        active_elements(start, end, Some(bytes))
    })
}

extern "C" fn translate_block(_id: PluginId, tb: *mut TranslationBlock) {
    let block_address = unsafe { qemu_plugin_tb_vaddr(tb) };
    let instruction_count = unsafe { qemu_plugin_tb_n_insns(tb) };
    let mut cost = BlockCost {
        vaddr: block_address,
        instructions: instruction_count as u64,
        ..BlockCost::default()
    };

    for index in 0..instruction_count {
        let instruction = unsafe { qemu_plugin_tb_get_insn(tb, index) };
        if index + 1 == instruction_count {
            cost.end_vaddr = unsafe { qemu_plugin_insn_vaddr(instruction) }
                .saturating_add(unsafe { qemu_plugin_insn_size(instruction) } as u64);
        }
        let disassembly = unsafe { qemu_plugin_insn_disas(instruction) };
        if !disassembly.is_null() {
            let text = unsafe { CStr::from_ptr(disassembly) }.to_string_lossy();
            if index + 1 == instruction_count {
                cost.flow = classify_flow(TARGET.get().copied(), &text);
            }
            if TARGET.get() == Some(&Target::Riscv) {
                match classify_riscv(&text) {
                    RiscvClassification::Counted(riscv_cost) => match riscv_cost.kind {
                        RiscvKind::VectorInteger | RiscvKind::VectorFloat => {
                            let rvv_cost = Box::into_raw(Box::new(RvvCost {
                                block: block_address,
                                kind: if riscv_cost.kind == RiscvKind::VectorFloat {
                                    RvvKind::Float
                                } else {
                                    RvvKind::Integer
                                },
                                factor: riscv_cost.factor,
                                masked: is_masked(&text),
                                sew_scale: riscv_cost.sew_scale,
                            }));
                            RVV_COSTS.lock().unwrap().push(rvv_cost as usize);
                            unsafe {
                                qemu_plugin_register_vcpu_insn_exec_cb(
                                    instruction,
                                    execute_rvv,
                                    CALLBACK_READ_REGS,
                                    rvv_cost.cast(),
                                )
                            };
                        }
                        RiscvKind::ScalarInteger => {
                            cost.scalar_int += riscv_cost.factor;
                        }
                        RiscvKind::ScalarFloat => {
                            cost.scalar_float += riscv_cost.factor;
                        }
                        RiscvKind::ScalarDouble => {
                            cost.scalar_double += riscv_cost.factor;
                        }
                    },
                    RiscvClassification::NonCompute => {}
                    RiscvClassification::Unclassified => {
                        report_unclassified(&text);
                        unsafe {
                            qemu_plugin_register_vcpu_insn_exec_cb(
                                instruction,
                                execute_unclassified,
                                CALLBACK_NO_REGS,
                                block_address as usize as *mut c_void,
                            )
                        };
                    }
                }
            } else {
                cost.add(classify_x86(&mnemonic(&text), &text));
            }
            unsafe { g_free(disassembly.cast()) };
        }
        unsafe {
            qemu_plugin_register_vcpu_mem_cb(
                instruction,
                memory_access,
                CALLBACK_NO_REGS,
                MEMORY_READ_WRITE,
                block_address as usize as *mut c_void,
            )
        };
    }

    let cost = Box::into_raw(Box::new(cost));
    BLOCK_COSTS.lock().unwrap().push(cost as usize);
    unsafe {
        qemu_plugin_register_vcpu_tb_exec_cb(tb, execute_block, CALLBACK_NO_REGS, cost.cast())
    };
}

fn report_unclassified(disassembly: &str) {
    let mnemonic = mnemonic(disassembly);
    let operation = if is_masked(disassembly) {
        format!("{mnemonic} (masked)")
    } else {
        mnemonic
    };
    let mut reported = UNCLASSIFIED_MNEMONICS.lock().unwrap();
    if !reported.contains(&operation) {
        eprintln!(
            "miniperf roofline: TMDL cannot classify RISC-V operation '{operation}'; counting it as zero"
        );
        reported.push(operation);
    }
}

extern "C" fn plugin_exit(_id: PluginId, _userdata: *mut c_void) {
    if ROOT_PID
        .get()
        .is_some_and(|pid| *pid != unsafe { libc::getpid() })
    {
        CHILD_PROCESS_SEEN.store(true, Ordering::Relaxed);
        return;
    }
    if let Some(path) = OUTPUT.get() {
        if let Some(cache) = CACHE_MODEL.get() {
            let traffic = cache.lock().unwrap().flush();
            DRAM_BYTES_LOAD.fetch_add(traffic.bytes_load, Ordering::Relaxed);
            DRAM_BYTES_STORE.fetch_add(traffic.bytes_store, Ordering::Relaxed);
        }
        let counters = CounterSnapshot {
            scalar_int_ops: SCALAR_INT_OPS.load(Ordering::Relaxed),
            scalar_float_ops: SCALAR_FLOAT_OPS.load(Ordering::Relaxed),
            scalar_double_ops: SCALAR_DOUBLE_OPS.load(Ordering::Relaxed),
            vector_int_ops: VECTOR_INT_OPS.load(Ordering::Relaxed),
            vector_float_ops: VECTOR_FLOAT_OPS.load(Ordering::Relaxed),
            vector_double_ops: VECTOR_DOUBLE_OPS.load(Ordering::Relaxed),
            bytes_load: BYTES_LOAD.load(Ordering::Relaxed),
            bytes_store: BYTES_STORE.load(Ordering::Relaxed),
            dram_bytes_load: DRAM_BYTES_LOAD.load(Ordering::Relaxed),
            dram_bytes_store: DRAM_BYTES_STORE.load(Ordering::Relaxed),
            rvv_state_errors: RVV_STATE_ERRORS.load(Ordering::Relaxed),
            unclassified_instructions: UNCLASSIFIED_INSTRUCTIONS.load(Ordering::Relaxed),
            instructions: RETIRED_INSTRUCTIONS.load(Ordering::Relaxed),
            child_process_seen: CHILD_PROCESS_SEEN.load(Ordering::Relaxed),
        };
        artifacts::write_counts(path, &counters);
        let cache = CACHE_MODEL.get().map(|cache| {
            let cache = cache.lock().unwrap();
            CacheDescription {
                line_size: cache.line_size(),
                capacity: cache.capacity(),
                associativity: cache.associativity(),
            }
        });
        artifacts::write_cfg(
            &path.with_extension("cfg"),
            cache,
            IMAGE.get().copied(),
            &DYNAMIC_CFG.lock().unwrap(),
        );
        if let Some(analysis) = MEMORY_ANALYSIS.get() {
            artifacts::write_memory(
                &path.with_extension("memory.json"),
                &analysis.lock().unwrap().artifact(),
            );
        }
    }

    for cost in std::mem::take(&mut *BLOCK_COSTS.lock().unwrap()) {
        unsafe { drop(Box::from_raw(cost as *mut BlockCost)) };
    }
    for cost in std::mem::take(&mut *RVV_COSTS.lock().unwrap()) {
        unsafe { drop(Box::from_raw(cost as *mut RvvCost)) };
    }
}

/// Installs the plugin into the QEMU process.
///
/// # Safety
///
/// QEMU must pass pointers matching the public `qemu-plugin.h` ABI for the
/// advertised plugin API version.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qemu_plugin_install(
    id: PluginId,
    info: *const QemuInfo,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    let _ = ROOT_PID.set(unsafe { libc::getpid() });
    if info.is_null() {
        return -1;
    }
    if unsafe { (*info).version.current } < REQUIRED_QEMU_PLUGIN_API {
        unsafe { qemu_plugin_outs(c"miniperf roofline: QEMU plugin API 6 is required\n".as_ptr()) };
        return -1;
    }
    let target_name = unsafe { CStr::from_ptr((*info).target_name) }.to_string_lossy();
    let target = if target_name.starts_with("riscv") {
        Target::Riscv
    } else if target_name == "x86_64" || target_name == "i386" {
        Target::X86
    } else {
        unsafe { qemu_plugin_outs(c"miniperf roofline: unsupported QEMU target\n".as_ptr()) };
        return -1;
    };
    if TARGET.set(target).is_err() {
        return -1;
    }
    let args = if argc <= 0 || argv.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc as usize) }
    };
    let arguments = args
        .iter()
        .map(|arg| {
            unsafe { CStr::from_ptr(*arg) }
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    let output = arguments
        .iter()
        .find_map(|arg| arg.strip_prefix("output=").map(PathBuf::from));
    let Some(output) = output else {
        unsafe { qemu_plugin_outs(c"miniperf roofline: missing output=<path>\n".as_ptr()) };
        return -1;
    };
    if OUTPUT.set(output).is_err() {
        return -1;
    }
    let parse_u64 = |name: &str, default: u64| {
        arguments
            .iter()
            .find_map(|argument| argument.strip_prefix(&format!("{name}=")))
            .map(str::parse::<u64>)
            .transpose()
            .map(|value| value.unwrap_or(default))
    };
    let cache_line = parse_u64("cache-line", DEFAULT_CACHE_LINE_SIZE);
    let cache_size = parse_u64("llc-size", DEFAULT_LLC_SIZE);
    let cache_associativity = parse_u64("llc-assoc", DEFAULT_LLC_ASSOCIATIVITY as u64)
        .ok()
        .and_then(|value| usize::try_from(value).ok());
    let memory_profile = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("memory-profile="))
        .map(|value| match value {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err(()),
        })
        .transpose();
    let memory_profile = match memory_profile {
        Ok(value) => value.unwrap_or(true),
        Err(()) => {
            unsafe {
                qemu_plugin_outs(
                    c"miniperf roofline: memory-profile must be 'on' or 'off'\n".as_ptr(),
                )
            };
            return -1;
        }
    };
    let cache = match (cache_line.ok(), cache_size.ok(), cache_associativity) {
        (Some(line), Some(size), Some(associativity)) => CacheModel::new(line, size, associativity),
        _ => None,
    };
    let Some(cache) = cache else {
        unsafe {
            qemu_plugin_outs(
                c"miniperf roofline: invalid cache-line, llc-size, or llc-assoc option\n".as_ptr(),
            )
        };
        return -1;
    };
    if CACHE_MODEL.set(Mutex::new(cache)).is_err() {
        return -1;
    }
    let line_size = CACHE_MODEL
        .get()
        .map(|cache| cache.lock().unwrap().line_size())
        .unwrap_or(DEFAULT_CACHE_LINE_SIZE);
    if memory_profile
        && MEMORY_ANALYSIS
            .set(Mutex::new(MemoryAnalysis::new(line_size)))
            .is_err()
    {
        return -1;
    }

    unsafe {
        qemu_plugin_register_vcpu_init_cb(id, initialize_vcpu);
        qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
        qemu_plugin_register_atexit_cb(id, plugin_exit, ptr::null_mut());
    }
    0
}
