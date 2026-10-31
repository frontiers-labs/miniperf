//! Roofline instrumentation entry points called by the Clang plugin. These
//! keep their historical names and ABI but write through the trace core.

use std::ffi::c_char;

use crate::api::cstr;
use crate::session::{EventRecord, HandleData, TraceKind, collector, current_tid, timestamp_ns};

#[repr(C)]
pub struct LoopInfo {
    line: u32,
    filename: *const c_char,
    func_name: *const c_char,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct LoopStats {
    trip_count: u64,
    bytes_load: u64,
    bytes_store: u64,
    scalar_int_ops: u64,
    scalar_float_ops: u64,
    scalar_double_ops: u64,
    vector_int_ops: u64,
    vector_float_ops: u64,
    vector_double_ops: u64,
}

pub struct LoopHandle {
    handle: HandleData,
    instance: u64,
}

fn roofline_enabled() -> bool {
    std::env::var_os("MPERF_COLLECTOR_ROOFLINE_INSTRUMENTED").is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn mperf_roofline_internal_is_instrumented_profiling() -> i32 {
    if crate::session::thread_is_internal() {
        return 0;
    }
    let _guard = crate::session::InternalGuard::enter();
    (collector().is_some() && roofline_enabled()) as i32
}

/// # Safety
/// `info` must point to a valid LoopInfo with NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_roofline_internal_notify_loop_begin(
    info: *const LoopInfo,
) -> *mut LoopHandle {
    if crate::session::thread_is_internal() {
        return std::ptr::null_mut();
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return std::ptr::null_mut();
    };
    let Some(info) = (unsafe { info.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let function = cstr(info.func_name);
    let file = cstr(info.filename);
    let handle = collector.register_payload(function, function, file, info.line, 0, false);
    let instance = collector.next_instance();
    collector.record(
        EventRecord {
            timestamp: timestamp_ns(),
            event_id: handle.event_id,
            instance,
            parent: 0,
            flow: 0,
            value: 0,
            tid: current_tid(),
            kind: TraceKind::Begin as u8,
            nframes: 0,
            _pad: [0; 2],
        },
        &[],
    );
    Box::into_raw(Box::new(LoopHandle { handle, instance }))
}

/// # Safety
/// `handle` must come from `mperf_roofline_internal_notify_loop_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_roofline_internal_notify_loop_end(handle: *mut LoopHandle) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return;
    };
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    collector.record(
        EventRecord {
            timestamp: timestamp_ns(),
            event_id: handle.handle.event_id,
            instance: collector.next_instance(),
            parent: 0,
            flow: handle.instance,
            value: 0,
            tid: current_tid(),
            kind: TraceKind::End as u8,
            nframes: 0,
            _pad: [0; 2],
        },
        &[],
    );
}

/// # Safety
/// `handle` and `stats` must be valid pointers from this ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mperf_roofline_internal_notify_loop_stats(
    handle: *mut LoopHandle,
    stats: *const LoopStats,
) {
    if crate::session::thread_is_internal() {
        return;
    }
    let _guard = crate::session::InternalGuard::enter();
    let Some(collector) = collector() else {
        return;
    };
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let stats = unsafe { stats.as_ref().copied().unwrap_or_default() };
    let timestamp = timestamp_ns();
    let tid = current_tid();
    for (name, value) in [
        ("roofline_bytes_load", stats.bytes_load),
        ("roofline_bytes_store", stats.bytes_store),
        ("roofline_scalar_int_ops", stats.scalar_int_ops),
        ("roofline_scalar_float_ops", stats.scalar_float_ops),
        ("roofline_scalar_double_ops", stats.scalar_double_ops),
        ("roofline_vector_int_ops", stats.vector_int_ops),
        ("roofline_vector_float_ops", stats.vector_float_ops),
        ("roofline_vector_double_ops", stats.vector_double_ops),
    ] {
        collector.record(
            EventRecord {
                timestamp,
                event_id: collector.intern(name),
                instance: collector.next_instance(),
                parent: handle.instance,
                flow: 0,
                value: value as i64,
                tid,
                kind: TraceKind::Counter as u8,
                nframes: 0,
                _pad: [0; 2],
            },
            &[],
        );
    }
}
