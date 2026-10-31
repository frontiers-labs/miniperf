//! Root-process libc interposition. Lifetimes are complete observed operations;
//! the optional stack policy samples attribution only. Collector-owned and
//! fork-child calls pass through without entering inherited collector locks.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

const KIND_INSTANT: u8 = 2;
const FLAG_STACK: u32 = 1;

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

static CORE_STATE: AtomicI32 = AtomicI32::new(0); // 0 unresolved, 1 active, -1 disabled
static CORE_REGISTER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    static INSIDE: Cell<bool> = const { Cell::new(false) };
    static ALLOC_TICK: Cell<u64> = const { Cell::new(0) };
}

static ALLOCATION_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static STACK_EVERY: AtomicU64 = AtomicU64::new(0);
static FORK_CHILD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn fork_child() {
    FORK_CHILD.store(true, Ordering::Relaxed);
}

type AllocationFn = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64;
fn allocation_emit(
    kind: u64,
    operation_id: u64,
    old: u64,
    address: u64,
    size: u64,
    retires: bool,
) -> u64 {
    if inside() || FORK_CHILD.load(Ordering::Relaxed) || !core_resolve() {
        return 0;
    }
    let emit = ALLOCATION_EMIT.load(Ordering::Acquire);
    if emit.is_null() {
        return 0;
    }
    guarded(|| {
        let every = STACK_EVERY.load(Ordering::Relaxed);
        let stack = (kind == 1 || kind == 4)
            && address != 0
            && every != 0
            && ALLOC_TICK
                .try_with(|tick| {
                    let next = tick.get() + 1;
                    tick.set(next);
                    next % every == 0
                })
                .unwrap_or(false);
        let emit: AllocationFn = unsafe { std::mem::transmute(emit) };
        unsafe {
            emit(
                kind,
                operation_id,
                old,
                address,
                size,
                u64::from(retires),
                u64::from(stack),
            )
        }
    })
}

#[cfg(target_os = "linux")]
unsafe fn mmap_fallback(
    address: *mut c_void,
    length: usize,
    protection: c_int,
    flags: c_int,
    fd: c_int,
    offset: libc::off_t,
) -> *mut c_void {
    unsafe {
        libc::syscall(
            libc::SYS_mmap,
            address,
            length,
            protection,
            flags,
            fd,
            offset,
        ) as *mut c_void
    }
}

#[cfg(not(target_os = "linux"))]
unsafe fn mmap_fallback(
    _address: *mut c_void,
    _length: usize,
    _protection: c_int,
    _flags: c_int,
    _fd: c_int,
    _offset: libc::off_t,
) -> *mut c_void {
    libc::MAP_FAILED
}

#[cfg(target_os = "linux")]
unsafe fn munmap_fallback(address: *mut c_void, length: usize) -> c_int {
    unsafe { libc::syscall(libc::SYS_munmap, address, length) as c_int }
}

#[cfg(not(target_os = "linux"))]
unsafe fn munmap_fallback(_address: *mut c_void, _length: usize) -> c_int {
    -1
}

#[cfg(target_os = "macos")]
fn pthread_id_value(thread: libc::pthread_t) -> u64 {
    thread as u64
}

#[cfg(not(target_os = "macos"))]
fn pthread_id_value(thread: libc::pthread_t) -> u64 {
    thread
}

/// Bump arena serving allocations made while `dlsym` itself is resolving the
/// real allocator (glibc's dlsym may calloc). Never freed.
const ARENA_BYTES: usize = 64 * 1024;
static ARENA: [u8; ARENA_BYTES] = [0; ARENA_BYTES];
static ARENA_OFFSET: AtomicUsize = AtomicUsize::new(0);

fn arena_alloc(size: usize) -> *mut c_void {
    let size = (size.max(1) + 15) & !15;
    let offset = ARENA_OFFSET.fetch_add(size, Ordering::Relaxed);
    if offset + size > ARENA_BYTES {
        return std::ptr::null_mut();
    }
    ARENA.as_ptr().wrapping_add(offset) as *mut c_void
}

fn is_arena(ptr: *mut c_void) -> bool {
    let start = ARENA.as_ptr() as usize;
    (start..start + ARENA_BYTES).contains(&(ptr as usize))
}

fn inside() -> bool {
    INSIDE.try_with(|flag| flag.get()).unwrap_or(true)
}

fn guarded<T>(body: impl FnOnce() -> T) -> T {
    let previous = INSIDE.try_with(|flag| flag.replace(true)).unwrap_or(true);
    let result = body();
    let _ = INSIDE.try_with(|flag| flag.set(previous));
    result
}

fn resolve_next(name: &CStr, slot: &AtomicPtr<c_void>) -> *mut c_void {
    let cached = slot.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let symbol = guarded(|| unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) });
    if !symbol.is_null() {
        slot.store(symbol, Ordering::Release);
    }
    symbol
}

fn env_lookup(name: &CStr) -> Option<&'static CStr> {
    let value = unsafe { libc::getenv(name.as_ptr()) };
    if value.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(value) })
    }
}

fn core_resolve() -> bool {
    match CORE_STATE.load(Ordering::Acquire) {
        1 => return true,
        -1 => return false,
        _ => {}
    }
    guarded(|| {
        if env_lookup(c"MPERF_SESSION_DIR").is_none() {
            CORE_STATE.store(-1, Ordering::Release);
            return;
        }
        let library = env_lookup(c"MPERF_COLLECTOR_LIBRARY").unwrap_or(c"libmperf_collector.so");
        let core = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if core.is_null() {
            CORE_STATE.store(-1, Ordering::Release);
            return;
        }
        let register = unsafe { libc::dlsym(core, c"mperf_trace_register".as_ptr()) };
        let emit = unsafe { libc::dlsym(core, c"mperf_trace_emit".as_ptr()) };
        if register.is_null() || emit.is_null() {
            CORE_STATE.store(-1, Ordering::Release);
            return;
        }
        CORE_REGISTER.store(register, Ordering::Release);
        CORE_EMIT.store(emit, Ordering::Release);
        if env_lookup(c"MPERF_ALLOCATION_CHANNEL").is_some() {
            let root = env_lookup(c"MPERF_PROFILE_ROOT_PID")
                .and_then(|s| s.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok());
            if root.is_none_or(|root| root == unsafe { libc::getpid() } as u32) {
                let allocation = unsafe { libc::dlsym(core, c"mperf_trace_allocation".as_ptr()) };
                ALLOCATION_EMIT.store(allocation, Ordering::Release);
                let every = env_lookup(c"MPERF_ALLOCATION_STACK_EVERY")
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(16);
                STACK_EVERY.store(every, Ordering::Relaxed);
            }
        }
        unsafe {
            libc::pthread_atfork(None, None, Some(fork_child));
        }
        CORE_STATE.store(1, Ordering::Release);
    });
    CORE_STATE.load(Ordering::Acquire) == 1
}

fn register_payload(name: &CStr, flags: u32) -> *mut c_void {
    let register: RegisterFn =
        unsafe { std::mem::transmute(CORE_REGISTER.load(Ordering::Acquire)) };
    let payload = Payload {
        name: name.as_ptr(),
        function: c"".as_ptr(),
        file: c"libc".as_ptr(),
        line: 0,
        column: 0,
        flags,
    };
    unsafe { register(&payload) }
}

fn raw_emit(handle: *mut c_void, kind: u8, value: i64, flow: u64) {
    if handle.is_null() {
        return;
    }
    let emit: EmitFn = unsafe { std::mem::transmute(CORE_EMIT.load(Ordering::Acquire)) };
    unsafe { emit(handle, kind, value, 0, flow, 0) };
}

fn shim_emit(slot: &AtomicPtr<c_void>, name: &CStr, flags: u32, value: i64, flow: u64) {
    if inside() || FORK_CHILD.load(Ordering::Relaxed) || !core_resolve() {
        return;
    }
    guarded(|| {
        let mut handle = slot.load(Ordering::Acquire);
        if handle.is_null() {
            handle = register_payload(name, flags);
            if !handle.is_null() {
                slot.store(handle, Ordering::Release);
            }
        }
        raw_emit(handle, KIND_INSTANT, value, flow);
    });
}

macro_rules! payload_slot {
    () => {{
        static SLOT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
        &SLOT
    }};
}

#[unsafe(no_mangle)]
/// # Safety
/// The caller must satisfy the platform `malloc` contract.
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"malloc", &NEXT);
    if next.is_null() {
        return arena_alloc(size);
    }
    let next: unsafe extern "C" fn(usize) -> *mut c_void = unsafe { std::mem::transmute(next) };
    let pointer = guarded(|| unsafe { next(size) });
    if !pointer.is_null() && !inside() {
        allocation_emit(1, 0, 0, pointer as u64, size as u64, false);
    }
    pointer
}

#[unsafe(no_mangle)]
/// # Safety
/// The caller must satisfy the platform `calloc` contract.
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"calloc", &NEXT);
    if next.is_null() {
        return arena_alloc(count.saturating_mul(size));
    }
    let next: unsafe extern "C" fn(usize, usize) -> *mut c_void =
        unsafe { std::mem::transmute(next) };
    let pointer = guarded(|| unsafe { next(count, size) });
    let total = count.saturating_mul(size);
    if !pointer.is_null() && !inside() {
        allocation_emit(1, 0, 0, pointer as u64, total as u64, false);
    }
    pointer
}

#[unsafe(no_mangle)]
/// # Safety
/// `pointer` must be null or allocated by a compatible allocator.
pub unsafe extern "C" fn free(pointer: *mut c_void) {
    if pointer.is_null() || is_arena(pointer) {
        return;
    }
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"free", &NEXT);
    if next.is_null() {
        return;
    }
    allocation_emit(2, 0, pointer as u64, 0, 0, false);
    let next: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(next) };
    guarded(|| unsafe { next(pointer) });
}

#[unsafe(no_mangle)]
/// # Safety
/// `pointer` must be null or allocated by a compatible allocator.
pub unsafe extern "C" fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"realloc", &NEXT);
    if next.is_null() {
        return arena_alloc(size);
    }
    if is_arena(pointer) {
        let replacement = unsafe { malloc(size) };
        if !replacement.is_null() {
            let start = ARENA.as_ptr() as usize;
            let available = ARENA_BYTES - (pointer as usize - start);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pointer as *const u8,
                    replacement as *mut u8,
                    size.min(available),
                )
            };
        }
        return replacement;
    }
    let next: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void =
        unsafe { std::mem::transmute(next) };
    let operation = allocation_emit(3, 0, pointer as u64, 0, size as u64, false);
    let replacement = guarded(|| unsafe { next(pointer, size) });
    // glibc documents that realloc(p, 0) frees p and returns NULL.
    let retires =
        !pointer.is_null() && (!replacement.is_null() || (cfg!(target_env = "gnu") && size == 0));
    if operation != 0 {
        allocation_emit(
            4,
            operation,
            pointer as u64,
            replacement as u64,
            size as u64,
            retires,
        );
    }

    replacement
}

#[unsafe(no_mangle)]
/// # Safety
/// The caller must satisfy the platform `aligned_alloc` contract.
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"aligned_alloc", &NEXT);
    if next.is_null() {
        return arena_alloc(size);
    }
    let next: unsafe extern "C" fn(usize, usize) -> *mut c_void =
        unsafe { std::mem::transmute(next) };
    let pointer = guarded(|| unsafe { next(alignment, size) });
    if !pointer.is_null() && !inside() {
        allocation_emit(1, 0, 0, pointer as u64, size as u64, false);
    }
    pointer
}

#[unsafe(no_mangle)]
/// # Safety
/// `pointer` must be valid for writing and the allocation arguments must satisfy `posix_memalign`.
pub unsafe extern "C" fn posix_memalign(
    pointer: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"posix_memalign", &NEXT);
    if next.is_null() {
        return libc::ENOMEM;
    }
    let next: unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int =
        unsafe { std::mem::transmute(next) };
    let result = guarded(|| unsafe { next(pointer, alignment, size) });
    if result == 0 && !inside() {
        allocation_emit(1, 0, 0, unsafe { *pointer } as u64, size as u64, false);
    }
    result
}

#[unsafe(no_mangle)]
/// # Safety
/// The caller must satisfy the platform `mmap` contract.
pub unsafe extern "C" fn mmap(
    address: *mut c_void,
    length: usize,
    protection: c_int,
    flags: c_int,
    fd: c_int,
    offset: libc::off_t,
) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"mmap", &NEXT);
    if next.is_null() {
        return unsafe { mmap_fallback(address, length, protection, flags, fd, offset) };
    }
    let next: unsafe extern "C" fn(
        *mut c_void,
        usize,
        c_int,
        c_int,
        c_int,
        libc::off_t,
    ) -> *mut c_void = unsafe { std::mem::transmute(next) };
    let result = unsafe { next(address, length, protection, flags, fd, offset) };
    if result != libc::MAP_FAILED && flags & libc::MAP_ANONYMOUS != 0 && !inside() {
        shim_emit(
            payload_slot!(),
            c"mmap",
            FLAG_STACK,
            length as i64,
            result as u64,
        );
    }
    result
}

#[unsafe(no_mangle)]
/// # Safety
/// `address` and `length` must describe a mapping valid for `munmap`.
pub unsafe extern "C" fn munmap(address: *mut c_void, length: usize) -> c_int {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"munmap", &NEXT);
    if next.is_null() {
        return unsafe { munmap_fallback(address, length) };
    }
    let next: unsafe extern "C" fn(*mut c_void, usize) -> c_int =
        unsafe { std::mem::transmute(next) };
    let result = unsafe { next(address, length) };
    if result == 0 && !inside() {
        shim_emit(payload_slot!(), c"munmap", 0, length as i64, address as u64);
    }
    result
}

#[unsafe(no_mangle)]
/// # Safety
/// The caller must satisfy the platform `sbrk` contract.
pub unsafe extern "C" fn sbrk(increment: libc::intptr_t) -> *mut c_void {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"sbrk", &NEXT);
    if next.is_null() {
        return usize::MAX as *mut c_void;
    }
    let next: unsafe extern "C" fn(libc::intptr_t) -> *mut c_void =
        unsafe { std::mem::transmute(next) };
    let result = unsafe { next(increment) };
    if result as usize != usize::MAX && increment != 0 && !inside() {
        shim_emit(payload_slot!(), c"sbrk", 0, increment as i64, 0);
    }
    result
}

#[unsafe(no_mangle)]
/// # Safety
/// All pointers and the start routine must remain valid as required by `pthread_create`.
pub unsafe extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attributes: *const libc::pthread_attr_t,
    start: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    argument: *mut c_void,
) -> c_int {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"pthread_create", &NEXT);
    if next.is_null() {
        return libc::EAGAIN;
    }
    let next: unsafe extern "C" fn(
        *mut libc::pthread_t,
        *const libc::pthread_attr_t,
        unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        *mut c_void,
    ) -> c_int = unsafe { std::mem::transmute(next) };
    let result = if inside() {
        let wrapper = Box::into_raw(Box::new(InternalStart {
            routine: start,
            argument,
        }));
        let result = unsafe { next(thread, attributes, internal_thread_start, wrapper.cast()) };
        if result != 0 {
            unsafe {
                drop(Box::from_raw(wrapper));
            }
        }
        result
    } else {
        unsafe { next(thread, attributes, start, argument) }
    };
    if result == 0 && !inside() {
        shim_emit(
            payload_slot!(),
            c"pthread_create",
            FLAG_STACK,
            0,
            pthread_id_value(unsafe { *thread }),
        );
    }
    result
}

#[unsafe(no_mangle)]
/// # Safety
/// `value` must be null or valid for writing as required by `pthread_join`.
pub unsafe extern "C" fn pthread_join(thread: libc::pthread_t, value: *mut *mut c_void) -> c_int {
    static NEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    let next = resolve_next(c"pthread_join", &NEXT);
    if next.is_null() {
        return libc::EINVAL;
    }
    let next: unsafe extern "C" fn(libc::pthread_t, *mut *mut c_void) -> c_int =
        unsafe { std::mem::transmute(next) };
    let result = unsafe { next(thread, value) };
    if result == 0 && !inside() {
        shim_emit(
            payload_slot!(),
            c"pthread_join",
            0,
            0,
            pthread_id_value(thread),
        );
    }
    result
}

struct InternalStart {
    routine: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    argument: *mut c_void,
}
unsafe extern "C" fn internal_thread_start(argument: *mut c_void) -> *mut c_void {
    let _ = INSIDE.try_with(|flag| flag.set(true));
    let start = unsafe { Box::from_raw(argument.cast::<InternalStart>()) };
    let routine = start.routine;
    let argument = start.argument;
    drop(start);
    unsafe { routine(argument) }
}
