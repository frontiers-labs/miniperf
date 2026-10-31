use std::ffi::{CStr, c_char};

use crate::session::{EventRecord, HandleData, TraceKind, collector, current_tid, timestamp_ns};
use crate::stack::{self, MAX_FRAMES};

pub const MPERF_TRACE_FLAG_STACK: u32 = 1;

/// Trace-point identity passed to `mperf_trace_register`.
#[repr(C)]
pub struct MperfTracePayload {
    pub name: *const c_char,
    pub function: *const c_char,
    pub file: *const c_char,
    pub line: u32,
    pub column: u32,
    pub flags: u32,
}

pub(crate) fn cstr<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("")
}

/// # Safety
/// `payload` must point to a valid payload whose strings are NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_register(
    payload: *const MperfTracePayload,
) -> *mut HandleData {
    if crate::session::thread_is_internal() {
        return std::ptr::null_mut();
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return std::ptr::null_mut();
    };
    let Some(payload) = (unsafe { payload.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let handle = collector.register_payload(
        cstr(payload.name),
        cstr(payload.function),
        cstr(payload.file),
        payload.line,
        payload.column,
        payload.flags & MPERF_TRACE_FLAG_STACK != 0,
    );
    Box::into_raw(Box::new(handle))
}

fn emit(handle: *const HandleData, kind: TraceKind, instance: u64, parent: u64, value: i64) {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let Some(collector) = collector() else {
        return;
    };
    let record = EventRecord {
        timestamp: timestamp_ns(),
        event_id: handle.event_id,
        instance,
        parent,
        flow: 0,
        value,
        tid: current_tid(),
        kind: kind as u8,
        nframes: 0,
        _pad: [0; 2],
    };
    if handle.capture_stack && kind != TraceKind::End {
        let mut frames = [0u64; MAX_FRAMES];
        let count = stack::capture(&mut frames);
        collector.record(record, &frames[..count]);
    } else {
        collector.record(record, &[]);
    }
}

/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_begin(handle: *const HandleData, parent: u64) -> u64 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return 0;
    };
    let instance = collector.next_instance();
    emit(handle, TraceKind::Begin, instance, parent, 0);
    instance
}

/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_end(handle: *const HandleData, instance: u64) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return;
    };
    let record = EventRecord {
        timestamp: timestamp_ns(),
        event_id: unsafe { handle.as_ref() }.map_or(0, |handle| handle.event_id),
        instance: collector.next_instance(),
        parent: 0,
        flow: instance,
        value: 0,
        tid: current_tid(),
        kind: TraceKind::End as u8,
        nframes: 0,
        _pad: [0; 2],
    };
    collector.record(record, &[]);
}

/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_instant(handle: *const HandleData, value: i64) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return;
    };
    let instance = collector.next_instance();
    emit(handle, TraceKind::Instant, instance, 0, value);
}

/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_counter(handle: *const HandleData, value: i64) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return;
    };
    let instance = collector.next_instance();
    emit(handle, TraceKind::Counter, instance, 0, value);
}

/// General emit entry for proxies: any kind, with explicit parent/flow and a
/// per-call stack-capture override. Returns the new row's instance ID.
///
/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_emit(
    handle: *const HandleData,
    kind: u8,
    value: i64,
    parent: u64,
    flow: u64,
    capture_stack: i32,
) -> u64 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    let Some(collector) = collector() else {
        return 0;
    };
    let kind = match kind {
        0 => TraceKind::Begin,
        1 => TraceKind::End,
        2 => TraceKind::Instant,
        3 => TraceKind::Counter,
        _ => return 0,
    };
    let instance = collector.next_instance();
    let record = EventRecord {
        timestamp: timestamp_ns(),
        event_id: handle.event_id,
        instance,
        parent,
        flow,
        value,
        tid: current_tid(),
        kind: kind as u8,
        nframes: 0,
        _pad: [0; 2],
    };
    if capture_stack != 0 || (handle.capture_stack && kind != TraceKind::End) {
        let mut frames = [0u64; MAX_FRAMES];
        let count = stack::capture(&mut frames);
        collector.record(record, &frames[..count]);
    } else {
        collector.record(record, &[]);
    }
    instance
}

/// Like `mperf_trace_emit` but with an explicit CLOCK_MONOTONIC timestamp,
/// for proxies delivering activity records after the fact (CUPTI).
///
/// # Safety
/// `handle` must come from `mperf_trace_register` (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_emit_at(
    handle: *const HandleData,
    kind: u8,
    timestamp_ns: i64,
    value: i64,
    parent: u64,
    flow: u64,
) -> u64 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    let Some(collector) = collector() else {
        return 0;
    };
    let kind = match kind {
        0 => TraceKind::Begin,
        1 => TraceKind::End,
        2 => TraceKind::Instant,
        3 => TraceKind::Counter,
        _ => return 0,
    };
    let instance = collector.next_instance();
    collector.record(
        EventRecord {
            timestamp: timestamp_ns.max(0) as u64,
            event_id: handle.event_id,
            instance,
            parent,
            flow,
            value,
            tid: current_tid(),
            kind: kind as u8,
            nframes: 0,
            _pad: [0; 2],
        },
        &[],
    );
    instance
}

/// Record this process's rank (MPI or other launcher) for process metadata.
#[unsafe(no_mangle)]
pub extern "C" fn mperf_trace_set_rank(rank: i64) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    if let Some(collector) = collector() {
        collector.set_rank(rank);
    }
}

/// Record one cross-node clock-offset measurement against `peer`.
///
/// # Safety
/// `phase` must be a NUL-terminated string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_clock_sync(
    peer: u32,
    phase: *const c_char,
    local_ns: i64,
    peer_ns: i64,
    uncertainty_ns: i64,
) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    if let Some(collector) = collector() {
        collector.clock_sync(peer, cstr(phase), local_ns, peer_ns, uncertainty_ns);
    }
}

/// Record one bracketed device-clock calibration pair for `device`.
///
/// # Safety
/// `device` must be a NUL-terminated string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_trace_device_clock(
    device: *const c_char,
    host_before_ns: i64,
    device_ns: i64,
    host_after_ns: i64,
) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    if let Some(collector) = collector() {
        collector.device_clock_pair(cstr(device), host_before_ns, device_ns, host_after_ns);
    }
}

/// Current CLOCK_MONOTONIC in nanoseconds, for proxies doing clock exchange.
#[unsafe(no_mangle)]
pub extern "C" fn mperf_trace_timestamp() -> i64 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    timestamp_ns() as i64
}

/// Flush and close the session tables; safe to call more than once.
#[unsafe(no_mangle)]
pub extern "C" fn mperf_trace_shutdown() {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    crate::session::shutdown();
}

/// Record one allocator boundary. Kind values are defined by the shared allocation protocol.
#[unsafe(no_mangle)]
pub extern "C" fn mperf_trace_allocation(
    kind: u64,
    operation_id: u64,
    old_address: u64,
    address: u64,
    requested_bytes: u64,
    retires_old: u64,
    capture_stack: u64,
) -> u64 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return 0;
    };
    use shmem::allocation::{Allocation, Operation};
    let Ok(operation) = Operation::try_from(kind) else {
        return 0;
    };
    let mut record = Allocation {
        pid: 0,
        process_epoch: 0,
        sequence: 0,
        timestamp_ns: 0,
        tid: 0,
        operation,
        operation_id,
        old_address,
        address,
        requested_bytes,
        retires_old: retires_old != 0,
        stack_status: 0,
        frame_count: 0,
        frames: [0; shmem::allocation::MAX_FRAMES],
    };
    if capture_stack != 0 {
        let mut frames = [0; MAX_FRAMES];
        let capture = stack::capture_status(&mut frames);
        record.frame_count = capture.frames.min(shmem::allocation::MAX_FRAMES);
        record.frames[..record.frame_count].copy_from_slice(&frames[..record.frame_count]);
        record.stack_status = if record.frame_count == 0 {
            2
        } else if record.frame_count == shmem::allocation::MAX_FRAMES {
            3
        } else if !capture.exhausted {
            4
        } else {
            1
        };
    }
    collector.record_allocation(record)
}
