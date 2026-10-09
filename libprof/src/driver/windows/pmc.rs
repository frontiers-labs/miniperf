//! ETW PMC snapshots attached to kernel context switches.
//!
//! A snapshot is a CPU-wide running count. The difference between two
//! consecutive switches on one CPU belongs to the thread switched out by the
//! latter event. Initial and malformed snapshots are deliberately discarded.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io;
use std::io::{BufWriter, Write};
use std::mem::size_of;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessIdOfThread, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};

use super::{properties, wide, windows_error, ProfilePrivilege, ProfileSource};
use crate::{Counter, Error};

#[derive(Default)]
struct Accumulator {
    previous: HashMap<u16, (u32, i64, Vec<u64>)>,
    missing_baseline: HashSet<u16>,
    totals: Vec<(Counter, u64)>,
    intervals: Vec<PmcInterval>,
    samples: Vec<PmcSample>,
    capture_intervals: bool,
    coverage: PmcCoverage,
    loss: PmcLoss,
    threads: HashMap<u32, Vec<ThreadLifetime>>,
    switch_totals: Vec<(u32, Option<u32>, u64, u64)>, // pid, optional tid filter, switches, migrations
    last_cpu: HashMap<u32, (u32, u32)>,
}

/// One coherent counter vector attributed to the thread switched out at `timestamp`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PmcInterval {
    pub cpu: u16,
    pub start: i64,
    pub end: i64,
    pub tid: u32,
    pub pid: u32,
    pub deltas: Vec<(Counter, u64)>,
}

/// A sampled instruction pointer from the same ETW clock as the PMC vectors.
#[derive(Clone, Debug, PartialEq)]
struct PmcSample {
    cpu: u16,
    timestamp: i64,
    tid: u32,
    pid: u32,
    ip: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PmcCoverage {
    pub baselines: u64,
    pub accepted: u64,
    pub rejected_vectors: u64,
    pub unrelated_missing_vectors: u64,
    pub rejected_attribution: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PmcLoss {
    pub events: u32,
    pub log_buffers: u32,
    pub realtime_buffers: u32,
}

pub(super) struct PmcTrace {
    pub intervals: Vec<PmcInterval>,
    samples: Vec<PmcSample>,
    pub totals: Vec<(Counter, u64)>,
    pub coverage: PmcCoverage,
    pub loss: PmcLoss,
    pub qpc_frequency: u64,
    pub switches: Vec<(Counter, u64)>,
    pub target_seen: bool,
}

struct ThreadLifetime {
    pid: u32,
    start: i64,
    end: Option<i64>,
}

fn decode_snapshot(data: &[u8], sources: usize) -> Option<Vec<u64>> {
    if data.len() != sources.checked_mul(8)? {
        return None;
    }
    Some(
        data.chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
    )
}

fn switch_tids(data: &[u8]) -> Option<(u32, u32)> {
    // CSwitch V2 begins with NewThreadId and OldThreadId.
    Some((
        u32::from_le_bytes(data.get(..4)?.try_into().ok()?),
        u32::from_le_bytes(data.get(4..8)?.try_into().ok()?),
    ))
}

fn thread_ids(data: &[u8]) -> Option<(u32, u32)> {
    // Thread V2 start/end data begins with ProcessId and TThreadId.
    switch_tids(data)
}

impl Accumulator {
    fn saw_thread(&self, pid: u32, tid: Option<u32>) -> bool {
        self.threads.iter().any(|(observed_tid, lifetimes)| {
            tid.is_none_or(|tid| tid == *observed_tid)
                && lifetimes.iter().any(|life| life.pid == pid)
        })
    }

    fn observe_switch(
        &mut self,
        cpu: u32,
        tid: u32,
        pid: Option<u32>,
        target_pid: u32,
        target_tid: Option<u32>,
    ) {
        if tid == 0 || pid != Some(target_pid) || target_tid.is_some_and(|target| target != tid) {
            return;
        }
        let migrated = self
            .last_cpu
            .get(&tid)
            .is_some_and(|(previous_pid, previous_cpu)| {
                *previous_pid == target_pid && *previous_cpu != cpu
            });
        self.last_cpu.insert(tid, (target_pid, cpu));
        if let Some(row) = self
            .switch_totals
            .iter_mut()
            .find(|(p, t, _, _)| *p == target_pid && *t == target_tid)
        {
            row.2 = row.2.saturating_add(1);
            row.3 = row.3.saturating_add(u64::from(migrated));
        } else {
            self.switch_totals
                .push((target_pid, target_tid, 1, u64::from(migrated)));
        }
    }

    fn reject_snapshot(&mut self, cpu: u16, target_interval: bool) {
        self.previous.remove(&cpu);
        if target_interval {
            self.missing_baseline.remove(&cpu);
            self.coverage.rejected_vectors += 1;
        } else {
            // An unrelated switch can interrupt the counter baseline. The
            // next valid switch establishes a new baseline; only fail the
            // capture if that gap swallowed a target interval.
            self.missing_baseline.insert(cpu);
            self.coverage.unrelated_missing_vectors += 1;
        }
    }

    fn thread_event(&mut self, opcode: u8, data: &[u8], timestamp: i64) {
        let Some((pid, tid)) = thread_ids(data) else {
            return;
        };
        if matches!(
            opcode as u32,
            EVENT_TRACE_TYPE_START
                | EVENT_TRACE_TYPE_END
                | EVENT_TRACE_TYPE_DC_START
                | EVENT_TRACE_TYPE_DC_END
        ) {
            self.last_cpu.remove(&tid);
        }
        let lifetimes = self.threads.entry(tid).or_default();
        match opcode as u32 {
            EVENT_TRACE_TYPE_START | EVENT_TRACE_TYPE_DC_START => {
                if let Some(open) = lifetimes.iter_mut().rev().find(|life| life.end.is_none()) {
                    open.end = Some(timestamp);
                }
                lifetimes.push(ThreadLifetime {
                    pid,
                    start: if opcode as u32 == EVENT_TRACE_TYPE_DC_START {
                        i64::MIN
                    } else {
                        timestamp
                    },
                    end: None,
                });
            }
            EVENT_TRACE_TYPE_END | EVENT_TRACE_TYPE_DC_END => {
                if let Some(open) = lifetimes.iter_mut().rev().find(|life| life.end.is_none()) {
                    open.end = Some(timestamp);
                } else {
                    lifetimes.push(ThreadLifetime {
                        pid,
                        start: i64::MIN,
                        end: Some(timestamp),
                    });
                }
            }
            _ => {}
        }
    }

    fn thread_pid_at(&self, tid: u32, timestamp: i64) -> Option<Option<u32>> {
        self.threads.get(&tid).map(|lifetimes| {
            lifetimes
                .iter()
                .rev()
                .find(|life| life.start <= timestamp && life.end.is_none_or(|end| timestamp <= end))
                .map(|life| life.pid)
        })
    }

    fn observe_at(
        &mut self,
        cpu: u16,
        incoming_tid: u32,
        tid: u32,
        target_tid: Option<u32>,
        belongs_to_pid: bool,
        pid: u32,
        timestamp: i64,
        snapshot: Vec<u64>,
        sources: &[ProfileSource],
    ) {
        let previous = self
            .previous
            .insert(cpu, (incoming_tid, timestamp, snapshot.clone()));
        let missing_baseline = self.missing_baseline.remove(&cpu);
        let Some((running_tid, start, previous)) = previous else {
            self.coverage.baselines += 1;
            if belongs_to_pid && missing_baseline {
                self.coverage.rejected_vectors += 1;
            }
            return;
        };
        if previous.len() != snapshot.len()
            || (self.capture_intervals && timestamp <= start)
            || tid == 0
            || running_tid != tid
            || !belongs_to_pid
            || target_tid.is_some_and(|target| target != tid)
        {
            self.coverage.rejected_attribution += 1;
            return;
        }
        let deltas: Option<Vec<_>> = sources
            .iter()
            .zip(snapshot)
            .zip(previous)
            .map(|((source, now), before)| {
                (now >= before).then(|| (source.counter.clone(), now - before))
            })
            .collect();
        let Some(deltas) = deltas else {
            self.coverage.rejected_vectors += 1;
            return;
        };
        for (counter, delta) in &deltas {
            if let Some((_, total)) = self.totals.iter_mut().find(|(key, _)| key == counter) {
                *total = total.saturating_add(*delta);
            } else {
                self.totals.push((counter.clone(), *delta));
            }
        }
        if self.capture_intervals {
            self.intervals.push(PmcInterval {
                cpu,
                start,
                end: timestamp,
                tid,
                pid,
                deltas,
            });
        }
        self.coverage.accepted += 1;
    }
}

struct Context {
    pid: u32,
    tid: Option<u32>,
    sources: Vec<ProfileSource>,
    values: Arc<Mutex<Accumulator>>,
    mode: DecodeMode,
    software_switches: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecodeMode {
    Live,
    OfflineThreads,
    OfflineCounters,
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = record.as_ref() else {
        return;
    };
    let provider = record.EventHeader.ProviderId;
    let Some(context) = (record.UserContext as *const Context).as_ref() else {
        return;
    };
    if context.mode == DecodeMode::OfflineCounters
        && provider.data1 == PerfInfoGuid.data1
        && provider.data2 == PerfInfoGuid.data2
        && provider.data3 == PerfInfoGuid.data3
        && provider.data4 == PerfInfoGuid.data4
        && record.EventHeader.EventDescriptor.Opcode == 46
        && !record.UserData.is_null()
    {
        let width = if record.EventHeader.Flags as u32 & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
            4
        } else {
            8
        };
        let bytes = std::slice::from_raw_parts(
            record.UserData as *const u8,
            record.UserDataLength as usize,
        );
        if bytes.len() >= width + 4 {
            let ip = if width == 4 {
                u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64
            } else {
                u64::from_le_bytes(bytes[..8].try_into().unwrap())
            };
            let tid = u32::from_le_bytes(bytes[width..width + 4].try_into().unwrap());
            let timestamp = record.EventHeader.TimeStamp;
            let mut data = context.values.lock().unwrap();
            if data.capture_intervals
                && ip != 0
                && data.thread_pid_at(tid, timestamp) == Some(Some(context.pid))
            {
                data.samples.push(PmcSample {
                    cpu: record.BufferContext.Anonymous.ProcessorIndex,
                    timestamp,
                    tid,
                    pid: context.pid,
                    ip,
                });
            }
        }
        return;
    }
    if provider.data1 != ThreadGuid.data1
        || provider.data2 != ThreadGuid.data2
        || provider.data3 != ThreadGuid.data3
        || provider.data4 != ThreadGuid.data4
        || record.UserData.is_null()
    {
        return;
    }
    let data =
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize);
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    if matches!(
        opcode as u32,
        EVENT_TRACE_TYPE_START
            | EVENT_TRACE_TYPE_END
            | EVENT_TRACE_TYPE_DC_START
            | EVENT_TRACE_TYPE_DC_END
    ) {
        if context.mode != DecodeMode::OfflineCounters {
            context
                .values
                .lock()
                .unwrap()
                .thread_event(opcode, data, record.EventHeader.TimeStamp);
        }
        return;
    }
    if opcode != 36 || context.mode == DecodeMode::OfflineThreads {
        return;
    }
    let cpu = record.BufferContext.Anonymous.ProcessorIndex;
    let Some((incoming_tid, tid)) = switch_tids(data) else {
        context.values.lock().unwrap().reject_snapshot(cpu, true);
        return;
    };
    let known_pid = context
        .values
        .lock()
        .unwrap()
        .thread_pid_at(tid, record.EventHeader.TimeStamp);
    let belongs = if context.tid == Some(tid) {
        true
    } else if context.tid.is_some() {
        false
    } else if let Some(pid) = known_pid {
        pid == Some(context.pid)
    } else if context.mode == DecodeMode::Live {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
        if thread.is_null() {
            false
        } else {
            let belongs = GetProcessIdOfThread(thread) == context.pid;
            CloseHandle(thread);
            belongs
        }
    } else {
        false
    };
    if context.software_switches && belongs {
        let pid = known_pid.flatten().or_else(|| {
            if context.mode == DecodeMode::Live {
                Some(context.pid)
            } else {
                None
            }
        });
        context.values.lock().unwrap().observe_switch(
            record.BufferContext.Anonymous.ProcessorIndex as u32,
            tid,
            pid,
            context.pid,
            context.tid,
        );
    }
    if context.sources.is_empty() {
        return;
    }
    if record.ExtendedData.is_null() {
        context.values.lock().unwrap().reject_snapshot(cpu, belongs);
        return;
    }
    let extras = std::slice::from_raw_parts(record.ExtendedData, record.ExtendedDataCount as usize);
    let Some(item) = extras
        .iter()
        .find(|item| item.ExtType as u32 == EVENT_HEADER_EXT_TYPE_PMC_COUNTERS)
    else {
        context.values.lock().unwrap().reject_snapshot(cpu, belongs);
        return;
    };
    if item.DataPtr == 0 {
        context.values.lock().unwrap().reject_snapshot(cpu, belongs);
        return;
    }
    let bytes = std::slice::from_raw_parts(item.DataPtr as *const u8, item.DataSize as usize);
    let Some(snapshot) = decode_snapshot(bytes, context.sources.len()) else {
        context.values.lock().unwrap().reject_snapshot(cpu, belongs);
        return;
    };
    context.values.lock().unwrap().observe_at(
        cpu,
        incoming_tid,
        tid,
        context.tid,
        belongs,
        context.pid,
        record.EventHeader.TimeStamp,
        snapshot,
        &context.sources,
    );
}

/// Decode a stopped WPR file using the counter order in its HardwareCounter
/// definition. ETW keeps raw QPC ticks when RAW_TIMESTAMP is requested.
fn read_etl(
    path: &Path,
    pid: u32,
    counters: &[Counter],
    capture_intervals: bool,
) -> Result<PmcTrace, Error> {
    let values = Arc::new(Mutex::new(Accumulator {
        capture_intervals,
        ..Accumulator::default()
    }));
    let mut context = Box::new(Context {
        pid,
        tid: None,
        sources: counters
            .iter()
            .enumerate()
            .map(|(index, counter)| ProfileSource {
                counter: counter.clone(),
                name: counter.name().to_owned(),
                source: index as u32,
                interval: 0,
                event_id: 0,
            })
            .collect(),
        values: values.clone(),
        mode: DecodeMode::OfflineThreads,
        software_switches: true,
    });
    let mut filename = wide(&path.to_string_lossy());
    let mut metadata = None;
    for mode in [DecodeMode::OfflineThreads, DecodeMode::OfflineCounters] {
        context.mode = mode;
        let mut logfile = EVENT_TRACE_LOGFILEW::default();
        logfile.LogFileName = filename.as_mut_ptr();
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);
        logfile.Context = &mut *context as *mut Context as *mut c_void;
        let consumer = unsafe { OpenTraceW(&mut logfile) };
        if consumer.Value == u64::MAX {
            return Err(Error::InvalidConfiguration(format!(
                "OpenTraceW({}): {}",
                path.display(),
                io::Error::last_os_error()
            )));
        }
        let frequency = logfile.LogfileHeader.PerfFreq;
        let process_status = unsafe { ProcessTrace(&consumer, 1, ptr::null(), ptr::null()) };
        let close_status = unsafe { CloseTrace(consumer) };
        if process_status != 0 {
            return Err(windows_error("ProcessTrace(WPR PMC file)", process_status));
        }
        if close_status != 0 {
            return Err(windows_error("CloseTrace(WPR PMC file)", close_status));
        }
        if frequency <= 0 {
            return Err(Error::InvalidConfiguration(
                "WPR trace has no QPC frequency".to_owned(),
            ));
        }
        metadata = Some((
            frequency as u64,
            logfile.EventsLost,
            logfile.LogfileHeader.BuffersLost,
        ));
    }
    drop(context);
    let (frequency, events_lost, buffers_lost) = metadata.expect("two offline decode passes");
    let mut data = Arc::try_unwrap(values)
        .map_err(|_| Error::InvalidConfiguration("WPR trace decoder is still in use".to_owned()))?
        .into_inner()
        .map_err(|_| Error::WorkerPanicked)?;
    data.loss.events = events_lost;
    data.loss.log_buffers = buffers_lost;
    let target_seen = data.saw_thread(pid, None)
        || data
            .switch_totals
            .iter()
            .any(|(process, _, _, _)| *process == pid);
    Ok(PmcTrace {
        intervals: data.intervals,
        samples: data.samples,
        totals: data.totals,
        coverage: data.coverage,
        loss: data.loss,
        qpc_frequency: frequency,
        target_seen,
        switches: [
            (
                Counter::ContextSwitches,
                data.switch_totals
                    .iter()
                    .filter(|(process, tid, _, _)| *process == pid && tid.is_none())
                    .map(|(_, _, value, _)| *value)
                    .sum(),
            ),
            (
                Counter::CpuMigrations,
                data.switch_totals
                    .iter()
                    .filter(|(process, tid, _, _)| *process == pid && tid.is_none())
                    .map(|(_, _, _, value)| *value)
                    .sum(),
            ),
        ]
        .into_iter()
        .collect(),
    })
}

fn validate_trace(trace: &PmcTrace) -> Result<(), Error> {
    if trace.coverage.accepted == 0 {
        return Err(Error::InvalidConfiguration(
            "WPR trace contains no attributable coherent PMC intervals".to_owned(),
        ));
    }
    if trace.loss.events != 0 || trace.loss.log_buffers != 0 || trace.loss.realtime_buffers != 0 {
        return Err(Error::InvalidConfiguration(format!(
            "WPR PMC trace lost {} events, {} log buffers, and {} real-time buffers",
            trace.loss.events, trace.loss.log_buffers, trace.loss.realtime_buffers
        )));
    }
    if trace.coverage.rejected_vectors != 0 {
        return Err(Error::InvalidConfiguration(format!(
            "WPR PMC trace contains {} missing or invalid target counter vectors ({} unrelated switches lacked vectors; {} target intervals accepted); totals would be incomplete",
            trace.coverage.rejected_vectors,
            trace.coverage.unrelated_missing_vectors,
            trace.coverage.accepted,
        )));
    }
    Ok(())
}

pub(super) fn etl_totals(
    etl_path: &Path,
    pid: u32,
    counters: &[Counter],
) -> Result<Vec<(Counter, u64)>, Error> {
    let trace = read_etl(etl_path, pid, counters, false)?;
    validate_trace(&trace)?;
    Ok(trace.totals)
}

pub(super) fn etl_switch_totals(etl_path: &Path, pid: u32) -> Result<Vec<(Counter, u64)>, Error> {
    let trace = read_etl(etl_path, pid, &[], false)?;
    if !trace.target_seen {
        return Err(Error::InvalidConfiguration(format!(
            "WPR thread trace has no lifecycle or context-switch event for process {pid}"
        )));
    }
    if trace.loss.events != 0 || trace.loss.log_buffers != 0 || trace.loss.realtime_buffers != 0 {
        return Err(Error::InvalidConfiguration(
            "WPR thread trace lost events; switch totals would be incomplete".to_owned(),
        ));
    }
    Ok(trace.switches)
}

/// Write the decoded WPR trace without building a second in-memory copy of
/// every context-switch interval. The event names match the TMA scenario.
pub(super) fn decode_etl_to_json(
    etl_path: &Path,
    json_path: &Path,
    pid: u32,
    counters: &[Counter],
) -> Result<(), Error> {
    let trace = read_etl(etl_path, pid, counters, true)?;
    validate_trace(&trace)?;
    let file = std::fs::File::create(json_path).map_err(|error| {
        Error::InvalidConfiguration(format!("cannot write {}: {error}", json_path.display()))
    })?;
    let mut writer = BufWriter::new(file);
    write!(
        writer,
        "{{\"qpc_frequency\":{},\"coverage\":{{\"baselines\":{},\"accepted\":{},\"rejected_vectors\":{},\"unrelated_missing_vectors\":{},\"rejected_attribution\":{}}},\"loss\":{{\"events\":{},\"log_buffers\":{},\"realtime_buffers\":{}}},\"intervals\":[",
        trace.qpc_frequency,
        trace.coverage.baselines,
        trace.coverage.accepted,
        trace.coverage.rejected_vectors,
        trace.coverage.unrelated_missing_vectors,
        trace.coverage.rejected_attribution,
        trace.loss.events,
        trace.loss.log_buffers,
        trace.loss.realtime_buffers,
    )
    .map_err(|error| Error::InvalidConfiguration(format!("cannot write WPR intervals: {error}")))?;
    for (index, interval) in trace.intervals.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",").map_err(|error| {
                Error::InvalidConfiguration(format!("cannot write WPR intervals: {error}"))
            })?;
        }
        let deltas = interval
            .deltas
            .iter()
            .map(|(counter, value)| (counter.name().to_owned(), *value))
            .collect::<std::collections::BTreeMap<_, _>>();
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "cpu": interval.cpu,
                "start": interval.start,
                "end": interval.end,
                "tid": interval.tid,
                "pid": interval.pid,
                "deltas": deltas,
            }),
        )
        .map_err(|error| {
            Error::InvalidConfiguration(format!("cannot serialize WPR interval: {error}"))
        })?;
    }
    writer.write_all(b"],\"samples\":[").map_err(|error| {
        Error::InvalidConfiguration(format!("cannot write WPR samples: {error}"))
    })?;
    for (index, sample) in trace.samples.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",").map_err(|error| {
                Error::InvalidConfiguration(format!("cannot write WPR samples: {error}"))
            })?;
        }
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "cpu": sample.cpu,
                "timestamp": sample.timestamp,
                "tid": sample.tid,
                "pid": sample.pid,
                "ip": sample.ip,
            }),
        )
        .map_err(|error| {
            Error::InvalidConfiguration(format!("cannot serialize WPR sample: {error}"))
        })?;
    }
    writer
        .write_all(b"]}")
        .and_then(|_| writer.flush())
        .map_err(|error| {
            Error::InvalidConfiguration(format!("cannot finish WPR intervals: {error}"))
        })
}

pub(super) struct PmcCounting {
    sources: Vec<ProfileSource>,
    pid: u32,
    tid: Option<u32>,
    name: Vec<u16>,
    values: Arc<Mutex<Accumulator>>,
    controller: Option<CONTROLTRACE_HANDLE>,
    worker: Option<thread::JoinHandle<u32>>,
    context: Option<Box<Context>>,
    privilege: Option<ProfilePrivilege>,
}

impl PmcCounting {
    pub(super) fn new(sources: Vec<ProfileSource>, pid: Option<u32>, tid: Option<u32>) -> Self {
        let pid = pid.unwrap_or_else(|| unsafe { GetCurrentProcessId() });
        Self {
            sources,
            pid,
            tid,
            name: wide(&format!("miniperf-pmc-{}-{}", pid, uuid::Uuid::now_v7())),
            values: Arc::default(),
            controller: None,
            worker: None,
            context: None,
            privilege: None,
        }
    }

    pub(super) fn start(&mut self) -> Result<(), Error> {
        if self.controller.is_some() {
            self.stop()?;
        }
        *self.values.lock().unwrap() = Accumulator::default();
        self.privilege = ProfilePrivilege::enable()?;
        let mut storage = properties(&self.name, false);
        let props = storage.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        let mut controller = CONTROLTRACE_HANDLE::default();
        let status = unsafe { StartTraceW(&mut controller, self.name.as_ptr(), props) };
        if status != 0 {
            self.privilege = None;
            return Err(windows_error("StartTraceW(PMC counting)", status));
        }
        self.controller = Some(controller);
        let ids: Vec<u32> = self.sources.iter().map(|source| source.source).collect();
        let event = CLASSIC_EVENT_ID {
            EventGuid: ThreadGuid,
            Type: 36,
            Reserved: [0; 7],
        };
        let mut settings = Vec::new();
        if !ids.is_empty() {
            settings.push((
                TracePmcCounterListInfo,
                ids.as_ptr() as *const c_void,
                (ids.len() * size_of::<u32>()) as u32,
            ));
        }
        for (class, data, len) in settings.into_iter().chain([(
            TracePmcEventListInfo,
            &event as *const _ as *const c_void,
            size_of::<CLASSIC_EVENT_ID>() as u32,
        )]) {
            let status = unsafe { TraceSetInformation(controller, class, data, len) };
            if status != 0 {
                let _ = self.stop();
                return Err(windows_error("TraceSetInformation(PMC counting)", status));
            }
        }
        let flags = [
            EVENT_TRACE_FLAG_CSWITCH | EVENT_TRACE_FLAG_THREAD,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let status = unsafe {
            TraceSetInformation(
                controller,
                TraceSystemTraceEnableFlagsInfo,
                flags.as_ptr() as *const c_void,
                size_of::<[u32; 8]>() as u32,
            )
        };
        if status != 0 {
            let _ = self.stop();
            return Err(windows_error("TraceSetInformation(CSwitch)", status));
        }
        let mut context = Box::new(Context {
            pid: self.pid,
            tid: self.tid,
            sources: self.sources.clone(),
            values: self.values.clone(),
            mode: DecodeMode::Live,
            software_switches: true,
        });
        let mut logfile = EVENT_TRACE_LOGFILEW::default();
        logfile.LoggerName = self.name.as_mut_ptr();
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);
        logfile.Context = &mut *context as *mut Context as *mut c_void;
        let consumer = unsafe { OpenTraceW(&mut logfile) };
        if consumer.Value == u64::MAX {
            let _ = self.stop();
            return Err(Error::InvalidConfiguration(format!(
                "OpenTraceW(PMC counting): {}",
                io::Error::last_os_error()
            )));
        }
        self.context = Some(context);
        self.worker = Some(thread::spawn(move || unsafe {
            let status = ProcessTrace(&consumer, 1, ptr::null(), ptr::null());
            CloseTrace(consumer);
            status
        }));
        Ok(())
    }

    pub(super) fn stop(&mut self) -> Result<(), Error> {
        if let Some(controller) = self.controller.take() {
            let mut storage = properties(&self.name, false);
            let status = unsafe {
                ControlTraceW(
                    controller,
                    self.name.as_ptr(),
                    storage.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            let process_status = self
                .worker
                .take()
                .map(|worker| worker.join())
                .transpose()
                .map_err(|_| Error::WorkerPanicked)?;
            self.context = None;
            self.privilege = None;
            if status != 0 {
                return Err(windows_error("ControlTraceW(PMC counting)", status));
            }
            if let Some(process_status) = process_status {
                if process_status != 0 {
                    return Err(windows_error("ProcessTrace(PMC counting)", process_status));
                }
            }
            let props = unsafe { &*(storage.as_ptr() as *const EVENT_TRACE_PROPERTIES) };
            self.values.lock().unwrap().loss = PmcLoss {
                events: props.EventsLost,
                log_buffers: props.LogBuffersLost,
                realtime_buffers: props.RealTimeBuffersLost,
            };
            if props.EventsLost != 0 || props.LogBuffersLost != 0 || props.RealTimeBuffersLost != 0
            {
                return Err(Error::InvalidConfiguration(format!(
                    "ETW PMC trace lost {} events, {} log buffers, and {} real-time buffers",
                    props.EventsLost, props.LogBuffersLost, props.RealTimeBuffersLost
                )));
            }
        }
        Ok(())
    }

    pub(super) fn value(&self, counter: &Counter) -> Option<u64> {
        if *counter == Counter::ContextSwitches || *counter == Counter::CpuMigrations {
            let values = self.values.lock().unwrap();
            let has_target_switch = values.switch_totals.iter().any(|(pid, tid, _, _)| {
                *pid == self.pid && (self.tid.is_none() || *tid == self.tid)
            });
            if !has_target_switch && !values.saw_thread(self.pid, self.tid) {
                return None;
            }
            return Some(
                values
                    .switch_totals
                    .iter()
                    .filter(|(pid, tid, _, _)| {
                        *pid == self.pid && (self.tid.is_none() || *tid == self.tid)
                    })
                    .map(|(_, _, switches, migrations)| {
                        if *counter == Counter::ContextSwitches {
                            *switches
                        } else {
                            *migrations
                        }
                    })
                    .fold(0_u64, u64::saturating_add),
            );
        }
        if !self.sources.iter().any(|source| &source.counter == counter) {
            return None;
        }
        self.values
            .lock()
            .unwrap()
            .totals
            .iter()
            .find(|(key, _)| key == counter)
            .map(|(_, value)| *value)
    }

    pub(super) fn has_observations(&self) -> bool {
        let values = self.values.lock().unwrap();
        if self.sources.is_empty() {
            return values.switch_totals.iter().any(|(pid, tid, _, _)| {
                *pid == self.pid && (self.tid.is_none() || *tid == self.tid)
            }) || values.saw_thread(self.pid, self.tid);
        }
        self.sources.iter().all(|source| {
            values
                .totals
                .iter()
                .any(|(counter, _)| counter == &source.counter)
        })
    }
}

impl Drop for PmcCounting {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
