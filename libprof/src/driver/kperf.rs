#![allow(dead_code)]

use dlopen2::wrapper::{Container, WrapperApi};
use libc::{c_char, c_int, c_uint, c_void, size_t};
use smallvec::SmallVec;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::driver::{
    CounterEntry, CounterResult, CounterValue, CountingDriver, MeasurementQuality, SamplingDriver,
    Sink,
};
use crate::sink::{Record, Sample};
use crate::{Counter, Error};

/// Upper bound on counters the kernel can write per CPU (arm64 is 11, x86_64 is
/// 32). Used purely as buffer headroom — the kpc read paths do no length
/// checking, so a generous guard turns any miscount into wasted space instead of
/// heap corruption.
const KPC_MAX_COUNTERS: usize = 32;

const KPC_CLASS_FIXED: u32 = 0;
const KPC_CLASS_CONFIGURABLE: u32 = 1;

const KPC_CLASS_FIXED_MASK: u32 = 1 << KPC_CLASS_FIXED;
const KPC_CLASS_CONFIGURABLE_MASK: u32 = 1 << KPC_CLASS_CONFIGURABLE;

const DBG_PERF: u32 = 37;
const DBG_FUNC_START: u32 = 1;
const DBG_FUNC_END: u32 = 2;
const KDBG_EVENTID_MASK: u32 = 0xffff_fffc;

const PERF_SAMPLE: u32 = kdbg_eventid(DBG_PERF, 0, 0);
const PERF_TI_DATA: u32 = kdbg_eventid(DBG_PERF, 1, 1);
const PERF_STK_UHDR: u32 = 0x2502_0018;
const PERF_STK_UDATA: u32 = 0x2502_0010;
const PERF_KPC_DATA_THREAD: u32 = kdbg_eventid(DBG_PERF, 6, 8);

const SAMPLE_FLAG_PEND_USER: u64 = 1 << 0;
const SAMPLE_META_UPEND: u64 = 1 << 1;
const CALLSTACK_VALID: u64 = 1 << 0;
const CALLSTACK_HAS_ASYNC: u64 = 1 << 9;

const SAMPLER_TH_INFO: u32 = 1 << 0;
const SAMPLER_USTACK: u32 = 1 << 3;
const SAMPLER_PMC_THREAD: u32 = 1 << 4;

const CTL_KERN: c_int = 1;
const KERN_KDEBUG: c_int = 24;
const KERN_KDENABLE: c_int = 3;
const KERN_KDSETBUF: c_int = 4;
const KERN_KDSETUP: c_int = 6;
const KERN_KDREMOVE: c_int = 7;
const KERN_KDREADTR: c_int = 10;
const KERN_KDSET_TYPEFILTER: c_int = 22;

const KDEBUG_ENABLE_TRACE: c_uint = 1;
const KDBG_TYPEFILTER_BITMAP_SIZE: usize = (256 * 256) / 8;
const KDBG_BYTES_PER_CLASS: usize = 256 / 8;

const RUSAGE_INFO_V4: c_int = 4;

const fn kdbg_eventid(class: u32, subclass: u32, code: u32) -> u32 {
    ((class & 0xff) << 24) | ((subclass & 0xff) << 16) | ((code & 0x3fff) << 2)
}

#[repr(C)]
struct KPepEvent {
    name: *const c_char,
    description: *const c_char,
    errata: *const c_char,
    alias: *const c_char,
    fallback: *const c_char,
    mask: u32,
    number: u8,
    umask: u8,
    reserved: u8,
    is_fixed: u8,
}

#[repr(C)]
struct KPepDB {
    _private: [u8; 0],
}

#[repr(C)]
struct KPepConfig {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KdBuf {
    timestamp: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    debugid: u32,
    cpuid: u32,
    unused: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RUsageInfoV4 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
    ri_cpu_time_qos_default: u64,
    ri_cpu_time_qos_maintenance: u64,
    ri_cpu_time_qos_background: u64,
    ri_cpu_time_qos_utility: u64,
    ri_cpu_time_qos_legacy: u64,
    ri_cpu_time_qos_user_initiated: u64,
    ri_cpu_time_qos_user_interactive: u64,
    ri_billed_system_time: u64,
    ri_serviced_system_time: u64,
    ri_logical_writes: u64,
    ri_lifetime_max_phys_footprint: u64,
    ri_instructions: u64,
    ri_cycles: u64,
    ri_billed_energy: u64,
    ri_serviced_energy: u64,
    ri_interval_max_phys_footprint: u64,
    ri_runnable_time: u64,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

#[derive(WrapperApi)]
struct KPCDispatch {
    kpc_cpu_string: unsafe extern "C" fn(buf: *mut c_char, buf_size: size_t) -> c_int,
    kpc_pmu_version: unsafe extern "C" fn() -> u32,
    kpc_get_counting: unsafe extern "C" fn() -> u32,
    kpc_set_counting: unsafe extern "C" fn(classes: u32) -> c_int,
    kpc_set_thread_counting: unsafe extern "C" fn(classes: u32) -> c_int,
    kpc_get_config_count: unsafe extern "C" fn(classes: u32) -> u32,
    kpc_get_counter_count: unsafe extern "C" fn(classes: u32) -> u32,
    kpc_set_config: unsafe extern "C" fn(classes: u32, config: *mut u64) -> c_int,
    kpc_set_period: unsafe extern "C" fn(classes: u32, period: *mut u64) -> c_int,
    kpc_get_cpu_counters: unsafe extern "C" fn(
        all_cpus: c_int,
        classes: u32,
        curcpu: *mut c_int,
        buf: *mut u64,
    ) -> c_int,
    kpc_get_thread_counters: unsafe extern "C" fn(tid: u32, buf_count: u32, buf: *mut u64) -> c_int,
    kpc_force_all_ctrs_set: unsafe extern "C" fn(val: c_int) -> c_int,
}

#[derive(WrapperApi)]
struct KPerfDispatch {
    kperf_reset: unsafe extern "C" fn(),
    kperf_ns_to_ticks: unsafe extern "C" fn(ns: u64) -> u64,
    kperf_action_count_set: unsafe extern "C" fn(count: c_uint) -> c_int,
    kperf_action_samplers_set: unsafe extern "C" fn(actionid: c_uint, samplers: u32) -> c_int,
    kperf_action_ucallstack_depth_set: unsafe extern "C" fn(actionid: c_uint, depth: u32) -> c_int,
    kperf_action_filter_set_by_pid: unsafe extern "C" fn(actionid: c_uint, pid: c_int) -> c_int,
    kperf_timer_count_set: unsafe extern "C" fn(count: c_uint) -> c_int,
    kperf_timer_period_set: unsafe extern "C" fn(timerid: c_uint, ticks: u64) -> c_int,
    kperf_timer_action_set: unsafe extern "C" fn(timerid: c_uint, actionid: c_uint) -> c_int,
    kperf_sample_set: unsafe extern "C" fn(enabled: c_int) -> c_int,
}

#[derive(WrapperApi)]
struct KPEPDispatch {
    kpep_config_create: unsafe extern "C" fn(db: *mut KPepDB, cfg: *mut *mut KPepConfig) -> c_int,
    kpep_config_free: unsafe extern "C" fn(cfg: *mut KPepConfig),
    kpep_db_create: unsafe extern "C" fn(name: *const c_char, db: *mut *mut KPepDB) -> c_int,
    kpep_db_free: unsafe extern "C" fn(db: *mut KPepDB),
    kpep_db_event: unsafe extern "C" fn(
        db: *const KPepDB,
        name: *const c_char,
        event: *mut *mut KPepEvent,
    ) -> c_int,
    kpep_db_events_count: unsafe extern "C" fn(db: *const KPepDB, count: *mut size_t) -> c_int,
    kpep_db_events:
        unsafe extern "C" fn(db: *const KPepDB, buf: *mut *mut KPepEvent, buf_size: usize) -> c_int,
    kpep_event_name: unsafe extern "C" fn(event: *const KPepEvent, name: *mut *const c_char),
    kpep_event_description: unsafe extern "C" fn(event: *const KPepEvent, desc: *mut *const c_char),
    kpep_config_force_counters: unsafe extern "C" fn(cfg: *mut KPepConfig) -> c_int,
    kpep_config_add_event: unsafe extern "C" fn(
        cfg: *mut KPepConfig,
        evt: *mut *mut KPepEvent,
        flag: u32,
        err: *mut u32,
    ) -> c_int,
    kpep_config_kpc:
        unsafe extern "C" fn(cfg: *mut KPepConfig, buf: *mut u64, buf_size: usize) -> c_int,
    kpep_config_kpc_periods:
        unsafe extern "C" fn(cfg: *mut KPepConfig, buf: *mut u64, buf_size: usize) -> c_int,
    kpep_config_kpc_count:
        unsafe extern "C" fn(cfg: *mut KPepConfig, count_ptr: *mut usize) -> c_int,
    kpep_config_kpc_classes:
        unsafe extern "C" fn(cfg: *mut KPepConfig, classes_ptr: *mut u32) -> c_int,
    kpep_config_kpc_map:
        unsafe extern "C" fn(cfg: *mut KPepConfig, buf: *mut usize, size: usize) -> c_int,
}

#[derive(Clone)]
struct KPerfContext {
    kpc: Arc<Container<KPCDispatch>>,
    kperf: Arc<Container<KPerfDispatch>>,
    kpep: Arc<Container<KPEPDispatch>>,
    db: *mut KPepDB,
}

unsafe impl Send for KPerfContext {}
unsafe impl Sync for KPerfContext {}

pub struct KPerfCountingDriver {
    ctx: KPerfContext,
    handles: Vec<NativeCounterHandle>,
    programs: Vec<KpcProgram>,
    classes: u32,
    kpc_count: usize,
    cpu_count: usize,
    /// Whether privileged kpc hardware counting is usable. When false (e.g. no
    /// root / KPC access denied) we still report whatever `proc_pid_rusage`
    /// provides (cycles, instructions, page faults, cpu-clock) rather than
    /// failing outright — "some data rather than none".
    hw_available: bool,
    before: Option<CounterSnapshot>,
    after: Option<CounterSnapshot>,
    pid: Option<i32>,
    multiplex_counts: Option<Arc<Mutex<MultiplexCounts>>>,
    multiplex_running: Arc<AtomicBool>,
    multiplex_thread: Option<thread::JoinHandle<()>>,
    taskinfo_latest: Arc<Mutex<Option<libc::proc_taskinfo>>>,
    taskinfo_running: Arc<AtomicBool>,
    taskinfo_thread: Option<thread::JoinHandle<()>>,
    pid_sampler: Option<KPerfSamplingDriver>,
    estimated_counts: Arc<Mutex<Vec<EstimatedCounterValue>>>,
}

pub struct KPerfSamplingDriver {
    ctx: KPerfContext,
    handles: Vec<NativeCounterHandle>,
    programs: Vec<KpcProgram>,
    classes: u32,
    kpc_count: usize,
    sample_freq: u64,
    pid: Option<i32>,
    capture_callstacks: bool,
    running: Arc<AtomicBool>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
struct NativeCounterHandle {
    counter: Counter,
    program_index: Option<usize>,
    kpc_index: Option<usize>,
}

#[derive(Clone)]
struct KpcProgram {
    handles: Vec<NativeCounterHandle>,
    classes: u32,
    kpc_count: usize,
    config: Vec<u64>,
    periods: Vec<u64>,
}

#[derive(Default)]
struct MultiplexCounts {
    counts: Vec<(Counter, u64)>,
    running_ns: Vec<(Counter, u64)>,
    started_at: Option<Instant>,
    stopped_at: Option<Instant>,
}

/// A read-time view of the multiplexer's accumulators. `value()` scales a
/// counter's raw total by (total wall time / time this counter was actually
/// scheduled), the same estimate `perf` reports for multiplexed events.
struct MultiplexView {
    enabled_ns: u64,
    counts: Vec<(Counter, u64)>,
    running_ns: Vec<(Counter, u64)>,
}

#[derive(Clone)]
struct EstimatedCounterValue {
    counter: Counter,
    value: u64,
    time_enabled: u64,
    time_running: u64,
    samples: u64,
}

impl MultiplexView {
    fn value(&self, counter: &Counter) -> Option<(u64, f64)> {
        let raw = find_counter_value(&self.counts, counter)?;
        let running_ns = find_counter_value(&self.running_ns, counter)?;
        if running_ns == 0 || self.enabled_ns == 0 {
            return None;
        }
        let scaling = self.enabled_ns as f64 / running_ns as f64;
        Some(((raw as f64 * scaling).round() as u64, scaling))
    }
}

#[derive(Clone, Default)]
struct CounterSnapshot {
    kpc: Vec<u64>,
    rusage: Option<RUsageInfoV4>,
    taskinfo: Option<libc::proc_taskinfo>,
    taskinfo_exact: bool,
}

struct StackBuilder {
    frames_left: usize,
    flags: u64,
    synchronous_frames: usize,
    async_start_index: usize,
    async_frames: usize,
    frames: SmallVec<[u64; 32]>,
}

impl StackBuilder {
    fn finish(self) -> SmallVec<[u64; 32]> {
        if self.flags & CALLSTACK_VALID == 0 {
            return SmallVec::new();
        }

        let synchronous_len = self.synchronous_frames.min(self.frames.len());
        let async_len = self
            .async_frames
            .min(self.frames.len().saturating_sub(synchronous_len));
        let mut synchronous = SmallVec::<[u64; 32]>::from_slice(&self.frames[..synchronous_len]);
        let async_frames = &self.frames[synchronous_len..synchronous_len + async_len];

        // backtrace_user() records the interrupted PC followed by the frame
        // pointer chain. kperf then appends the saved LR (arm64) or stack-top
        // return address (x86_64) as the final synchronous entry so consumers
        // can repair leaf functions that have not established a frame yet.
        // Put that fixup immediately after the sampled PC to reconstruct the
        // logical leaf-to-root call chain. Async frames, when present, follow
        // the synchronous portion and must not be mistaken for the fixup.
        let fixup = (synchronous.len() > 1).then(|| synchronous.pop()).flatten();

        // Swift async frames replace the physical-stack suffix at the index
        // reported by XNU. They are not an independent rootward suffix.
        if self.flags & CALLSTACK_HAS_ASYNC != 0 && !async_frames.is_empty() {
            synchronous.truncate(self.async_start_index.min(synchronous.len()));
        }

        if let Some(fixup) = fixup {
            if fixup != 0 && synchronous.get(1).copied() != Some(fixup) {
                synchronous.insert(1.min(synchronous.len()), fixup);
            }
        }

        if self.flags & CALLSTACK_HAS_ASYNC != 0 {
            synchronous.extend(async_frames.iter().copied());
        }
        synchronous.retain(|frame| *frame != 0);
        synchronous
    }
}

#[derive(Clone, Default)]
struct PendingSample {
    time: u64,
    pid: u32,
    tid: u32,
    trace_tid: u64,
    cpu: u32,
    kpc_values: Vec<u64>,
    callstack: SmallVec<[u64; 32]>,
    forced_pid: Option<u32>,
    timer_observation: bool,
}

struct DeferredSample {
    sample: PendingSample,
    handles: Vec<NativeCounterHandle>,
    program_epoch: u64,
    time_enabled_multiplier: u64,
}

#[derive(Default)]
struct KdebugDecodeState {
    pending_stacks: HashMap<u64, StackBuilder>,
    pending_samples: HashMap<u64, PendingSample>,
    deferred_samples: HashMap<u64, VecDeque<DeferredSample>>,
    last_kpc_values: HashMap<(u32, u32, u64, usize), LastKpcValue>,
    sample_period_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct LastKpcValue {
    time: u64,
    value: u64,
}

impl KPerfContext {
    fn new() -> Result<Self, Error> {
        let kpc: Container<KPCDispatch> = unsafe {
            Container::load("/System/Library/PrivateFrameworks/kperf.framework/kperf")
                .map_err(|_| Error::CounterCreationFail)?
        };
        let kperf: Container<KPerfDispatch> = unsafe {
            Container::load("/System/Library/PrivateFrameworks/kperf.framework/kperf")
                .map_err(|_| Error::CounterCreationFail)?
        };
        let kpep: Container<KPEPDispatch> = unsafe {
            Container::load("/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata")
                .map_err(|_| Error::CounterCreationFail)?
        };

        let kpc: Arc<Container<KPCDispatch>> = kpc.into();
        let kperf: Arc<Container<KPerfDispatch>> = kperf.into();
        let kpep: Arc<Container<KPEPDispatch>> = kpep.into();
        let db = create_kpep_db(&kpc, &kpep)?;

        Ok(KPerfContext {
            kpc,
            kperf,
            kpep,
            db,
        })
    }
}

fn create_kpep_db(
    kpc: &Container<KPCDispatch>,
    kpep: &Container<KPEPDispatch>,
) -> Result<*mut KPepDB, Error> {
    let mut names = Vec::<String>::new();
    let mut cpu = [0_i8; 128];

    if unsafe { kpc.kpc_cpu_string(cpu.as_mut_ptr(), cpu.len()) } == 0 {
        if let Ok(name) = unsafe { CStr::from_ptr(cpu.as_ptr()) }.to_str() {
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }

    names.extend(
        [
            "as5-2",
            "as5-1",
            "as5",
            "as4-2",
            "as4-1",
            "as4",
            "as3",
            "as2",
            "as1",
            "a16",
            "a15",
            "a14",
            "a13",
            "a12",
            "a11",
            "a10",
            "a9",
            "a8",
            "a7",
            "icelake",
            "cometlake",
            "kabylake",
            "skylake",
            "broadwell",
            "haswell",
            "ivybridge",
        ]
        .into_iter()
        .map(str::to_string),
    );

    let mut db: *mut KPepDB = ptr::null_mut();
    if unsafe { kpep.kpep_db_create(ptr::null(), &mut db) } == 0 && !db.is_null() {
        return Ok(db);
    }

    for name in names {
        let Ok(c_name) = CString::new(name) else {
            continue;
        };
        db = ptr::null_mut();
        if unsafe { kpep.kpep_db_create(c_name.as_ptr(), &mut db) } == 0 && !db.is_null() {
            return Ok(db);
        }
    }

    Err(Error::CounterCreationFail)
}

impl Drop for KPerfContext {
    fn drop(&mut self) {
        if Arc::strong_count(&self.kpep) == 1 && !self.db.is_null() {
            unsafe { self.kpep.kpep_db_free(self.db) };
            self.db = ptr::null_mut();
        }
    }
}

impl KPerfCountingDriver {
    pub fn new(counters: Vec<Counter>, pid: Option<i32>) -> Result<Self, Error> {
        let ctx = KPerfContext::new()?;
        // Build the kpc program layout but do not apply it yet: applying requires
        // privileged access, and we want construction to succeed even without root
        // so the rusage-based counters (cycles/instructions/...) still work.
        let config = build_kpc_config(&ctx, &counters, 0, false)?;
        let cpu_count = logical_cpu_count().unwrap_or(1);

        Ok(KPerfCountingDriver {
            ctx,
            handles: config.handles,
            programs: config.programs,
            classes: config.classes,
            kpc_count: config.kpc_count,
            cpu_count,
            hw_available: false,
            before: None,
            after: None,
            pid,
            multiplex_counts: None,
            multiplex_running: Arc::new(AtomicBool::new(false)),
            multiplex_thread: None,
            taskinfo_latest: Arc::new(Mutex::new(None)),
            taskinfo_running: Arc::new(AtomicBool::new(false)),
            taskinfo_thread: None,
            pid_sampler: None,
            estimated_counts: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn read_snapshot(&self) -> Result<CounterSnapshot, std::io::Error> {
        let mut snapshot = CounterSnapshot::default();
        if self.hw_available && self.kpc_count > 0 {
            // kpc_get_cpu_counters(all_cpus=1) writes counter_count u64 values per
            // CPU, indexed by physical cpu_number(), so the buffer must be sized for
            // logical_cpu_max, not the number of online CPUs. It returns 0 on
            // success; a nonzero return means the read failed, in which case the
            // zero-initialised buffer yields a zero delta (harmless) rather than an
            // error that would discard the whole snapshot. The extra KPC_MAX_COUNTERS
            // of headroom is pure defence: the kernel only asserts on the buffer
            // pointer (no length check), so any miscount would otherwise corrupt the
            // heap. Reads still use `kpc_count` as the stride, so the guard slots
            // stay zero and never affect the totals.
            let mut kpc = vec![0_u64; self.cpu_count * self.kpc_count + KPC_MAX_COUNTERS];
            unsafe {
                self.ctx.kpc.kpc_get_cpu_counters(
                    1,
                    self.classes,
                    ptr::null_mut(),
                    kpc.as_mut_ptr(),
                );
            }
            kpc.truncate(self.cpu_count * self.kpc_count);
            snapshot.kpc = kpc;
        }

        if let Some(pid) = self.pid {
            snapshot.rusage = read_rusage(pid).ok();
            snapshot.taskinfo = read_taskinfo(pid).ok();
            snapshot.taskinfo_exact = snapshot.taskinfo.is_some();
        }

        Ok(snapshot)
    }
}

impl CountingDriver for KPerfCountingDriver {
    fn start(&mut self) -> Result<(), Error> {
        if self.pid.is_some() {
            self.before = self.read_snapshot().ok();
            self.start_taskinfo_monitor();
            self.start_pid_estimator();
            return Ok(());
        }

        // Multiplex only when the configurable events don't fit in one program.
        // Multiplexing reads the same system-wide kpc counters, so it applies
        // whether or not a specific pid was requested.
        if self.programs.len() > 1 {
            self.start_multiplexer()?;
            self.start_taskinfo_monitor();
            return Ok(());
        }

        // Enable system-wide hardware counting. This is privileged; if it fails
        // (no root / KPC access denied / another client owns the PMU) we keep
        // going with hardware unavailable and fall back to rusage-derived
        // counters, rather than aborting the whole run.
        self.hw_available = self.enable_hw_counting();
        self.before = self.read_snapshot().ok();
        self.start_taskinfo_monitor();
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        if self.pid.is_some() {
            self.stop_pid_estimator();
            self.capture_final_process_snapshot();
            return Ok(());
        }

        if self.multiplex_thread.is_some() {
            return self.stop_multiplexer();
        }

        self.capture_final_process_snapshot();
        if self.hw_available {
            unsafe {
                self.ctx.kpc.kpc_set_counting(0);
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Error> {
        if let Some(multiplex_counts) = &self.multiplex_counts {
            if let Ok(mut counts) = multiplex_counts.lock() {
                *counts = MultiplexCounts::default();
            }
        }
        self.before = self.read_snapshot().ok();
        self.after = None;
        Ok(())
    }

    fn counters(&mut self) -> Result<CounterResult, std::io::Error> {
        // Snapshot (single-program) reads need an `after`; multiplexing keeps its
        // own accumulator and reads no snapshot here.
        let multiplexing = self.multiplex_counts.is_some();
        if !multiplexing && self.after.is_none() {
            self.after = Some(self.read_snapshot()?);
        }
        let before = self.before.clone().unwrap_or_default();
        let after = self.after.clone().unwrap_or_default();
        let mux = self.multiplex_view();

        let mut entries = SmallVec::<[CounterEntry; 16]>::with_capacity(self.handles.len());
        for handle in &self.handles {
            // Prefer the accurate per-process rusage value for the counters it
            // provides (cycles, instructions, page faults, cpu-clock). This holds
            // whether or not we are multiplexing the configurable PMU events.
            if self.pid.is_some() {
                if let Some((value, quality)) = process_counter(&handle.counter, &before, &after) {
                    entries.push(CounterEntry {
                        core: None,
                        counter: handle.counter.clone(),
                        value: CounterValue {
                            value,
                            scaling: 1.0,
                            quality,
                        },
                    });
                    continue;
                }
                if let Some((value, scaling)) = self.estimated_counter(&handle.counter) {
                    entries.push(CounterEntry {
                        core: None,
                        counter: handle.counter.clone(),
                        value: CounterValue {
                            value,
                            scaling,
                            quality: MeasurementQuality::Estimated,
                        },
                    });
                }
                // kpc_get_cpu_counters is system-wide. Never present that value
                // as if it belonged to the requested PID; programmable events
                // require the PID-filtered PMC_THREAD estimator.
                continue;
            }

            // Otherwise the hardware value: either the userspace-multiplexed and
            // scaled estimate, or the system-wide kpc snapshot delta.
            let (value, scaling) = match &mux {
                Some(mux) => {
                    let Some(value) = mux.value(&handle.counter) else {
                        continue;
                    };
                    value
                }
                None => {
                    if !self.hw_available {
                        continue;
                    }
                    let Some(idx) = handle.kpc_index else {
                        continue;
                    };
                    (
                        sum_counter_delta(&before.kpc, &after.kpc, self.kpc_count, idx),
                        1.0,
                    )
                }
            };

            entries.push(CounterEntry {
                core: None,
                counter: handle.counter.clone(),
                value: CounterValue {
                    value,
                    scaling,
                    quality: if mux.is_some() {
                        MeasurementQuality::Scaled
                    } else {
                        MeasurementQuality::Exact
                    },
                },
            });
        }

        Ok(CounterResult::from_entries(entries))
    }
}

/// Exact process-scoped values exposed by Darwin's recount/proc accounting.
/// Programmable PMU events are intentionally absent here: KPC's direct read is
/// system-wide and its userspace thread getter can only read the caller.
fn process_counter(
    counter: &Counter,
    before: &CounterSnapshot,
    after: &CounterSnapshot,
) -> Option<(u64, MeasurementQuality)> {
    match counter {
        Counter::Cycles => Some((
            after
                .rusage
                .as_ref()?
                .ri_cycles
                .saturating_sub(before.rusage.as_ref()?.ri_cycles),
            MeasurementQuality::Exact,
        )),
        Counter::Instructions => Some((
            after
                .rusage
                .as_ref()?
                .ri_instructions
                .saturating_sub(before.rusage.as_ref()?.ri_instructions),
            MeasurementQuality::Exact,
        )),
        Counter::PageFaults => Some((
            nonnegative_i32(after.taskinfo.as_ref()?.pti_faults)
                .saturating_sub(nonnegative_i32(before.taskinfo.as_ref()?.pti_faults)),
            taskinfo_quality(after),
        )),
        Counter::ContextSwitches => Some((
            nonnegative_i32(after.taskinfo.as_ref()?.pti_csw)
                .saturating_sub(nonnegative_i32(before.taskinfo.as_ref()?.pti_csw)),
            taskinfo_quality(after),
        )),
        Counter::CpuClock => Some((
            mach_ticks_to_nanos(
                after
                    .rusage
                    .as_ref()?
                    .ri_user_time
                    .saturating_add(after.rusage.as_ref()?.ri_system_time)
                    .saturating_sub(
                        before
                            .rusage
                            .as_ref()?
                            .ri_user_time
                            .saturating_add(before.rusage.as_ref()?.ri_system_time),
                    ),
            ),
            MeasurementQuality::Exact,
        )),
        _ => None,
    }
}

fn taskinfo_quality(snapshot: &CounterSnapshot) -> MeasurementQuality {
    if snapshot.taskinfo_exact {
        MeasurementQuality::Exact
    } else {
        MeasurementQuality::Estimated
    }
}

fn nonnegative_i32(value: i32) -> u64 {
    value.max(0) as u64
}

impl KPerfCountingDriver {
    fn start_multiplexer(&mut self) -> Result<(), Error> {
        // Best-effort: without privileges the per-segment reads simply yield
        // zeros instead of aborting the run.
        self.hw_available = unsafe { self.ctx.kpc.kpc_force_all_ctrs_set(1) } == 0;

        let counts = Arc::new(Mutex::new(MultiplexCounts {
            started_at: Some(Instant::now()),
            ..MultiplexCounts::default()
        }));
        self.multiplex_running.store(true, Ordering::SeqCst);

        let ctx = self.ctx.clone();
        let programs = self.programs.clone();
        let cpu_count = self.cpu_count;
        let counts_for_thread = counts.clone();
        let running = self.multiplex_running.clone();

        let handle = thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                for program in &programs {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }

                    if apply_kpc_program(&ctx, program).is_err() {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    unsafe {
                        ctx.kpc.kpc_set_counting(program.classes);
                    }

                    let before = read_program_kpc(&ctx, program, cpu_count).unwrap_or_default();
                    let segment_start = Instant::now();
                    let quantum = Duration::from_millis(50);
                    while running.load(Ordering::SeqCst) && segment_start.elapsed() < quantum {
                        thread::sleep(Duration::from_millis(2));
                    }
                    let elapsed_ns =
                        segment_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    let after = read_program_kpc(&ctx, program, cpu_count).unwrap_or_default();

                    unsafe {
                        ctx.kpc.kpc_set_counting(0);
                    }

                    if let Ok(mut counts) = counts_for_thread.lock() {
                        for handle in &program.handles {
                            let Some(idx) = handle.kpc_index else {
                                continue;
                            };
                            let delta = sum_counter_delta(&before, &after, program.kpc_count, idx);
                            add_counter_value(&mut counts.counts, handle.counter.clone(), delta);
                            add_counter_value(
                                &mut counts.running_ns,
                                handle.counter.clone(),
                                elapsed_ns,
                            );
                        }
                    }
                }
            }

            if let Ok(mut counts) = counts_for_thread.lock() {
                counts.stopped_at = Some(Instant::now());
            }
            unsafe {
                ctx.kpc.kpc_set_counting(0);
            }
        });

        self.multiplex_counts = Some(counts);
        self.multiplex_thread = Some(handle);
        Ok(())
    }

    fn stop_multiplexer(&mut self) -> Result<(), Error> {
        self.multiplex_running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.multiplex_thread.take() {
            handle.join().map_err(|_| Error::EnableFailed)?;
        }
        // Capture the child's final rusage while it is still a zombie, so the
        // rusage overlay (cycles/instructions/...) works in multiplex mode too.
        self.capture_final_process_snapshot();
        Ok(())
    }

    fn start_taskinfo_monitor(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        if self.taskinfo_thread.is_some() {
            return;
        }

        if let Ok(mut latest) = self.taskinfo_latest.lock() {
            *latest = None;
        }
        self.taskinfo_running.store(true, Ordering::SeqCst);
        let running = self.taskinfo_running.clone();
        let latest = self.taskinfo_latest.clone();
        self.taskinfo_thread = Some(thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if let Ok(info) = read_taskinfo(pid) {
                    if let Ok(mut slot) = latest.lock() {
                        *slot = Some(info);
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
        }));
    }

    fn capture_final_process_snapshot(&mut self) {
        self.after = self.read_snapshot().ok();
        self.taskinfo_running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.taskinfo_thread.take() {
            let _ = handle.join();
        }

        let fallback = self.taskinfo_latest.lock().ok().and_then(|latest| *latest);
        let Some(after) = self.after.as_mut() else {
            return;
        };
        if after.taskinfo.is_none() {
            after.taskinfo = fallback;
            after.taskinfo_exact = false;
        }
    }

    fn start_pid_estimator(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        let counters: Vec<Counter> = self
            .handles
            .iter()
            .filter(|handle| handle.kpc_index.is_some())
            .map(|handle| handle.counter.clone())
            .collect();
        if counters.is_empty() {
            return;
        }

        if let Ok(mut counts) = self.estimated_counts.lock() {
            counts.clear();
        }
        let Ok(mut sampler) =
            KPerfSamplingDriver::new_with_callstacks(&counters, 200, Some(pid), false)
        else {
            return;
        };

        let counts = self.estimated_counts.clone();
        let callback: Arc<dyn Sink> = Arc::new(move |record| {
            let Record::Sample(sample) = record else {
                return;
            };
            let Ok(mut values) = counts.lock() else {
                return;
            };
            if let Some(value) = values
                .iter_mut()
                .find(|value| value.counter == sample.counter)
            {
                value.value = value.value.saturating_add(sample.value);
                value.time_enabled = value.time_enabled.saturating_add(sample.time_enabled);
                value.time_running = value.time_running.saturating_add(sample.time_running);
                value.samples = value.samples.saturating_add(1);
            } else {
                values.push(EstimatedCounterValue {
                    counter: sample.counter,
                    value: sample.value,
                    time_enabled: sample.time_enabled,
                    time_running: sample.time_running,
                    samples: 1,
                });
            }
        });

        if sampler.start(callback).is_ok() {
            self.pid_sampler = Some(sampler);
        }
    }

    fn stop_pid_estimator(&mut self) {
        if let Some(mut sampler) = self.pid_sampler.take() {
            let _ = sampler.stop();
        }
    }

    fn estimated_counter(&self, counter: &Counter) -> Option<(u64, f64)> {
        let values = self.estimated_counts.lock().ok()?;
        let value = values
            .iter()
            .find(|value| &value.counter == counter && value.samples > 1)?;
        if value.time_running == 0 || value.time_enabled == 0 {
            return None;
        }
        let scaling = value.time_enabled as f64 / value.time_running as f64;
        Some(((value.value as f64 * scaling).round() as u64, scaling))
    }

    /// Snapshot of the userspace multiplexer's accumulators, or `None` when not
    /// multiplexing. Lets `counters()` treat multiplexed and single-program
    /// hardware values uniformly (with the rusage overlay layered on top).
    fn multiplex_view(&self) -> Option<MultiplexView> {
        let counts = self.multiplex_counts.as_ref()?.lock().ok()?;
        let started_at = counts.started_at?;
        let stopped_at = counts.stopped_at.unwrap_or_else(Instant::now);
        let enabled_ns = stopped_at
            .saturating_duration_since(started_at)
            .as_nanos()
            .min(u64::MAX as u128) as u64;

        Some(MultiplexView {
            enabled_ns,
            counts: counts.counts.clone(),
            running_ns: counts.running_ns.clone(),
        })
    }

    /// Apply the (single) kpc program and switch on system-wide counting.
    /// Returns whether hardware counting actually became available. All failures
    /// are treated as "hardware unavailable" (typically missing privileges) so
    /// the caller can transparently fall back to rusage-derived counters.
    fn enable_hw_counting(&mut self) -> bool {
        let Some(program) = self.programs.first() else {
            // No hardware events requested (e.g. only software counters).
            return false;
        };
        if program.config.is_empty() && program.classes == 0 {
            return false;
        }

        if apply_kpc_program(&self.ctx, program).is_err() {
            return false;
        }

        if self.classes != 0 && unsafe { self.ctx.kpc.kpc_set_counting(self.classes) } != 0 {
            return false;
        }

        true
    }
}

impl KPerfSamplingDriver {
    pub fn new(counters: &[Counter], sample_freq: u64, pid: Option<i32>) -> Result<Self, Error> {
        Self::new_with_callstacks(counters, sample_freq, pid, true)
    }

    fn new_with_callstacks(
        counters: &[Counter],
        sample_freq: u64,
        pid: Option<i32>,
        capture_callstacks: bool,
    ) -> Result<Self, Error> {
        let ctx = KPerfContext::new()?;
        let counters = counters
            .iter()
            .filter(|counter| sampling_counter_supported(counter))
            .cloned()
            .collect::<Vec<_>>();
        if counters.is_empty() {
            return Err(Error::InvalidConfiguration(
                "none of the requested counters can be sampled by macOS kperf".to_string(),
            ));
        }
        let mut config =
            build_kpc_config(&ctx, &counters, sample_period_from_freq(sample_freq), true)?;
        config.handles.retain(sampling_handle_supported);
        if config.handles.is_empty() {
            return Err(Error::InvalidConfiguration(
                "none of the requested counters can be sampled by macOS kperf".to_string(),
            ));
        }

        Ok(KPerfSamplingDriver {
            ctx,
            handles: config.handles,
            programs: config.programs,
            classes: config.classes,
            kpc_count: config.kpc_count,
            sample_freq,
            pid,
            capture_callstacks,
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        })
    }
}

impl SamplingDriver for KPerfSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        self.handles
            .iter()
            .map(|handle| handle.counter.clone())
            .collect()
    }

    fn start(&mut self, callback: Arc<dyn Sink>) -> Result<(), Error> {
        setup_kdebug_buffer().map_err(|_| Error::EnableFailed)?;

        let period_ns = (1_000_000_000_u64 / self.sample_freq.max(1)).max(1);
        let has_kpc_payload = self
            .programs
            .first()
            .is_some_and(|program| program.kpc_count > 0);
        let samplers = sampling_samplers(has_kpc_payload, self.capture_callstacks);
        if let Err(err) = configure_kperf_timer_sampler(
            &self.ctx,
            self.classes,
            self.pid.unwrap_or(-1),
            period_ns,
            samplers,
            self.capture_callstacks.then_some(64),
        ) {
            unsafe {
                self.ctx.kperf.kperf_sample_set(0);
                self.ctx.kperf.kperf_reset();
                self.ctx.kpc.kpc_set_thread_counting(0);
                self.ctx.kpc.kpc_set_counting(0);
            }
            teardown_kdebug_buffer();
            return Err(err);
        }

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let ctx = self.ctx.clone();
        let programs = self.programs.clone();
        let mut handles = initial_sampling_handles(&programs, &self.handles);
        let mut kpc_count = programs
            .first()
            .map(|program| program.kpc_count)
            .unwrap_or(self.kpc_count);
        let pid_filter = self.pid.map(|pid| pid as u32);

        let handle = thread::spawn(move || {
            let mut state = KdebugDecodeState {
                sample_period_ns: period_ns,
                ..Default::default()
            };
            let mut active_program = 0_usize;
            let mut program_epoch = 0_u64;
            let mut last_switch = Instant::now();

            loop {
                let is_running = running.load(Ordering::SeqCst);
                let mut records = vec![KdBuf::default(); 4096];
                match read_kdebug_records(&mut records) {
                    Ok(0) => {
                        if !is_running {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Ok(count) => {
                        for rec in records.into_iter().take(count) {
                            if let Some(pid) = pid_filter {
                                // Kdebug records carry thread ID, not PID. Kperf's action filter
                                // handles PID selection in the kernel; this guard is intentionally
                                // only for future records that include PID metadata.
                                let _ = pid;
                            }
                            handle_kdebug_record(
                                rec,
                                KdebugDecodeConfig {
                                    handles: &handles,
                                    kpc_count,
                                    forced_pid: pid_filter,
                                    program_epoch,
                                    time_enabled_multiplier: programs.len().max(1) as u64,
                                },
                                &mut state,
                                &callback,
                            );
                        }
                    }
                    Err(_) => {
                        if !is_running {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }

                if is_running
                    && programs.len() > 1
                    && last_switch.elapsed() >= Duration::from_millis(50)
                {
                    rotate_sampling_program(
                        &ctx,
                        &programs,
                        &mut active_program,
                        &mut program_epoch,
                        &mut handles,
                        &mut kpc_count,
                        pid_filter,
                        &mut state,
                        &callback,
                    );
                    last_switch = Instant::now();
                }
            }

            flush_incomplete_deferred_samples(&mut state, &callback);
        });
        self.thread_handle = Some(handle);

        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        unsafe {
            self.ctx.kperf.kperf_sample_set(0);
        }
        // User stacks requested by a timer interrupt are delivered later from
        // an AST. Give those ASTs a bounded chance to run before stopping trace
        // emission, then drain the now-finite kdebug buffer in the reader.
        thread::sleep(Duration::from_millis(5));
        unsafe {
            self.ctx.kperf.kperf_reset();
            self.ctx.kpc.kpc_set_thread_counting(0);
            self.ctx.kpc.kpc_set_counting(0);
        }
        let _ = sysctl_kdebug_set(KERN_KDENABLE, 0);
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| Error::EnableFailed)?;
        }
        teardown_kdebug_buffer();
        Ok(())
    }
}

fn sampling_counter_supported(counter: &Counter) -> bool {
    !counter.is_software() || matches!(counter, Counter::CpuClock)
}

fn sampling_handle_supported(handle: &NativeCounterHandle) -> bool {
    handle.counter == Counter::CpuClock || handle.kpc_index.is_some()
}

fn sampling_samplers(has_kpc_payload: bool, capture_callstacks: bool) -> u32 {
    SAMPLER_TH_INFO
        | if has_kpc_payload {
            SAMPLER_PMC_THREAD
        } else {
            0
        }
        | if capture_callstacks {
            SAMPLER_USTACK
        } else {
            0
        }
}

fn initial_sampling_handles(
    programs: &[KpcProgram],
    configured_handles: &[NativeCounterHandle],
) -> Vec<NativeCounterHandle> {
    programs
        .first()
        .map(|program| program.handles.clone())
        .unwrap_or_else(|| configured_handles.to_vec())
}

struct BuiltConfig {
    handles: Vec<NativeCounterHandle>,
    programs: Vec<KpcProgram>,
    classes: u32,
    kpc_count: usize,
}

fn build_kpc_config(
    ctx: &KPerfContext,
    counters: &[Counter],
    period: u64,
    apply_first: bool,
) -> Result<BuiltConfig, Error> {
    let mut remaining: Vec<Counter> = counters
        .iter()
        .filter(|counter| !counter.is_software())
        .cloned()
        .collect();
    let mut programs = Vec::<KpcProgram>::new();

    while !remaining.is_empty() {
        let Some(program) = build_single_kpc_program(ctx, &remaining, period)? else {
            break;
        };

        let before = remaining.len();
        remaining.retain(|counter| {
            !program
                .handles
                .iter()
                .any(|handle| handle.counter == *counter && handle.kpc_index.is_some())
        });

        if remaining.len() == before {
            break;
        }
        programs.push(program);
    }

    for (program_index, program) in programs.iter_mut().enumerate() {
        for handle in &mut program.handles {
            handle.program_index = Some(program_index);
        }
    }
    let software_handles = counters
        .iter()
        .filter(|counter| counter.is_software())
        .map(|counter| NativeCounterHandle {
            counter: counter.clone(),
            program_index: None,
            kpc_index: None,
        })
        .collect::<Vec<_>>();
    for program in &mut programs {
        program.handles.extend(software_handles.iter().cloned());
    }

    let mut handles = Vec::new();
    for counter in counters {
        let native = programs
            .iter()
            .flat_map(|program| program.handles.iter())
            .find(|handle| handle.counter == *counter)
            .cloned()
            .unwrap_or_else(|| NativeCounterHandle {
                counter: counter.clone(),
                program_index: None,
                kpc_index: None,
            });
        handles.push(native);
    }

    let classes = programs.first().map(|program| program.classes).unwrap_or(0);
    let kpc_count = programs
        .first()
        .map(|program| program.kpc_count)
        .unwrap_or(0);

    if apply_first {
        if let Some(program) = programs.first() {
            apply_kpc_program(ctx, program)?;
        }
    }

    Ok(BuiltConfig {
        handles,
        programs,
        classes,
        kpc_count,
    })
}

fn build_single_kpc_program(
    ctx: &KPerfContext,
    counters: &[Counter],
    period: u64,
) -> Result<Option<KpcProgram>, Error> {
    let mut cfg: *mut KPepConfig = ptr::null_mut();
    if unsafe { ctx.kpep.kpep_config_create(ctx.db, &mut cfg) } != 0 || cfg.is_null() {
        return Err(Error::CounterCreationFail);
    }

    let result = (|| {
        // Take ownership of all counters up front (best effort — needs root). This
        // is what frees the power-manager-reserved configurable PMCs, which in
        // turn changes kpc_get_counter_count(). We must query the counts and the
        // register map in the SAME forced state that reads will later run in, or
        // the kernel writes more counters per CPU than the read buffer was sized
        // for (a heap-corrupting overflow). Failure here is fine: without root the
        // hardware counters are never read.
        unsafe {
            ctx.kpc.kpc_force_all_ctrs_set(1);
            ctx.kpep.kpep_config_force_counters(cfg);
        }

        let mut configured = Vec::<Counter>::new();
        for counter in counters.iter().filter(|c| !c.is_software()) {
            let Some(mut event) = resolve_event(ctx, counter) else {
                continue;
            };
            let mut err = 0_u32;
            let rc = unsafe { ctx.kpep.kpep_config_add_event(cfg, &mut event, 0, &mut err) };
            if rc == 0 {
                configured.push(counter.clone());
            }
        }

        let mut config_count = 0_usize;
        unsafe {
            ctx.kpep.kpep_config_kpc_count(cfg, &mut config_count);
        }
        let mut classes = 0_u32;
        unsafe {
            ctx.kpep.kpep_config_kpc_classes(cfg, &mut classes);
        }
        let counter_count = if classes == 0 {
            0
        } else {
            unsafe { ctx.kpc.kpc_get_counter_count(classes) as usize }
        };

        let mut config = Vec::new();
        let mut periods = Vec::new();
        if config_count > 0 {
            config = vec![0_u64; config_count];
            if unsafe {
                ctx.kpep
                    .kpep_config_kpc(cfg, config.as_mut_ptr(), config.len() * size_of::<u64>())
            } != 0
            {
                return Err(Error::CounterCreationFail);
            }
            if period > 0 {
                // kpc_set_period reads counter_count entries (kernel-side), while
                // kperfdata's kpep_config_kpc_periods writes config_count entries.
                // On Apple Silicon those differ when FIXED and CONFIGURABLE classes
                // are mixed (config_count omits the fixed counters). Size for the
                // larger so kpc_set_period never reads past the buffer; the extra
                // slots keep the caller-supplied period as a harmless default.
                periods = vec![period; counter_count.max(config_count)];
                if unsafe {
                    ctx.kpep.kpep_config_kpc_periods(
                        cfg,
                        periods.as_mut_ptr(),
                        periods.len() * size_of::<u64>(),
                    )
                } != 0
                {
                    periods.fill(period);
                }
            }
        }

        // kperfdata writes this map using the KPC config count as its capacity
        // (see XNU's cpu_counters tests), not the number of events requested.
        let mut map = vec![usize::MAX; config_count.max(configured.len())];
        if !map.is_empty() {
            unsafe {
                ctx.kpep
                    .kpep_config_kpc_map(cfg, map.as_mut_ptr(), map.len() * size_of::<usize>());
            }
        }

        let mut handles = Vec::new();
        for counter in &configured {
            let kpc_index = configured
                .iter()
                .position(|c| c == counter)
                .and_then(|pos| map.get(pos).copied())
                .filter(|idx| *idx != usize::MAX);
            handles.push(NativeCounterHandle {
                counter: counter.clone(),
                program_index: None,
                kpc_index,
            });
        }

        if handles.is_empty() {
            return Ok(None);
        }

        Ok(Some(KpcProgram {
            handles,
            classes,
            kpc_count: counter_count,
            config,
            periods,
        }))
    })();

    unsafe {
        ctx.kpep.kpep_config_free(cfg);
    }

    result
}

fn apply_kpc_program(ctx: &KPerfContext, program: &KpcProgram) -> Result<(), Error> {
    if program.config.is_empty() {
        return Ok(());
    }

    if unsafe { ctx.kpc.kpc_force_all_ctrs_set(1) } != 0 {
        return Err(Error::PermissionDenied);
    }

    let mut config = program.config.clone();
    if unsafe { ctx.kpc.kpc_set_config(program.classes, config.as_mut_ptr()) } != 0 {
        return Err(Error::PermissionDenied);
    }

    if !program.periods.is_empty() {
        let mut periods = program.periods.clone();
        unsafe {
            ctx.kpc
                .kpc_set_period(program.classes, periods.as_mut_ptr());
        }
    }

    Ok(())
}

fn enable_sampling_kpc_program(ctx: &KPerfContext, program: &KpcProgram) -> Result<(), Error> {
    apply_kpc_program(ctx, program)?;
    if program.classes == 0 {
        return Ok(());
    }
    unsafe {
        if ctx.kpc.kpc_set_counting(program.classes) != 0 {
            return Err(Error::EnableFailed);
        }
        if ctx.kpc.kpc_set_thread_counting(program.classes) != 0 {
            ctx.kpc.kpc_set_counting(0);
            return Err(Error::EnableFailed);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rotate_sampling_program(
    ctx: &KPerfContext,
    programs: &[KpcProgram],
    active_program: &mut usize,
    program_epoch: &mut u64,
    handles: &mut Vec<NativeCounterHandle>,
    kpc_count: &mut usize,
    pid_filter: Option<u32>,
    state: &mut KdebugDecodeState,
    callback: &Arc<dyn Sink>,
) {
    // Stop generation first, then decode all buffered KPC records with the old
    // epoch/slot mapping. A deferred AST stack may arrive later, but its sample
    // already owns the old handles and epoch.
    unsafe {
        ctx.kperf.kperf_sample_set(0);
    }
    loop {
        let mut records = vec![KdBuf::default(); 4096];
        match read_kdebug_records(&mut records) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                for rec in records.into_iter().take(count) {
                    handle_kdebug_record(
                        rec,
                        KdebugDecodeConfig {
                            handles,
                            kpc_count: *kpc_count,
                            forced_pid: pid_filter,
                            program_epoch: *program_epoch,
                            time_enabled_multiplier: programs.len() as u64,
                        },
                        state,
                        callback,
                    );
                }
            }
        }
    }

    unsafe {
        ctx.kpc.kpc_set_thread_counting(0);
        ctx.kpc.kpc_set_counting(0);
    }

    let next_program = (*active_program + 1) % programs.len();
    let selected = if enable_sampling_kpc_program(ctx, &programs[next_program]).is_ok() {
        next_program
    } else {
        // Keep profiling with the old program if the transition fails, but use
        // a new epoch because stopping/reprogramming invalidates slot deltas.
        let _ = enable_sampling_kpc_program(ctx, &programs[*active_program]);
        *active_program
    };

    *active_program = selected;
    *handles = programs[selected].handles.clone();
    *kpc_count = programs[selected].kpc_count;
    *program_epoch = program_epoch.saturating_add(1);
    unsafe {
        ctx.kperf.kperf_sample_set(1);
    }
}

fn read_program_kpc(
    ctx: &KPerfContext,
    program: &KpcProgram,
    cpu_count: usize,
) -> Result<Vec<u64>, std::io::Error> {
    if program.kpc_count == 0 {
        return Ok(Vec::new());
    }

    // Over-allocate by KPC_MAX_COUNTERS as a guard against any counter-count
    // miscount (see read_snapshot); the kernel does no length checking. The read
    // stride stays program.kpc_count, so the truncated buffer holds exactly the
    // real per-CPU counter blocks.
    let mut kpc = vec![0_u64; cpu_count * program.kpc_count + KPC_MAX_COUNTERS];
    unsafe {
        ctx.kpc
            .kpc_get_cpu_counters(1, program.classes, ptr::null_mut(), kpc.as_mut_ptr());
    }
    kpc.truncate(cpu_count * program.kpc_count);
    Ok(kpc)
}

fn add_counter_value(values: &mut Vec<(Counter, u64)>, counter: Counter, delta: u64) {
    if let Some((_, value)) = values.iter_mut().find(|(existing, _)| *existing == counter) {
        *value = value.saturating_add(delta);
    } else {
        values.push((counter, delta));
    }
}

fn find_counter_value(values: &[(Counter, u64)], counter: &Counter) -> Option<u64> {
    values
        .iter()
        .find_map(|(existing, value)| (existing == counter).then_some(*value))
}

fn resolve_event(ctx: &KPerfContext, counter: &Counter) -> Option<*mut KPepEvent> {
    let candidates: &[&str] = match counter {
        Counter::Cycles => &[
            "Cycles",
            "cycles",
            "FIXED_CYCLES",
            "CORE_ACTIVE_CYCLE",
            "CPU_CLK_UNHALTED.THREAD",
        ],
        Counter::Instructions => &[
            "Instructions",
            "instructions",
            "FIXED_INSTRUCTIONS",
            "INST_RETIRED",
        ],
        Counter::BranchInstructions => &[
            "branches",
            "INST_BRANCH",
            "BRANCH_INDIR_NONSPEC",
            "BR_INST_RETIRED.ALL_BRANCHES",
            "ARM_BR_PRED",
        ],
        Counter::BranchMisses => &[
            "branch_misses",
            "BRANCH_MISPRED_NONSPEC",
            "BRANCH_COND_MISPRED_NONSPEC",
            "BR_MISP_RETIRED.ALL_BRANCHES",
            "ARM_BR_MIS_PRED",
        ],
        Counter::LLCReferences => &[
            "cache_references",
            "LLC_REFERENCES",
            "L2D_CACHE",
            "L2D_CACHE_REFILL",
            "ARM_L1D_CACHE",
        ],
        Counter::LLCMisses => &[
            "cache_misses",
            "LLC_MISSES",
            "L2D_CACHE_REFILL",
            "ARM_L1D_CACHE_REFILL",
        ],
        Counter::StalledCyclesFrontend => &[
            "stalled_cycles_frontend",
            "ARM_STALL_FRONTEND",
            "ARM_STALL_SLOT_FRONTEND",
            "STALL_FRONTEND",
        ],
        Counter::StalledCyclesBackend => &[
            "stalled_cycles_backend",
            "ARM_STALL_BACKEND",
            "ARM_STALL_SLOT_BACKEND",
            "STALL_BACKEND",
        ],
        Counter::Custom(name) => return resolve_event_by_name(ctx, name),
        Counter::Internal { name, .. } => return resolve_event_by_name(ctx, name),
        _ => return None,
    };

    candidates
        .iter()
        .find_map(|name| resolve_event_by_name(ctx, name))
}

fn resolve_event_by_name(ctx: &KPerfContext, name: &str) -> Option<*mut KPepEvent> {
    let c_name = CString::new(name).ok()?;
    let mut event: *mut KPepEvent = ptr::null_mut();
    let rc = unsafe { ctx.kpep.kpep_db_event(ctx.db, c_name.as_ptr(), &mut event) };
    (rc == 0 && !event.is_null()).then_some(event)
}

fn sum_counter_delta(before: &[u64], after: &[u64], width: usize, idx: usize) -> u64 {
    if width == 0 || before.len() != after.len() {
        return 0;
    }

    after
        .chunks(width)
        .zip(before.chunks(width))
        .map(|(a, b)| {
            a.get(idx)
                .copied()
                .unwrap_or(0)
                .saturating_sub(b.get(idx).copied().unwrap_or(0))
        })
        .sum()
}

fn sample_period_from_freq(freq: u64) -> u64 {
    // PMI periods are event-count periods. Use a conservative default so the
    // timer sampler remains the primary source even if the PMU accepts periods.
    (1_000_000_000_u64 / freq.max(1)).max(100_000)
}

fn logical_cpu_count() -> Option<usize> {
    let mut cpus: c_int = 0;
    let mut len = size_of::<c_int>();
    let name = CString::new("hw.logicalcpu_max").ok()?;
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut cpus as *mut _ as *mut c_void,
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && cpus > 0).then_some(cpus as usize)
}

fn read_rusage(pid: i32) -> Result<RUsageInfoV4, std::io::Error> {
    let mut info = RUsageInfoV4::default();
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            RUSAGE_INFO_V4,
            &mut info as *mut _ as *mut libc::rusage_info_t,
        )
    };
    if rc == 0 {
        Ok(info)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn read_taskinfo(pid: i32) -> Result<libc::proc_taskinfo, std::io::Error> {
    let mut info = unsafe { std::mem::zeroed::<libc::proc_taskinfo>() };
    let expected = size_of::<libc::proc_taskinfo>() as c_int;
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            expected,
        )
    };
    if bytes == expected {
        Ok(info)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Log which kperf configuration step failed (with errno) and map it to the
/// generic enable error. kperf's private calls give little detail, so the step
/// name plus errno is the most useful signal when sampling setup fails.
fn kperf_step_failed(step: &str) -> Error {
    let err = std::io::Error::last_os_error();
    eprintln!("kperf sampling setup failed at {step}: {err}");
    Error::EnableFailed
}

fn setup_kdebug_buffer() -> Result<(), std::io::Error> {
    // KDREMOVE also establishes/validates ktrace ownership. Do not ignore an
    // EBUSY from another live trace consumer and then clobber its session.
    sysctl_kdebug_set(KERN_KDREMOVE, 0)?;

    let result = (|| {
        sysctl_kdebug_set(KERN_KDSETBUF, 262_144)?;
        sysctl_kdebug_set(KERN_KDSETUP, 0)?;
        install_perf_typefilter()?;
        sysctl_kdebug_set(KERN_KDENABLE, KDEBUG_ENABLE_TRACE as c_int)
    })();

    if result.is_err() {
        let _ = sysctl_kdebug_set(KERN_KDENABLE, 0);
        let _ = sysctl_kdebug_set(KERN_KDREMOVE, 0);
    }
    result
}

fn install_perf_typefilter() -> Result<(), std::io::Error> {
    let mut bitmap = vec![0_u8; KDBG_TYPEFILTER_BITMAP_SIZE];
    let start = DBG_PERF as usize * KDBG_BYTES_PER_CLASS;
    bitmap[start..start + KDBG_BYTES_PER_CLASS].fill(0xff);

    let mut mib = [CTL_KERN, KERN_KDEBUG, KERN_KDSET_TYPEFILTER];
    let mut size = bitmap.len();
    // This legacy sysctl consumes the bitmap through oldp/oldlen even though it
    // is an input. XNU also forces DBG_TRACE on alongside our DBG_PERF class.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as c_uint,
            bitmap.as_mut_ptr() as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn configure_kperf_timer_sampler(
    ctx: &KPerfContext,
    classes: u32,
    pid: i32,
    period_ns: u64,
    samplers: u32,
    user_stack_depth: Option<u32>,
) -> Result<(), Error> {
    unsafe {
        if classes != 0 {
            if ctx.kpc.kpc_force_all_ctrs_set(1) != 0 {
                return Err(Error::PermissionDenied);
            }
            if ctx.kpc.kpc_set_counting(classes) != 0 {
                return Err(Error::PermissionDenied);
            }
            if ctx.kpc.kpc_set_thread_counting(classes) != 0 {
                return Err(Error::PermissionDenied);
            }
        }
        ctx.kperf.kperf_reset();
        // The kernel's `actionc` only ever grows for the boot: it refuses to
        // shrink (`kperf_action_set_count` returns EINVAL when count < actionc)
        // and `kperf_reset` clears each action's config but never lowers the
        // count. So if any tool (Instruments, powermetrics, a prior run) already
        // grew it past 1, `action_count_set(1)` fails — yet action slot 1 still
        // exists, and this very (failed) write already made us the ktrace owner
        // via ktrace_configure. Treat that as non-fatal and configure slot 1; a
        // genuinely broken state still surfaces on the samplers/timer calls below.
        if ctx.kperf.kperf_action_count_set(1) != 0 {
            eprintln!("kperf: action table already has multiple slots; reusing slot 1");
        }
        if ctx.kperf.kperf_action_samplers_set(1, samplers) != 0 {
            return Err(kperf_step_failed("kperf_action_samplers_set"));
        }
        if let Some(depth) = user_stack_depth {
            if ctx.kperf.kperf_action_ucallstack_depth_set(1, depth) != 0 {
                return Err(kperf_step_failed("kperf_action_ucallstack_depth_set"));
            }
        }
        if pid > 0 && ctx.kperf.kperf_action_filter_set_by_pid(1, pid) != 0 {
            return Err(kperf_step_failed("kperf_action_filter_set_by_pid"));
        }
        if ctx.kperf.kperf_timer_count_set(1) != 0 {
            return Err(kperf_step_failed("kperf_timer_count_set"));
        }
        let ticks = ctx.kperf.kperf_ns_to_ticks(period_ns.max(1));
        if ctx.kperf.kperf_timer_period_set(0, ticks) != 0 {
            return Err(kperf_step_failed("kperf_timer_period_set"));
        }
        if ctx.kperf.kperf_timer_action_set(0, 1) != 0 {
            return Err(kperf_step_failed("kperf_timer_action_set"));
        }
        if ctx.kperf.kperf_sample_set(1) != 0 {
            return Err(kperf_step_failed("kperf_sample_set"));
        }
    }

    Ok(())
}

fn teardown_kdebug_buffer() {
    let _ = sysctl_kdebug_set(KERN_KDENABLE, 0);
    let _ = sysctl_kdebug_set(KERN_KDREMOVE, 0);
}

fn sysctl_kdebug_set(op: c_int, value: c_int) -> Result<(), std::io::Error> {
    let mut mib = [CTL_KERN, KERN_KDEBUG, op, value];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as c_uint,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn read_kdebug_records(records: &mut [KdBuf]) -> Result<usize, std::io::Error> {
    let mut mib = [CTL_KERN, KERN_KDEBUG, KERN_KDREADTR];
    let mut size = size_of_val(records);
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as c_uint,
            records.as_mut_ptr() as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(size)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[derive(Clone, Copy)]
struct KdebugDecodeConfig<'a> {
    handles: &'a [NativeCounterHandle],
    kpc_count: usize,
    forced_pid: Option<u32>,
    program_epoch: u64,
    time_enabled_multiplier: u64,
}

fn handle_kdebug_record(
    rec: KdBuf,
    config: KdebugDecodeConfig<'_>,
    state: &mut KdebugDecodeState,
    callback: &Arc<dyn Sink>,
) {
    let event = rec.debugid & KDBG_EVENTID_MASK;
    match event {
        PERF_SAMPLE if (rec.debugid & DBG_FUNC_START) == DBG_FUNC_START => {
            state.pending_samples.insert(
                rec.arg5,
                PendingSample {
                    time: mach_ticks_to_nanos(rec.timestamp),
                    pid: 0,
                    tid: rec.arg5 as u32,
                    trace_tid: rec.arg5,
                    cpu: rec.cpuid,
                    kpc_values: Vec::with_capacity(config.kpc_count),
                    callstack: SmallVec::new(),
                    forced_pid: config.forced_pid,
                    timer_observation: rec.arg4 & SAMPLE_FLAG_PEND_USER != 0,
                },
            );
        }
        PERF_SAMPLE if (rec.debugid & DBG_FUNC_END) == DBG_FUNC_END => {
            let mut sample =
                state
                    .pending_samples
                    .remove(&rec.arg5)
                    .unwrap_or_else(|| PendingSample {
                        time: mach_ticks_to_nanos(rec.timestamp),
                        pid: 0,
                        tid: rec.arg5 as u32,
                        trace_tid: rec.arg5,
                        cpu: rec.cpuid,
                        kpc_values: Vec::new(),
                        callstack: SmallVec::new(),
                        forced_pid: config.forced_pid,
                        timer_observation: false,
                    });
            sample.callstack = take_pending_stack(state, &sample);
            finish_pending_sample(
                sample,
                rec.arg2,
                config.handles,
                config.program_epoch,
                config.time_enabled_multiplier,
                state,
                callback,
            );
        }
        PERF_TI_DATA => {
            let key = rec.arg5;
            let sample = state.pending_samples.entry(key).or_insert(PendingSample {
                time: mach_ticks_to_nanos(rec.timestamp),
                pid: rec.arg1 as u32,
                tid: rec.arg2 as u32,
                trace_tid: rec.arg5,
                cpu: rec.cpuid,
                kpc_values: Vec::with_capacity(config.kpc_count),
                callstack: SmallVec::new(),
                forced_pid: config.forced_pid,
                timer_observation: false,
            });
            sample.pid = rec.arg1 as u32;
            sample.tid = rec.arg2 as u32;
        }
        PERF_STK_UHDR => {
            // XNU logs the synchronous frame count in arg2 and the async
            // (notably Swift concurrency) frame count in arg4. UDATA then
            // contains the concatenated frame sequence.
            let frames = rec.arg2.saturating_add(rec.arg4) as usize;
            state.pending_stacks.insert(
                rec.arg5,
                StackBuilder {
                    frames_left: frames,
                    flags: rec.arg1,
                    synchronous_frames: rec.arg2 as usize,
                    async_start_index: rec.arg3 as usize,
                    async_frames: rec.arg4 as usize,
                    frames: SmallVec::new(),
                },
            );
        }
        PERF_STK_UDATA => {
            if let Some(stack) = state.pending_stacks.get_mut(&rec.arg5) {
                for addr in [rec.arg1, rec.arg2, rec.arg3, rec.arg4] {
                    if stack.frames_left == 0 {
                        break;
                    }
                    stack.frames.push(addr);
                    stack.frames_left -= 1;
                }
            }
        }
        PERF_KPC_DATA_THREAD => {
            if let Some(sample) = state.pending_samples.get_mut(&rec.arg5) {
                if (rec.debugid & DBG_FUNC_START) == DBG_FUNC_START {
                    sample.kpc_values.clear();
                }

                for value in [rec.arg1, rec.arg2, rec.arg3, rec.arg4] {
                    if sample.kpc_values.len() >= config.kpc_count {
                        break;
                    }
                    sample.kpc_values.push(value);
                }
            }
        }
        _ => {}
    }
}

fn mach_ticks_to_nanos(ticks: u64) -> u64 {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc != 0 || info.numer == 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer as u64, info.denom as u64)
        }
    });

    ((ticks as u128).saturating_mul(numer as u128) / denom as u128).min(u64::MAX as u128) as u64
}

fn take_pending_stack(
    state: &mut KdebugDecodeState,
    sample: &PendingSample,
) -> SmallVec<[u64; 32]> {
    state
        .pending_stacks
        .remove(&sample.trace_tid)
        .or_else(|| state.pending_stacks.remove(&(sample.pid as u64)))
        .map(StackBuilder::finish)
        .unwrap_or_default()
}

fn finish_pending_sample(
    mut sample: PendingSample,
    sample_meta_flags: u64,
    handles: &[NativeCounterHandle],
    program_epoch: u64,
    time_enabled_multiplier: u64,
    state: &mut KdebugDecodeState,
    callback: &Arc<dyn Sink>,
) {
    if sample.pid == 0 {
        if let Some(pid) = sample.forced_pid {
            sample.pid = pid;
        }
    }

    // Timer groups have SAMPLE_FLAG_PEND_USER. A KPC payload is also
    // authoritative because the later AST group never samples PMCs.
    let timer_observation = sample.timer_observation || !sample.kpc_values.is_empty();
    if timer_observation {
        if sample_meta_flags & SAMPLE_META_UPEND != 0 && sample.callstack.is_empty() {
            state
                .deferred_samples
                .entry(sample.trace_tid)
                .or_default()
                .push_back(DeferredSample {
                    sample,
                    handles: handles.to_vec(),
                    program_epoch,
                    time_enabled_multiplier,
                });
        } else {
            emit_pending_sample(
                sample,
                handles,
                program_epoch,
                time_enabled_multiplier,
                state,
                callback,
            );
        }
        return;
    }

    // A pended user callstack is logged later by XNU's kperf AST as a second
    // generic sample containing USTACK + TH_INFO but no PMC payload. Join it to
    // the timer sample instead of treating the AST delivery as another
    // CPU-clock observation.
    if sample.callstack.is_empty() {
        return;
    }
    let Some(queue) = state.deferred_samples.get_mut(&sample.trace_tid) else {
        return;
    };
    let Some(mut deferred) = queue.pop_front() else {
        return;
    };
    if queue.is_empty() {
        state.deferred_samples.remove(&sample.trace_tid);
    }
    deferred.sample.callstack = sample.callstack;
    emit_pending_sample(
        deferred.sample,
        &deferred.handles,
        deferred.program_epoch,
        deferred.time_enabled_multiplier,
        state,
        callback,
    );
}

fn flush_incomplete_deferred_samples(state: &mut KdebugDecodeState, callback: &Arc<dyn Sink>) {
    // PMU values need the deferred stack to produce a useful attributed delta.
    // CPU-clock is sampled occupancy, though, so the timer hit itself is enough
    // evidence to keep one period even when shutdown wins the AST race.
    let deferred_samples = std::mem::take(&mut state.deferred_samples);
    for deferred in deferred_samples.into_values().flat_map(VecDeque::into_iter) {
        emit_cpu_clock_observation(
            &deferred.sample,
            &deferred.handles,
            state.sample_period_ns,
            uuid::Uuid::now_v7().as_u128(),
            callback,
        );
    }
}

fn emit_pending_sample(
    sample: PendingSample,
    handles: &[NativeCounterHandle],
    program_epoch: u64,
    time_enabled_multiplier: u64,
    state: &mut KdebugDecodeState,
    callback: &Arc<dyn Sink>,
) {
    let ip = sample.callstack.first().copied().unwrap_or_default();
    let event_id = uuid::Uuid::now_v7().as_u128();

    for handle in handles {
        let Some(idx) = handle.kpc_index else {
            continue;
        };
        let Some(value) = sample.kpc_values.get(idx).copied() else {
            continue;
        };
        let key = (sample.pid, sample.tid, program_epoch, idx);
        let last = state.last_kpc_values.get(&key).copied();

        state.last_kpc_values.insert(
            key,
            LastKpcValue {
                time: sample.time,
                value,
            },
        );

        // The first observation for a thread/physical-slot/program epoch only
        // establishes a baseline. Emitting it as a zero-valued sample creates
        // bogus rows and zero-weight flamegraph stacks.
        let Some(last) = last else {
            continue;
        };
        let time_delta = sample.time.saturating_sub(last.time).max(1);
        let value_delta = value.saturating_sub(last.value);

        callback.record(Record::Sample(Sample {
            event_id,
            ip,
            pid: sample.pid,
            tid: sample.tid,
            cpu: sample.cpu,
            core: None,
            time: sample.time,
            time_enabled: time_delta.saturating_mul(time_enabled_multiplier.max(1)),
            time_running: time_delta,
            counter: handle.counter.clone(),
            value: value_delta,
            callstack: sample.callstack.iter().copied().collect(),
            lbr_callstack: Default::default(),
            user_regs: None,
            user_stack: Vec::new(),
        }));
    }

    emit_cpu_clock_observation(&sample, handles, state.sample_period_ns, event_id, callback);
}

fn emit_cpu_clock_observation(
    sample: &PendingSample,
    handles: &[NativeCounterHandle],
    sample_period_ns: u64,
    event_id: u128,
    callback: &Arc<dyn Sink>,
) {
    if sample_period_ns == 0
        || !handles
            .iter()
            .any(|handle| handle.counter == Counter::CpuClock)
    {
        return;
    }

    // The PID-filtered timer only produces an observation while its endpoint
    // thread is on-CPU. Each hit therefore contributes exactly one configured
    // period to the CPU that delivered it. Interarrival time is deliberately
    // irrelevant: back-projecting it would smear occupancy across migration or
    // scheduled-out gaps and would also lose the first hit.
    callback.record(Record::Sample(Sample {
        event_id,
        ip: sample.callstack.first().copied().unwrap_or_default(),
        pid: sample.pid,
        tid: sample.tid,
        cpu: sample.cpu,
        core: None,
        time: sample.time,
        time_enabled: sample_period_ns,
        time_running: sample_period_ns,
        counter: Counter::CpuClock,
        value: sample_period_ns,
        callstack: sample.callstack.iter().copied().collect(),
        lbr_callstack: Default::default(),
        user_regs: None,
        user_stack: Vec::new(),
    }));
}

pub fn list_supported_counters() -> Vec<Counter> {
    let mut counters = vec![
        Counter::Cycles,
        Counter::Instructions,
        Counter::LLCReferences,
        Counter::LLCMisses,
        Counter::BranchInstructions,
        Counter::BranchMisses,
        Counter::StalledCyclesFrontend,
        Counter::StalledCyclesBackend,
        Counter::CpuClock,
        Counter::PageFaults,
    ];

    if let Ok(ctx) = KPerfContext::new() {
        let mut count = 0_usize;
        if unsafe { ctx.kpep.kpep_db_events_count(ctx.db, &mut count) } == 0 && count > 0 {
            let mut events = vec![ptr::null_mut(); count];
            if unsafe {
                ctx.kpep.kpep_db_events(
                    ctx.db,
                    events.as_mut_ptr(),
                    count * size_of::<*mut KPepEvent>(),
                )
            } == 0
            {
                for event in events.into_iter().filter(|event| !event.is_null()) {
                    let mut name = ptr::null();
                    let mut desc = ptr::null();
                    unsafe {
                        ctx.kpep.kpep_event_name(event, &mut name);
                        ctx.kpep.kpep_event_description(event, &mut desc);
                    }
                    if !name.is_null() {
                        let name = unsafe { CStr::from_ptr(name) }
                            .to_string_lossy()
                            .to_string();
                        let desc = if desc.is_null() {
                            String::new()
                        } else {
                            unsafe { CStr::from_ptr(desc) }
                                .to_string_lossy()
                                .to_string()
                        };
                        counters.push(Counter::Internal {
                            name,
                            desc,
                            code: 0,
                        });
                    }
                }
            }
        }
    }

    counters
}
