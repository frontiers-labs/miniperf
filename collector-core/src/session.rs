use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use std::sync::atomic::AtomicI64;

use store::{
    ClockAnchorRows, ClockSyncRows, DeviceClockRows, EventMetaRows, EventRows, PayloadRows,
    SegmentWriter, StackRows, StringInterner, stack_hash, xxh3,
};

use crate::buffer::{Buffer, BufferQueue};
use crate::control::{CollectorStats, ControlCommand, ControlPlane};
use crate::stack::MAX_FRAMES;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TraceKind {
    Begin = 0,
    End = 1,
    Instant = 2,
    Counter = 3,
    Loss = 4,
}

/// Fixed-size wire record producers append to thread buffers; a frame list
/// of `nframes` u64s follows when stack capture is on.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventRecord {
    pub timestamp: u64,
    pub event_id: u64,
    pub instance: u64,
    pub parent: u64,
    pub flow: u64,
    pub value: i64,
    pub tid: u32,
    pub kind: u8,
    pub nframes: u8,
    pub _pad: [u8; 2],
}

const RECORD_BYTES: usize = std::mem::size_of::<EventRecord>();

pub struct HandleData {
    pub event_id: u64,
    pub capture_stack: bool,
}

struct Registry {
    interner: StringInterner,
    payloads: PayloadRows,
    seen: HashSet<u64>,
}

struct ThreadSlot {
    buffer: Mutex<Option<Buffer>>,
    dropped: AtomicU64,
}

pub struct Collector {
    pid: u32,
    queue: Arc<BufferQueue>,
    registry: Mutex<Registry>,
    slots: Mutex<Vec<Arc<ThreadSlot>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    stack_key_id: u64,
    total_dropped: AtomicU64,
    paused: AtomicBool,
    rank: AtomicI64,
    clock_sync: Mutex<ClockSyncRows>,
    device_clock: Mutex<DeviceClockRows>,
}

static COLLECTOR: RwLock<Option<Arc<Collector>>> = RwLock::new(None);
static FORKED: AtomicBool = AtomicBool::new(false);
static INIT_HOOKS: OnceLock<()> = OnceLock::new();

thread_local! {
    static SLOT: RefCell<Option<Arc<ThreadSlot>>> = const { RefCell::new(None) };
    static INSTANCE_COUNTER: RefCell<u64> = const { RefCell::new(0) };
    static INTERNAL_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True on collector-owned threads; events from them are discarded so
/// LD_PRELOAD shims do not trace the profiler itself.
pub fn thread_is_internal() -> bool {
    INTERNAL_THREAD.try_with(|flag| flag.get()).unwrap_or(true)
}

pub fn timestamp_ns() -> u64 {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(value: *mut i64) -> i32;
            fn QueryPerformanceFrequency(value: *mut i64) -> i32;
        }
        static FREQUENCY: OnceLock<i64> = OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut value = 0;
            unsafe { QueryPerformanceFrequency(&mut value) };
            value
        });
        let mut counter = 0;
        if frequency <= 0 || unsafe { QueryPerformanceCounter(&mut counter) } == 0 {
            return 0;
        }
        return (counter as u128 * 1_000_000_000 / frequency as u128) as u64;
    }
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
    }
}

fn current_thread_id() -> u32 {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::gettid() as u32
    }
    #[cfg(target_os = "macos")]
    {
        let mut tid = 0_u64;
        unsafe { libc::pthread_threadid_np(0, &mut tid) };
        tid as u32
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() }
    }
}

/// The process-wide collector, initialized from `MPERF_SESSION_DIR` on first
/// touch and reinitialized transparently in fork children.
pub fn collector() -> Option<Arc<Collector>> {
    if FORKED.swap(false, Ordering::AcqRel) {
        *COLLECTOR.write().unwrap() = None;
        let _ = SLOT.try_with(|slot| slot.borrow_mut().take());
    }
    if let Some(collector) = COLLECTOR.read().unwrap().as_ref() {
        return Some(collector.clone());
    }
    let dir = std::env::var_os(crate::SESSION_DIR_ENV)?;
    let mut guard = COLLECTOR.write().unwrap();
    if guard.is_none() {
        *guard = Some(Collector::start(PathBuf::from(dir)));
        INIT_HOOKS.get_or_init(|| unsafe {
            libc::atexit(atexit_shutdown);
            #[cfg(unix)]
            libc::pthread_atfork(None, None, Some(atfork_child));
        });
    }
    guard.clone()
}

extern "C" fn atexit_shutdown() {
    shutdown();
}

#[cfg(unix)]
extern "C" fn atfork_child() {
    FORKED.store(true, Ordering::Release);
}

/// Flush every thread's buffer, drain the writer, and close all segments.
/// Records arriving afterwards are dropped.
pub fn shutdown() {
    let collector = COLLECTOR.write().unwrap().take();
    if let Some(collector) = collector {
        collector.finish();
    }
}

impl Collector {
    fn start(dir: PathBuf) -> Arc<Collector> {
        let pid = std::process::id();
        let queue = Arc::new(BufferQueue::new());
        let mut interner = StringInterner::new(&dir, Some(pid));
        let stack_key_id = interner.intern("stack_id");
        let collector = Arc::new(Collector {
            pid,
            queue: queue.clone(),
            registry: Mutex::new(Registry {
                interner,
                payloads: PayloadRows::default(),
                seen: HashSet::new(),
            }),
            slots: Mutex::new(Vec::new()),
            writer: Mutex::new(None),
            stack_key_id,
            total_dropped: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            rank: AtomicI64::new(-1),
            clock_sync: Mutex::new(ClockSyncRows::default()),
            device_clock: Mutex::new(DeviceClockRows::default()),
        });
        let writer_collector = collector.clone();
        let writer_dir = dir.clone();
        let handle = std::thread::Builder::new()
            .name("mperf-writer".into())
            .spawn(move || {
                let _ = INTERNAL_THREAD.try_with(|flag| flag.set(true));
                writer_loop(writer_collector, writer_dir, queue)
            })
            .expect("failed to spawn collector writer thread");
        *collector.writer.lock().unwrap() = Some(handle);
        collector
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Register a trace-point payload; identity is the XXH3-64 of the payload
    /// contents, so the same source location hashes identically everywhere.
    pub fn register_payload(
        &self,
        name: &str,
        function: &str,
        file: &str,
        line: u32,
        column: u32,
        capture_stack: bool,
    ) -> HandleData {
        let mut bytes = Vec::with_capacity(name.len() + function.len() + file.len() + 10);
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(function.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(file.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&line.to_le_bytes());
        bytes.extend_from_slice(&column.to_le_bytes());
        let event_id = xxh3(&bytes);

        let mut registry = self.registry.lock().unwrap();
        if registry.seen.insert(event_id) {
            let name_id = registry.interner.intern(name);
            let function_id = registry.interner.intern(function);
            let file_id = registry.interner.intern(file);
            registry.payloads.event_id.push(event_id);
            registry.payloads.name_id.push(name_id);
            registry.payloads.function_id.push(function_id);
            registry.payloads.file_id.push(file_id);
            registry.payloads.line.push(line);
            registry.payloads.column.push(column);
        }
        HandleData {
            event_id,
            capture_stack,
        }
    }

    /// Intern a string into this process's dictionary segment.
    pub fn intern(&self, value: &str) -> u64 {
        self.registry.lock().unwrap().interner.intern(value)
    }

    /// A session-unique instance ID for a new event row.
    pub fn next_instance(&self) -> u64 {
        let Ok(counter) = INSTANCE_COUNTER.try_with(|counter| {
            let mut counter = counter.borrow_mut();
            *counter += 1;
            *counter
        }) else {
            return 0;
        };
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&self.pid.to_le_bytes());
        bytes[4..8].copy_from_slice(&current_thread_id().to_le_bytes());
        bytes[8..].copy_from_slice(&counter.to_le_bytes());
        xxh3(&bytes)
    }

    /// Append one record (plus optional stack frames) to this thread's
    /// buffer. Never blocks: on pool exhaustion the record is dropped and
    /// counted, surfacing later as a Loss row.
    pub fn record(&self, mut record: EventRecord, frames: &[u64]) {
        if thread_is_internal() || self.paused.load(Ordering::Relaxed) {
            return;
        }
        record.nframes = frames.len().min(MAX_FRAMES) as u8;
        let needed = RECORD_BYTES + record.nframes as usize * 8;
        let Some(slot) = self.thread_slot() else {
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut buffer_guard = slot.buffer.lock().unwrap();
        if buffer_guard
            .as_ref()
            .is_none_or(|buffer| !buffer.has_room(needed))
        {
            if let Some(full) = buffer_guard.take() {
                self.queue.submit(full);
            }
            *buffer_guard = self.queue.acquire();
        }
        let Some(buffer) = buffer_guard.as_mut() else {
            slot.dropped.fetch_add(1, Ordering::Relaxed);
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let dropped = slot.dropped.swap(0, Ordering::Relaxed);
        if dropped != 0 {
            let loss = EventRecord {
                timestamp: record.timestamp,
                event_id: 0,
                instance: 0,
                parent: 0,
                flow: 0,
                value: dropped as i64,
                tid: record.tid,
                kind: TraceKind::Loss as u8,
                nframes: 0,
                _pad: [0; 2],
            };
            append(buffer, &loss, &[]);
        }
        append(buffer, &record, &frames[..record.nframes as usize]);
    }

    pub fn dropped(&self) -> u64 {
        self.total_dropped.load(Ordering::Relaxed)
    }

    /// Record this process's MPI (or launcher) rank for process metadata.
    pub fn set_rank(&self, rank: i64) {
        self.rank.store(rank, Ordering::Relaxed);
    }

    /// Record one bracketed device-clock calibration pair.
    pub fn device_clock_pair(
        &self,
        device: &str,
        host_before_ns: i64,
        device_ns: i64,
        host_after_ns: i64,
    ) {
        let mut rows = self.device_clock.lock().unwrap();
        rows.device.push(device.to_owned());
        rows.host_before_ns.push(host_before_ns);
        rows.device_ns.push(device_ns);
        rows.host_after_ns.push(host_after_ns);
    }

    /// Record one cross-node clock-offset measurement.
    pub fn clock_sync(
        &self,
        peer: u32,
        phase: &str,
        local_ns: i64,
        peer_ns: i64,
        uncertainty_ns: i64,
    ) {
        let mut rows = self.clock_sync.lock().unwrap();
        rows.peer.push(peer);
        rows.phase.push(phase.to_owned());
        rows.local_ns.push(local_ns);
        rows.peer_ns.push(peer_ns);
        rows.uncertainty_ns.push(uncertainty_ns);
    }

    fn thread_slot(&self) -> Option<Arc<ThreadSlot>> {
        SLOT.try_with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(slot) = slot.as_ref() {
                return slot.clone();
            }
            let new = Arc::new(ThreadSlot {
                buffer: Mutex::new(None),
                dropped: AtomicU64::new(0),
            });
            self.slots.lock().unwrap().push(new.clone());
            *slot = Some(new.clone());
            new
        })
        .ok()
    }

    fn finish(&self) {
        for slot in self.slots.lock().unwrap().iter() {
            if let Some(buffer) = slot.buffer.lock().unwrap().take() {
                self.queue.submit(buffer);
            }
        }
        self.queue.close();
        if let Some(handle) = self.writer.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn append(buffer: &mut Buffer, record: &EventRecord, frames: &[u64]) {
    let header = unsafe {
        std::slice::from_raw_parts(record as *const EventRecord as *const u8, RECORD_BYTES)
    };
    buffer.data.extend_from_slice(header);
    for frame in frames {
        buffer.data.extend_from_slice(&frame.to_le_bytes());
    }
}

fn writer_loop(collector: Arc<Collector>, dir: PathBuf, queue: Arc<BufferQueue>) {
    let pid = collector.pid;
    let control = ControlPlane::create(pid);
    let mut total_events = 0u64;
    let mut events = EventRows::default();
    let mut events_writer = SegmentWriter::new(&dir, "events", Some(pid), EventRows::schema());
    let mut meta = EventMetaRows::default();
    let mut meta_writer =
        SegmentWriter::new(&dir, "event_meta", Some(pid), EventMetaRows::schema());
    let mut stacks = StackRows::default();
    let mut stacks_writer = SegmentWriter::new(&dir, "stacks", Some(pid), StackRows::schema());
    let mut stacks_seen = HashSet::new();
    let mut clock = ClockAnchorRows::default();
    clock.push("start");

    let flush = |rows: &mut EventRows, writer: &mut SegmentWriter| {
        if rows.is_empty() {
            return;
        }
        if let Err(err) = rows.to_batch().and_then(|batch| writer.write(&batch)) {
            eprintln!("mperf-collector: failed to write events: {err:#}");
        }
    };

    while let Some(buffer) = queue.next_full() {
        let mut offset = 0;
        let data = &buffer.data;
        while offset + RECORD_BYTES <= data.len() {
            let record = unsafe {
                std::ptr::read_unaligned(data.as_ptr().add(offset) as *const EventRecord)
            };
            offset += RECORD_BYTES;
            let mut frames = Vec::with_capacity(record.nframes as usize);
            for _ in 0..record.nframes {
                if offset + 8 > data.len() {
                    break;
                }
                frames.push(u64::from_le_bytes(
                    data[offset..offset + 8].try_into().unwrap(),
                ));
                offset += 8;
            }
            events.timestamp.push(record.timestamp as i64);
            events.event_id.push(record.event_id);
            events.instance.push(record.instance);
            events.parent_id.push(record.parent);
            events.flow_id.push(record.flow);
            events.kind.push(record.kind);
            events.pid.push(pid);
            events.tid.push(record.tid);
            events.value.push(record.value);
            total_events += 1;
            if !frames.is_empty() {
                let stack_id = stack_hash(&frames);
                if stacks_seen.insert(stack_id) {
                    stacks.stack_id.push(stack_id);
                    stacks.frames.push(frames);
                }
                meta.event_instance.push(record.instance);
                meta.key_id.push(collector.stack_key_id);
                meta.value_type.push(0);
                meta.value_int.push(stack_id as i64);
                meta.value_double.push(0.0);
                meta.value_string_id.push(0);
            }
        }
        queue.recycle(buffer);
        if events.len() >= 8192 {
            flush(&mut events, &mut events_writer);
        }
        if let Some(control) = &control {
            while let Some(command) = control.poll_command() {
                match command {
                    ControlCommand::Pause => collector.paused.store(true, Ordering::Relaxed),
                    ControlCommand::Resume => collector.paused.store(false, Ordering::Relaxed),
                    ControlCommand::Flush => flush(&mut events, &mut events_writer),
                }
            }
            control.publish(CollectorStats {
                pid,
                events: total_events,
                dropped: collector.dropped(),
                timestamp: timestamp_ns(),
            });
        }
    }

    flush(&mut events, &mut events_writer);
    clock.push("end");
    let mut registry = collector.registry.lock().unwrap();
    let steps = [
        ("events", events_writer.finish().map(|_| ())),
        (
            "event_meta",
            meta.to_batch().and_then(|batch| {
                if batch.num_rows() > 0 {
                    meta_writer.write(&batch)?;
                }
                meta_writer.finish().map(|_| ())
            }),
        ),
        (
            "stacks",
            stacks.to_batch().and_then(|batch| {
                if batch.num_rows() > 0 {
                    stacks_writer.write(&batch)?;
                }
                stacks_writer.finish().map(|_| ())
            }),
        ),
        (
            "payloads",
            registry.payloads.to_batch().and_then(|batch| {
                let mut writer =
                    SegmentWriter::new(&dir, "payloads", Some(pid), PayloadRows::schema());
                if batch.num_rows() > 0 {
                    writer.write(&batch)?;
                }
                writer.finish().map(|_| ())
            }),
        ),
        ("strings", registry.interner.finish()),
        (
            "clock",
            clock.to_batch().and_then(|batch| {
                let mut writer =
                    SegmentWriter::new(&dir, "clock", Some(pid), ClockAnchorRows::schema());
                writer.write(&batch)?;
                writer.finish().map(|_| ())
            }),
        ),
    ];
    for (name, result) in steps {
        if let Err(err) = result {
            eprintln!("mperf-collector: failed to write {name}: {err:#}");
        }
    }
    {
        let mut sync = collector.clock_sync.lock().unwrap();
        if !sync.is_empty() {
            let result = sync.to_batch().and_then(|batch| {
                let mut writer =
                    SegmentWriter::new(&dir, "clock_sync", Some(pid), ClockSyncRows::schema());
                writer.write(&batch)?;
                writer.finish().map(|_| ())
            });
            if let Err(err) = result {
                eprintln!("mperf-collector: failed to write clock_sync: {err:#}");
            }
        }
    }
    {
        let mut pairs = collector.device_clock.lock().unwrap();
        if !pairs.is_empty() {
            let result = pairs.to_batch().and_then(|batch| {
                let mut writer =
                    SegmentWriter::new(&dir, "device_clock", Some(pid), DeviceClockRows::schema());
                writer.write(&batch)?;
                writer.finish().map(|_| ())
            });
            if let Err(err) = result {
                eprintln!("mperf-collector: failed to write device_clock: {err:#}");
            }
        }
    }
    write_process_metadata(&dir, pid, &collector);
    if let Some(control) = &control {
        control.publish(CollectorStats {
            pid,
            events: total_events,
            dropped: collector.dropped(),
            timestamp: timestamp_ns(),
        });
        control.close();
    }
}

pub fn current_tid() -> u32 {
    current_thread_id()
}

fn write_process_metadata(dir: &Path, pid: u32, collector: &Collector) {
    #[cfg(unix)]
    let hostname = {
        let mut buffer = [0u8; 256];
        let ok =
            unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
        if ok == 0 {
            let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
            String::from_utf8_lossy(&buffer[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(windows)]
    let hostname = {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetComputerNameW(buffer: *mut u16, size: *mut u32) -> i32;
        }
        let mut buffer = [0u16; 256];
        let mut len = buffer.len() as u32;
        if unsafe { GetComputerNameW(buffer.as_mut_ptr(), &mut len) } != 0 {
            String::from_utf16_lossy(&buffer[..len as usize])
        } else {
            String::new()
        }
    };
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let rank = collector.rank.load(Ordering::Relaxed);
    let rank = if rank >= 0 {
        rank.to_string()
    } else {
        "null".to_string()
    };
    let json = format!(
        "{{\"pid\":{pid},\"ppid\":{},\"exe\":{},\"hostname\":{},\"rank\":{rank}}}",
        parent_pid(),
        json_string(&exe),
        json_string(&hostname),
    );
    let path = dir.join(format!("process-{pid}.json"));
    if let Err(err) = std::fs::write(&path, json) {
        eprintln!("mperf-collector: failed to write {}: {err}", path.display());
    }
}

#[cfg(unix)]
fn parent_pid() -> u32 {
    unsafe { libc::getppid() as u32 }
}

#[cfg(windows)]
fn parent_pid() -> u32 {
    // Windows has no direct getppid equivalent; an unknown parent is recorded
    // as zero until the launcher supplies richer process metadata.
    0
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
