//! miniperf OMPT proxy: activated via `OMP_TOOL_LIBRARIES`. v1 coverage per
//! the redesign: parallel-region begin/end, task create/schedule (spans +
//! flow IDs), implicit tasks, sync-region waits, thread begin/end. The OMPT
//! ABI subset used here is fixed by the OpenMP 5.x specification (enum values
//! verified against LLVM's omp-tools.h), so no OpenMP headers are needed.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

#[path = "../../src/dynload.rs"]
mod dynload;

const KIND_BEGIN: u8 = 0;
const KIND_END: u8 = 1;
const KIND_INSTANT: u8 = 2;

const CALLBACK_THREAD_BEGIN: c_int = 1;
const CALLBACK_THREAD_END: c_int = 2;
const CALLBACK_PARALLEL_BEGIN: c_int = 3;
const CALLBACK_PARALLEL_END: c_int = 4;
const CALLBACK_TASK_CREATE: c_int = 5;
const CALLBACK_TASK_SCHEDULE: c_int = 6;
const CALLBACK_IMPLICIT_TASK: c_int = 7;
const CALLBACK_SYNC_REGION_WAIT: c_int = 16;

const SCOPE_BEGIN: c_int = 1;

const SYNC_TASKWAIT: c_int = 5;
const SYNC_TASKGROUP: c_int = 6;
const SYNC_REDUCTION: c_int = 7;

#[repr(C)]
pub union OmptData {
    pub value: u64,
    pub pointer: *mut c_void,
}

#[repr(C)]
pub struct OmptFrame {
    pub exit_frame: OmptData,
    pub enter_frame: OmptData,
    pub exit_frame_flags: c_int,
    pub enter_frame_flags: c_int,
}

type LookupFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type SetCallbackFn = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;

#[repr(C)]
pub struct OmptStartToolResult {
    pub initialize: unsafe extern "C" fn(LookupFn, c_int, *mut OmptData) -> c_int,
    pub finalize: unsafe extern "C" fn(*mut OmptData),
    pub tool_data: u64,
}

#[repr(C)]
struct Payload {
    name: *const c_char,
    function: *const c_char,
    file: *const c_char,
    line: u32,
    column: u32,
    flags: u32,
}

type RegisterFn = unsafe extern "C" fn(*const Payload) -> *mut c_void;
type EmitFn = unsafe extern "C" fn(*mut c_void, u8, i64, u64, u64, c_int) -> u64;

static CORE_REGISTER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

macro_rules! handles {
    ($($slot:ident => $name:literal),+ $(,)?) => {
        $(static $slot: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());)+
        unsafe fn register_all() {
            $(
                let payload = Payload {
                    name: concat!($name, "\0").as_ptr() as *const c_char,
                    function: c"".as_ptr(),
                    file: c"openmp".as_ptr(),
                    line: 0,
                    column: 0,
                    flags: 0,
                };
                let register: RegisterFn =
                    unsafe { std::mem::transmute(CORE_REGISTER.load(Ordering::Acquire)) };
                $slot.store(unsafe { register(&payload) }, Ordering::Release);
            )+
        }
    };
}

handles! {
    H_PARALLEL => "omp_parallel",
    H_TASK => "omp_task",
    H_TASK_CREATE => "omp_task_create",
    H_IMPLICIT => "omp_implicit_task",
    H_THREAD_BEGIN => "omp_thread_begin",
    H_THREAD_END => "omp_thread_end",
    H_BARRIER => "omp_barrier",
    H_TASKWAIT => "omp_taskwait",
    H_TASKGROUP => "omp_taskgroup",
    H_REDUCTION => "omp_reduction",
}

fn emit(slot: &AtomicPtr<c_void>, kind: u8, value: i64, parent: u64, flow: u64) -> u64 {
    let handle = slot.load(Ordering::Acquire);
    if handle.is_null() {
        return 0;
    }
    let emit: EmitFn = unsafe { std::mem::transmute(CORE_EMIT.load(Ordering::Acquire)) };
    unsafe { emit(handle, kind, value, parent, flow, 0) }
}

const SYNC_DEPTH: usize = 16;
thread_local! {
    static SYNC_STACK: Cell<[u64; SYNC_DEPTH]> = const { Cell::new([0; SYNC_DEPTH]) };
    static SYNC_TOP: Cell<usize> = const { Cell::new(0) };
    static IMPLICIT_SPAN: Cell<u64> = const { Cell::new(0) };
}

unsafe extern "C" fn on_thread_begin(thread_type: c_int, _thread_data: *mut OmptData) {
    emit(&H_THREAD_BEGIN, KIND_INSTANT, thread_type as i64, 0, 0);
}

unsafe extern "C" fn on_thread_end(_thread_data: *mut OmptData) {
    emit(&H_THREAD_END, KIND_INSTANT, 0, 0, 0);
}

unsafe extern "C" fn on_parallel_begin(
    task_data: *mut OmptData,
    _frame: *const OmptFrame,
    parallel_data: *mut OmptData,
    requested_parallelism: u32,
    _flags: c_int,
    _codeptr: *const c_void,
) {
    let parent = unsafe { task_data.as_ref() }.map_or(0, |data| unsafe { data.value });
    let span = emit(
        &H_PARALLEL,
        KIND_BEGIN,
        requested_parallelism as i64,
        parent,
        0,
    );
    if let Some(data) = unsafe { parallel_data.as_mut() } {
        data.value = span;
    }
}

unsafe extern "C" fn on_parallel_end(
    parallel_data: *mut OmptData,
    _task_data: *mut OmptData,
    _flags: c_int,
    _codeptr: *const c_void,
) {
    let span = unsafe { parallel_data.as_ref() }.map_or(0, |data| unsafe { data.value });
    emit(&H_PARALLEL, KIND_END, 0, 0, span);
}

unsafe extern "C" fn on_task_create(
    task_data: *mut OmptData,
    _frame: *const OmptFrame,
    new_task_data: *mut OmptData,
    flags: c_int,
    _has_dependences: c_int,
    _codeptr: *const c_void,
) {
    let parent = unsafe { task_data.as_ref() }.map_or(0, |data| unsafe { data.value });
    let instance = emit(&H_TASK_CREATE, KIND_INSTANT, flags as i64, parent, 0);
    if let Some(data) = unsafe { new_task_data.as_mut() } {
        data.value = instance;
    }
}

unsafe extern "C" fn on_task_schedule(
    prior_task_data: *mut OmptData,
    _prior_status: c_int,
    next_task_data: *mut OmptData,
) {
    let prior = unsafe { prior_task_data.as_ref() }.map_or(0, |data| unsafe { data.value });
    if prior != 0 {
        emit(&H_TASK, KIND_END, 0, 0, prior);
    }
    let next = unsafe { next_task_data.as_ref() }.map_or(0, |data| unsafe { data.value });
    if next != 0 {
        emit(&H_TASK, KIND_BEGIN, 0, 0, next);
    }
}

unsafe extern "C" fn on_implicit_task(
    endpoint: c_int,
    parallel_data: *mut OmptData,
    task_data: *mut OmptData,
    _actual_parallelism: u32,
    index: u32,
    _flags: c_int,
) {
    if endpoint == SCOPE_BEGIN {
        let parent = unsafe { parallel_data.as_ref() }.map_or(0, |data| unsafe { data.value });
        let span = emit(&H_IMPLICIT, KIND_BEGIN, index as i64, parent, 0);
        let _ = IMPLICIT_SPAN.try_with(|cell| cell.set(span));
        if let Some(data) = unsafe { task_data.as_mut() } {
            data.value = span;
        }
    } else {
        let span = IMPLICIT_SPAN.try_with(|cell| cell.replace(0)).unwrap_or(0);
        emit(&H_IMPLICIT, KIND_END, 0, 0, span);
    }
}

unsafe extern "C" fn on_sync_region_wait(
    kind: c_int,
    endpoint: c_int,
    parallel_data: *mut OmptData,
    _task_data: *mut OmptData,
    _codeptr: *const c_void,
) {
    let slot = match kind {
        SYNC_TASKWAIT => &H_TASKWAIT,
        SYNC_TASKGROUP => &H_TASKGROUP,
        SYNC_REDUCTION => &H_REDUCTION,
        _ => &H_BARRIER,
    };
    if endpoint == SCOPE_BEGIN {
        let parent = unsafe { parallel_data.as_ref() }.map_or(0, |data| unsafe { data.value });
        let span = emit(slot, KIND_BEGIN, kind as i64, parent, 0);
        let top = SYNC_TOP.try_with(|top| {
            let value = top.get();
            top.set(value + 1);
            value
        });
        if let Ok(top) = top
            && top < SYNC_DEPTH
        {
            let _ = SYNC_STACK.try_with(|stack| {
                let mut frames = stack.get();
                frames[top] = span;
                stack.set(frames);
            });
        }
    } else {
        let top = SYNC_TOP
            .try_with(|top| {
                let value = top.get();
                if value > 0 {
                    top.set(value - 1);
                }
                value
            })
            .unwrap_or(0);
        let span = if top > 0 && top - 1 < SYNC_DEPTH {
            SYNC_STACK
                .try_with(|stack| stack.get()[top - 1])
                .unwrap_or(0)
        } else {
            0
        };
        emit(slot, KIND_END, 0, 0, span);
    }
}

unsafe extern "C" fn tool_initialize(
    lookup: LookupFn,
    _initial_device: c_int,
    _tool_data: *mut OmptData,
) -> c_int {
    let library = std::env::var("MPERF_COLLECTOR_LIBRARY")
        .unwrap_or_else(|_| dynload::CORE_LIBRARY.to_string());
    let Ok(library) = std::ffi::CString::new(library) else {
        return 0;
    };
    let core = unsafe { dynload::load(&library) };
    if core.is_null() {
        return 0;
    }
    let register = unsafe { dynload::symbol(core, c"mperf_trace_register") };
    let emit = unsafe { dynload::symbol(core, c"mperf_trace_emit") };
    if register.is_null() || emit.is_null() {
        return 0;
    }
    CORE_REGISTER.store(register, Ordering::Release);
    CORE_EMIT.store(emit, Ordering::Release);

    let set_callback = unsafe { lookup(c"ompt_set_callback".as_ptr()) };
    if set_callback.is_null() {
        return 0;
    }
    let set_callback: SetCallbackFn = unsafe { std::mem::transmute(set_callback) };

    unsafe {
        register_all();
        set_callback(CALLBACK_THREAD_BEGIN, on_thread_begin as *mut c_void);
        set_callback(CALLBACK_THREAD_END, on_thread_end as *mut c_void);
        set_callback(CALLBACK_PARALLEL_BEGIN, on_parallel_begin as *mut c_void);
        set_callback(CALLBACK_PARALLEL_END, on_parallel_end as *mut c_void);
        set_callback(CALLBACK_TASK_CREATE, on_task_create as *mut c_void);
        set_callback(CALLBACK_TASK_SCHEDULE, on_task_schedule as *mut c_void);
        set_callback(CALLBACK_IMPLICIT_TASK, on_implicit_task as *mut c_void);
        set_callback(
            CALLBACK_SYNC_REGION_WAIT,
            on_sync_region_wait as *mut c_void,
        );
    }
    1
}

unsafe extern "C" fn tool_finalize(_tool_data: *mut OmptData) {}

/// OMPT entry point: the OpenMP runtime calls this after dlopening the
/// library named in `OMP_TOOL_LIBRARIES`.
#[unsafe(no_mangle)]
pub extern "C" fn ompt_start_tool(
    _omp_version: u32,
    _runtime_version: *const c_char,
) -> *mut OmptStartToolResult {
    if std::env::var_os("MPERF_SESSION_DIR").is_none() {
        return std::ptr::null_mut();
    }
    static RESULT: OmptStartToolResult = OmptStartToolResult {
        initialize: tool_initialize,
        finalize: tool_finalize,
        tool_data: 0,
    };
    &RESULT as *const OmptStartToolResult as *mut OmptStartToolResult
}
