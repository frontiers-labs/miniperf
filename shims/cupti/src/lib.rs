//! miniperf CUPTI proxy: loaded via `CUDA_INJECTION64_PATH`. Built without
//! the CUDA toolkit: `libcupti.so` is dlopened at runtime and only
//! version-stable CUPTI surfaces are used — the callback API
//! (`CUpti_CallbackData` has a frozen layout) for kernel-launch and memory
//! transfer spans, and `cuptiGetTimestamp` for bracketed host/device
//! calibration pairs (post-mortem linear fit maps device timestamps into
//! CLOCK_MONOTONIC). Device-side activity-record timelines need the
//! version-specific activity structs and stay out until built against real
//! headers.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

#[path = "../../src/dynload.rs"]
mod dynload;

const CUPTI_API_ENTER: c_int = 0;
const CUPTI_API_EXIT: c_int = 1;
const CB_DOMAIN_DRIVER_API: c_int = 1;
const CB_DOMAIN_RUNTIME_API: c_int = 2;

#[repr(C)]
struct CallbackData {
    callback_site: c_int,
    function_name: *const c_char,
    function_params: *const c_void,
    function_return_value: *const c_void,
    symbol_name: *const c_char,
    context: *mut c_void,
    context_uid: u32,
    correlation_data: *mut u64,
    correlation_id: u32,
}

type SubscribeFn = unsafe extern "C" fn(
    *mut *mut c_void,
    extern "C" fn(*mut c_void, c_int, u32, *const c_void),
    *mut c_void,
) -> c_int;
type EnableDomainFn = unsafe extern "C" fn(u32, *mut c_void, c_int) -> c_int;
type GetTimestampFn = unsafe extern "C" fn(*mut u64) -> c_int;

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
type DeviceClockFn = unsafe extern "C" fn(*const c_char, i64, i64, i64);
type TimestampFn = unsafe extern "C" fn() -> i64;

const KIND_BEGIN: u8 = 0;
const KIND_END: u8 = 1;

static CORE_REGISTER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_TIMESTAMP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_DEVICE_CLOCK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CUPTI_TIMESTAMP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

struct HandleCache {
    by_callback: HashMap<(c_int, u32), *mut c_void>,
}

unsafe impl Send for HandleCache {}

static HANDLES: std::sync::LazyLock<Mutex<HandleCache>> = std::sync::LazyLock::new(|| {
    Mutex::new(HandleCache {
        by_callback: HashMap::new(),
    })
});

fn interesting(name: &CStr) -> bool {
    let Ok(name) = name.to_str() else {
        return false;
    };
    name.contains("Launch") || name.contains("Memcpy") || name.contains("Memset")
}

fn payload_for(domain: c_int, callback_id: u32, name: *const c_char) -> *mut c_void {
    let mut cache = HANDLES.lock().unwrap();
    if let Some(&handle) = cache.by_callback.get(&(domain, callback_id)) {
        return handle;
    }
    let handle = if name.is_null() || !interesting(unsafe { CStr::from_ptr(name) }) {
        std::ptr::null_mut()
    } else {
        let register: RegisterFn =
            unsafe { std::mem::transmute(CORE_REGISTER.load(Ordering::Acquire)) };
        let payload = Payload {
            name,
            function: c"".as_ptr(),
            file: c"cuda".as_ptr(),
            line: 0,
            column: 0,
            flags: 0,
        };
        unsafe { register(&payload) }
    };
    cache.by_callback.insert((domain, callback_id), handle);
    handle
}

extern "C" fn on_callback(
    _userdata: *mut c_void,
    domain: c_int,
    callback_id: u32,
    data: *const c_void,
) {
    if domain != CB_DOMAIN_RUNTIME_API && domain != CB_DOMAIN_DRIVER_API {
        return;
    }
    let Some(data) = (unsafe { (data as *const CallbackData).as_ref() }) else {
        return;
    };
    let handle = payload_for(domain, callback_id, data.function_name);
    if handle.is_null() {
        return;
    }
    let emit: EmitFn = unsafe { std::mem::transmute(CORE_EMIT.load(Ordering::Acquire)) };
    match data.callback_site {
        CUPTI_API_ENTER => {
            let span = unsafe { emit(handle, KIND_BEGIN, 0, 0, data.correlation_id as u64, 0) };
            if let Some(scratch) = unsafe { data.correlation_data.as_mut() } {
                *scratch = span;
            }
        }
        CUPTI_API_EXIT => {
            let span = unsafe { data.correlation_data.as_ref() }
                .copied()
                .unwrap_or(0);
            unsafe { emit(handle, KIND_END, 0, 0, span, 0) };
        }
        _ => {}
    }
}

fn calibrate() {
    let cupti_timestamp = CUPTI_TIMESTAMP.load(Ordering::Acquire);
    let core_timestamp = CORE_TIMESTAMP.load(Ordering::Acquire);
    if cupti_timestamp.is_null() || core_timestamp.is_null() {
        return;
    }
    let cupti_timestamp: GetTimestampFn = unsafe { std::mem::transmute(cupti_timestamp) };
    let core_timestamp: TimestampFn = unsafe { std::mem::transmute(core_timestamp) };
    let before = unsafe { core_timestamp() };
    let mut device = 0u64;
    if unsafe { cupti_timestamp(&mut device) } != 0 {
        return;
    }
    let after = unsafe { core_timestamp() };
    let core = CORE_DEVICE_CLOCK.load(Ordering::Acquire);
    if core.is_null() {
        return;
    }
    let device_clock: DeviceClockFn = unsafe { std::mem::transmute(core) };
    unsafe { device_clock(c"cupti".as_ptr(), before, device as i64, after) };
}

extern "C" fn calibrate_at_exit() {
    calibrate();
}

/// CUDA injection entry point (`CUDA_INJECTION64_PATH`).
#[unsafe(no_mangle)]
pub extern "C" fn InitializeInjection() -> c_int {
    if std::env::var_os("MPERF_SESSION_DIR").is_none() {
        return 1;
    }
    let collector = std::env::var("MPERF_COLLECTOR_LIBRARY")
        .unwrap_or_else(|_| dynload::CORE_LIBRARY.to_string());
    let Ok(collector) = std::ffi::CString::new(collector) else {
        return 1;
    };
    let core = unsafe { dynload::load(&collector) };
    if core.is_null() {
        return 1;
    }
    let register = unsafe { dynload::symbol(core, c"mperf_trace_register") };
    let emit = unsafe { dynload::symbol(core, c"mperf_trace_emit") };
    let timestamp = unsafe { dynload::symbol(core, c"mperf_trace_timestamp") };
    if register.is_null() || emit.is_null() || timestamp.is_null() {
        return 1;
    }
    CORE_REGISTER.store(register, Ordering::Release);
    CORE_EMIT.store(emit, Ordering::Release);
    CORE_TIMESTAMP.store(timestamp, Ordering::Release);
    CORE_DEVICE_CLOCK.store(
        unsafe { dynload::symbol(core, c"mperf_trace_device_clock") },
        Ordering::Release,
    );

    #[cfg(windows)]
    let cupti = std::env::var("MPERF_CUPTI_LIBRARY")
        .ok()
        .and_then(|path| std::ffi::CString::new(path).ok())
        .map(|path| unsafe { dynload::load(&path) })
        .unwrap_or_else(|| unsafe { dynload::load(c"cupti64.dll") });
    #[cfg(unix)]
    let cupti = unsafe {
        let handle = dynload::load(c"libcupti.so");
        if handle.is_null() {
            dynload::load(c"libcupti.so.12")
        } else {
            handle
        }
    };
    if cupti.is_null() {
        return 1;
    }
    let subscribe = unsafe { dynload::symbol(cupti, c"cuptiSubscribe") };
    let enable_domain = unsafe { dynload::symbol(cupti, c"cuptiEnableDomain") };
    let get_timestamp = unsafe { dynload::symbol(cupti, c"cuptiGetTimestamp") };
    if subscribe.is_null() || enable_domain.is_null() {
        return 1;
    }
    CUPTI_TIMESTAMP.store(get_timestamp, Ordering::Release);
    calibrate();
    unsafe { libc::atexit(calibrate_at_exit) };

    let subscribe: SubscribeFn = unsafe { std::mem::transmute(subscribe) };
    let enable_domain: EnableDomainFn = unsafe { std::mem::transmute(enable_domain) };
    let mut subscriber = std::ptr::null_mut();
    if unsafe { subscribe(&mut subscriber, on_callback, std::ptr::null_mut()) } != 0 {
        return 1;
    }
    unsafe {
        enable_domain(1, subscriber, CB_DOMAIN_RUNTIME_API);
        enable_domain(1, subscriber, CB_DOMAIN_DRIVER_API);
    }
    1
}
