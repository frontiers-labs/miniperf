//! miniperf ITT collector: named by `INTEL_LIBITTNOTIFY64`, dlopened by the
//! ittnotify static loader inside TBB/oneAPI runtimes, which calls
//! `__itt_api_init` and resolves each `__itt_*` entry against this library's
//! exports. v1 coverage per the redesign: task begin/end, frames, domains and
//! string handles. The ABI subset is fixed by the ittapi headers (layouts
//! verified against intel/ittapi).

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

#[path = "../../src/dynload.rs"]
mod dynload;

const KIND_BEGIN: u8 = 0;
const KIND_END: u8 = 1;

#[repr(C)]
pub struct IttDomain {
    pub flags: c_int,
    pub name_ascii: *const c_char,
    pub name_wide: *const c_void,
    pub extra1: c_int,
    pub extra2: *mut c_void,
    pub next: *mut IttDomain,
}

#[repr(C)]
pub struct IttStringHandle {
    pub string_ascii: *const c_char,
    pub string_wide: *const c_void,
    pub extra1: c_int,
    pub extra2: *mut c_void,
    pub next: *mut IttStringHandle,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IttId {
    pub d1: u64,
    pub d2: u64,
    pub d3: u64,
}

#[repr(C)]
pub struct IttApiInfo {
    pub name: *const c_char,
    pub func_ptr: *mut *mut c_void,
    pub init_func: *mut c_void,
    pub null_func: *mut c_void,
    pub group: c_int,
}

#[repr(C)]
pub struct IttGlobal {
    pub magic: [u8; 8],
    pub version_major: libc::c_ulong,
    pub version_minor: libc::c_ulong,
    pub version_build: libc::c_ulong,
    pub api_initialized: libc::c_long,
    pub mutex_initialized: libc::c_long,
    pub atomic_counter: libc::c_long,
    #[cfg(unix)]
    pub mutex: libc::pthread_mutex_t,
    #[cfg(all(windows, target_pointer_width = "64"))]
    pub mutex: [u8; 40],
    #[cfg(all(windows, target_pointer_width = "32"))]
    pub mutex: [u8; 24],
    pub lib: *mut c_void,
    pub error_handler: *mut c_void,
    pub dll_path_ptr: *const *const c_char,
    pub api_list_ptr: *mut IttApiInfo,
    pub next: *mut IttGlobal,
    pub thread_list: *mut c_void,
    pub domain_list: *mut IttDomain,
    pub string_list: *mut IttStringHandle,
    pub state: c_int,
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

static CORE_STATE: AtomicI32 = AtomicI32::new(0);
static CORE_REGISTER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

struct Registry {
    domains: Vec<*mut IttDomain>,
    strings: Vec<*mut IttStringHandle>,
}

unsafe impl Send for Registry {}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    domains: Vec::new(),
    strings: Vec::new(),
});

const TASK_DEPTH: usize = 64;
thread_local! {
    static TASK_STACK: Cell<[u64; TASK_DEPTH]> = const { Cell::new([0; TASK_DEPTH]) };
    static TASK_TOP: Cell<usize> = const { Cell::new(0) };
}

fn core_resolve() -> bool {
    match CORE_STATE.load(Ordering::Acquire) {
        1 => return true,
        -1 => return false,
        _ => {}
    }
    if std::env::var_os("MPERF_SESSION_DIR").is_none() {
        CORE_STATE.store(-1, Ordering::Release);
        return false;
    }
    let library = std::env::var("MPERF_COLLECTOR_LIBRARY")
        .unwrap_or_else(|_| dynload::CORE_LIBRARY.to_string());
    let Ok(library) = std::ffi::CString::new(library) else {
        CORE_STATE.store(-1, Ordering::Release);
        return false;
    };
    let core = unsafe { dynload::load(&library) };
    if core.is_null() {
        CORE_STATE.store(-1, Ordering::Release);
        return false;
    }
    let register = unsafe { dynload::symbol(core, c"mperf_trace_register") };
    let emit = unsafe { dynload::symbol(core, c"mperf_trace_emit") };
    if register.is_null() || emit.is_null() {
        CORE_STATE.store(-1, Ordering::Release);
        return false;
    }
    CORE_REGISTER.store(register, Ordering::Release);
    CORE_EMIT.store(emit, Ordering::Release);
    CORE_STATE.store(1, Ordering::Release);
    true
}

fn register_payload(name: *const c_char, domain: *const c_char) -> *mut c_void {
    let register: RegisterFn =
        unsafe { std::mem::transmute(CORE_REGISTER.load(Ordering::Acquire)) };
    let payload = Payload {
        name,
        function: if domain.is_null() {
            c"".as_ptr()
        } else {
            domain
        },
        file: c"itt".as_ptr(),
        line: 0,
        column: 0,
        flags: 0,
    };
    unsafe { register(&payload) }
}

fn emit(handle: *mut c_void, kind: u8, value: i64, parent: u64, flow: u64) -> u64 {
    if handle.is_null() {
        return 0;
    }
    let emit: EmitFn = unsafe { std::mem::transmute(CORE_EMIT.load(Ordering::Acquire)) };
    unsafe { emit(handle, kind, value, parent, flow, 0) }
}

fn leak_cstring(value: &CStr) -> *const c_char {
    let owned = value.to_owned();
    let pointer = owned.as_ptr();
    std::mem::forget(owned);
    pointer
}

/// # Safety
/// Called by the ittnotify static loader with a valid NUL-terminated name.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_domain_create(name: *const c_char) -> *mut IttDomain {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let wanted = unsafe { CStr::from_ptr(name) };
    let mut registry = REGISTRY.lock().unwrap();
    for &domain in &registry.domains {
        let existing = unsafe { CStr::from_ptr((*domain).name_ascii) };
        if existing == wanted {
            return domain;
        }
    }
    let domain = Box::into_raw(Box::new(IttDomain {
        flags: 1,
        name_ascii: leak_cstring(wanted),
        name_wide: std::ptr::null(),
        extra1: 0,
        extra2: std::ptr::null_mut(),
        next: std::ptr::null_mut(),
    }));
    registry.domains.push(domain);
    domain
}

/// # Safety
/// See `__itt_domain_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_domain_createA(name: *const c_char) -> *mut IttDomain {
    unsafe { __itt_domain_create(name) }
}

/// # Safety
/// Called by the ittnotify static loader with a valid NUL-terminated name.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_string_handle_create(name: *const c_char) -> *mut IttStringHandle {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let wanted = unsafe { CStr::from_ptr(name) };
    let mut registry = REGISTRY.lock().unwrap();
    for &handle in &registry.strings {
        let existing = unsafe { CStr::from_ptr((*handle).string_ascii) };
        if existing == wanted {
            return handle;
        }
    }
    let handle = Box::into_raw(Box::new(IttStringHandle {
        string_ascii: leak_cstring(wanted),
        string_wide: std::ptr::null(),
        extra1: 0,
        extra2: std::ptr::null_mut(),
        next: std::ptr::null_mut(),
    }));
    registry.strings.push(handle);
    handle
}

/// # Safety
/// See `__itt_string_handle_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_string_handle_createA(name: *const c_char) -> *mut IttStringHandle {
    unsafe { __itt_string_handle_create(name) }
}

/// # Safety
/// ittnotify ABI: `domain`/`name` come from the create functions above.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_task_begin(
    domain: *const IttDomain,
    _task_id: IttId,
    parent_id: IttId,
    name: *mut IttStringHandle,
) {
    if name.is_null() || !core_resolve() {
        return;
    }
    let name = unsafe { &mut *name };
    if name.extra2.is_null() {
        let domain_name = if domain.is_null() {
            std::ptr::null()
        } else {
            unsafe { (*domain).name_ascii }
        };
        name.extra2 = register_payload(name.string_ascii, domain_name);
    }
    let span = emit(name.extra2, KIND_BEGIN, 0, parent_id.d1, 0);
    let top = TASK_TOP.with(|top| {
        let value = top.get();
        top.set(value + 1);
        value
    });
    if top < TASK_DEPTH {
        TASK_STACK.with(|stack| {
            let mut frames = stack.get();
            frames[top] = span;
            stack.set(frames);
        });
    }
}

/// # Safety
/// ittnotify ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_task_end(_domain: *const IttDomain) {
    if !core_resolve() {
        return;
    }
    let top = TASK_TOP.with(|top| {
        let value = top.get();
        if value > 0 {
            top.set(value - 1);
        }
        value
    });
    if top == 0 {
        return;
    }
    let span = if top - 1 < TASK_DEPTH {
        TASK_STACK.with(|stack| stack.get()[top - 1])
    } else {
        0
    };
    static END_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let mut handle = END_HANDLE.load(Ordering::Acquire);
    if handle.is_null() {
        handle = register_payload(c"itt_task_end".as_ptr(), c"itt".as_ptr());
        END_HANDLE.store(handle, Ordering::Release);
    }
    emit(handle, KIND_END, 0, 0, span);
}

/// # Safety
/// ittnotify ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_frame_begin_v3(domain: *mut IttDomain, id: *mut IttId) {
    if domain.is_null() || !core_resolve() {
        return;
    }
    let domain = unsafe { &mut *domain };
    if domain.extra2.is_null() {
        domain.extra2 = register_payload(c"itt_frame".as_ptr(), domain.name_ascii);
    }
    emit(domain.extra2, KIND_BEGIN, 0, 0, id as u64);
}

/// # Safety
/// ittnotify ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_frame_end_v3(domain: *mut IttDomain, id: *mut IttId) {
    if domain.is_null() || !core_resolve() {
        return;
    }
    let extra = unsafe { (*domain).extra2 };
    emit(extra, KIND_END, 0, 0, id as u64);
}

/// The ittnotify static loader's collector entry point: patch every listed
/// API pointer with this library's export of the same name (or the provided
/// null implementation).
///
/// # Safety
/// `global` points to the loader's `__itt_global` with `lib` being this
/// library's dlopen handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __itt_api_init(global: *mut IttGlobal, _init_groups: c_int) {
    if global.is_null() || !core_resolve() {
        return;
    }
    let global = unsafe { &mut *global };
    let mut api = global.api_list_ptr;
    while !api.is_null() && !unsafe { (*api).name }.is_null() {
        let entry = unsafe { &mut *api };
        let implementation = unsafe { dynload::symbol(global.lib, CStr::from_ptr(entry.name)) };
        unsafe {
            *entry.func_ptr = if implementation.is_null() {
                entry.null_func
            } else {
                implementation
            };
        }
        api = unsafe { api.add(1) };
    }
}
