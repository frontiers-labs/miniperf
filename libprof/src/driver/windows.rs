//! Windows process accounting and ETW sampled execution profiles.
//!
//! ETW supports both timer samples and samples on hardware counter overflow.
//! Hardware profile sources are discovered from the running Windows HAL.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use smallvec::SmallVec;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_NOT_ALL_ASSIGNED,
    ERROR_PRIVILEGE_NOT_HELD, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, CONTEXT};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::Diagnostics::Debug::{
    Wow64GetThreadContext, WOW64_CONTEXT, WOW64_CONTEXT_CONTROL,
};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetProcessIdOfThread,
    GetProcessTimes, GetThreadTimes, OpenProcess, OpenProcessToken, OpenThread, ResumeThread,
    SuspendThread, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_GET_CONTEXT,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::Threading::{IsWow64Process, Wow64SuspendThread};
use windows_sys::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

use super::{
    CounterEntry, CounterResult, CounterValue, CountingDriver, MeasurementQuality, SamplingDriver,
    Sink,
};
use crate::sink::{Record, Sample};
use crate::{Counter, Error};

mod pmc;
pub(crate) mod wpr;
use pmc::PmcCounting;

/// Decode context-switch PMC snapshots from a WPR ETL into miniperf JSON.
pub fn windows_decode_pmc_etl(
    etl_path: &std::path::Path,
    json_path: &std::path::Path,
    pid: u32,
    counters: &[Counter],
) -> Result<(), Error> {
    pmc::decode_etl_to_json(etl_path, json_path, pid, counters)
}

/// Sum coherent context-switch PMC vectors from a stopped WPR ETL for one process.
pub fn windows_pmc_etl_totals(
    etl_path: &std::path::Path,
    pid: u32,
    counters: &[Counter],
) -> Result<Vec<(Counter, u64)>, Error> {
    pmc::etl_totals(etl_path, pid, counters)
}

/// Count process-attributed context switches and observed CPU migrations in a stopped ETW trace.
pub fn windows_switch_etl_totals(
    etl_path: &std::path::Path,
    pid: u32,
) -> Result<Vec<(Counter, u64)>, Error> {
    pmc::etl_switch_totals(etl_path, pid)
}

/// Maximum number of profile sources ETW can collect at once on this host.
/// A failed global query leaves callers free to let ETW validate the group.
pub fn windows_max_pmc_sources() -> Option<usize> {
    let mut max = 0u32;
    let mut returned = 0u32;
    let status = unsafe {
        TraceQueryInformation(
            CONTROLTRACE_HANDLE::default(),
            TraceMaxPmcCounterQuery,
            &mut max as *mut _ as *mut c_void,
            size_of::<u32>() as u32,
            &mut returned,
        )
    };
    (status == 0 && returned as usize >= size_of::<u32>() && max > 0).then_some(max as usize)
}

fn windows_error(operation: &str, code: u32) -> Error {
    Error::InvalidConfiguration(format!(
        "Windows {operation} failed: {}",
        io::Error::from_raw_os_error(code as i32)
    ))
}

struct ProfilePrivilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}

impl ProfilePrivilege {
    fn enable() -> Result<Option<Self>, Error> {
        let mut token = ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        } == 0
        {
            return Ok(None);
        }
        let name: Vec<u16> = "SeSystemProfilePrivilege\0".encode_utf16().collect();
        let mut luid = unsafe { std::mem::zeroed() };
        if unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) } == 0 {
            unsafe { CloseHandle(token) };
            return Err(Error::InvalidConfiguration(format!(
                "cannot resolve Windows system profiling privilege: {}",
                io::Error::last_os_error()
            )));
        }
        let mut requested = TOKEN_PRIVILEGES::default();
        requested.PrivilegeCount = 1;
        requested.Privileges[0].Luid = luid;
        requested.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut returned = 0;
        let ok = unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &requested,
                size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut previous,
                &mut returned,
            )
        };
        let code = unsafe { GetLastError() };
        if ok == 0 || code == ERROR_NOT_ALL_ASSIGNED {
            unsafe { CloseHandle(token) };
            return Ok(None);
        }
        Ok(Some(Self { token, previous }))
    }
}

impl Drop for ProfilePrivilege {
    fn drop(&mut self) {
        unsafe {
            AdjustTokenPrivileges(
                self.token,
                0,
                &self.previous,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CloseHandle(self.token);
        }
    }
}

fn open_process(pid: u32) -> Result<HANDLE, Error> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        Err(Error::InvalidConfiguration(format!(
            "cannot open Windows process {pid}: {}",
            io::Error::last_os_error()
        )))
    } else {
        Ok(handle)
    }
}

fn filetime_ticks(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[derive(Clone, Copy)]
struct Snapshot {
    cycles: u64,
    cpu_100ns: u64,
    page_faults: u64,
}

fn snapshot(handle: HANDLE, process_scope: bool) -> io::Result<Snapshot> {
    let (mut creation, mut exit, mut kernel, mut user) = unsafe { std::mem::zeroed() };
    let times_ok = if process_scope {
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
    } else {
        unsafe { GetThreadTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
    };
    if times_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let page_faults = if process_scope {
        let mut memory = PROCESS_MEMORY_COUNTERS::default();
        memory.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if unsafe { GetProcessMemoryInfo(handle, &mut memory, memory.cb) } == 0 {
            return Err(io::Error::last_os_error());
        }
        memory.PageFaultCount as u64
    } else {
        0
    };
    let mut cycles = 0;
    let cycles_ok = if process_scope {
        unsafe { QueryProcessCycleTime(handle, &mut cycles) }
    } else {
        unsafe { QueryThreadCycleTime(handle, &mut cycles) }
    };
    if cycles_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Snapshot {
        cycles,
        cpu_100ns: filetime_ticks(kernel).saturating_add(filetime_ticks(user)),
        page_faults,
    })
}

#[derive(Clone)]
struct ProfileSource {
    counter: Counter,
    name: String,
    source: u32,
    interval: u32,
    event_id: u128,
}

fn profile_counter(name: &str) -> Option<Counter> {
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match name.as_str() {
        "totalcycles" | "cpucycles" | "unhaltedcorecycles" => Some(Counter::Cycles),
        "instructionretired" | "instructionsretired" | "retiredinstructions" => {
            Some(Counter::Instructions)
        }
        "branchinstructions" | "branchinstructionsretired" | "branchesretired" => {
            Some(Counter::BranchInstructions)
        }
        "branchmispredictions" | "branchmispredicts" | "mispredictedbranches" => {
            Some(Counter::BranchMisses)
        }
        "l3cacheaccess" | "l3cacheaccesses" | "llcreferences" => Some(Counter::LLCReferences),
        "l3cachemiss" | "l3cachemisses" | "llcmisses" => Some(Counter::LLCMisses),
        _ => None,
    }
}

fn source_matches_counter(source: &ProfileSource, counter: &Counter) -> bool {
    match counter {
        // Custom requests name the HAL source itself. Ignore ASCII case only;
        // punctuation/spacing differences can identify a different source.
        Counter::Custom(name) => source.name.eq_ignore_ascii_case(name),
        _ => source.counter == *counter,
    }
}

fn selected_profile_source(
    available: &[ProfileSource],
    counter: &Counter,
) -> Option<ProfileSource> {
    available
        .iter()
        .find(|source| source_matches_counter(source, counter))
        .cloned()
        .map(|mut source| {
            // Samples and PMC totals must retain the requested spelling. A
            // custom source can match the HAL name with different ASCII case.
            source.counter = counter.clone();
            source
        })
}

pub(super) fn wpr_profile_source_name(counter: &Counter) -> Result<String, Error> {
    let sources = match profile_sources() {
        Ok(sources) => sources,
        Err(_) if matches!(counter, Counter::Cycles | Counter::Instructions) => Vec::new(),
        Err(error) => return Err(error),
    };
    selected_profile_source(&sources, counter)
        .map(|source| source.name)
        .or_else(|| match counter {
            Counter::Cycles => Some("TotalCycles".to_owned()),
            Counter::Instructions => Some("InstructionRetired".to_owned()),
            _ => None,
        })
        .ok_or_else(|| Error::UnsupportedCounter {
            counter: counter.name().to_owned(),
            family: "Windows HAL profile sources".to_owned(),
        })
}

fn fixed_tma_source_name(sources: &[ProfileSource], counter: &Counter) -> Option<String> {
    let preferred = match counter {
        Counter::Cycles => "UnhaltedCoreCyclesFixed",
        Counter::Instructions => "InstructionsRetiredFixed",
        _ => return None,
    };
    sources
        .iter()
        .find(|source| source.name.eq_ignore_ascii_case(preferred))
        .map(|source| source.name.clone())
}

/// Prefer fixed PMCs for the architectural TMA baseline, preserving general
/// programmable counters for the model-specific event group.
pub(super) fn wpr_tma_source_name(counter: &Counter) -> Result<String, Error> {
    if let Ok(sources) = profile_sources() {
        if let Some(name) = fixed_tma_source_name(&sources, counter) {
            return Ok(name);
        }
    }
    wpr_profile_source_name(counter)
}

fn countable_profile_source(name: &str, source: u32) -> bool {
    source != 0
        && !["Timer", "TimerFixed"]
            .iter()
            .any(|timer| name.eq_ignore_ascii_case(timer))
}

fn profile_sources() -> Result<Vec<ProfileSource>, Error> {
    let mut needed = 0_u32;
    // TraceId 0 asks for the machine-wide profile-source list. An initial
    // ERROR_BAD_LENGTH/ERROR_MORE_DATA is expected on the size probe.
    let status = unsafe {
        TraceQueryInformation(
            CONTROLTRACE_HANDLE::default(),
            TraceProfileSourceListInfo,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 || (status != 0 && status != 24 && status != 122) {
        return Err(windows_error(
            "TraceQueryInformation(profile sources)",
            status,
        ));
    }
    if needed > 1024 * 1024 {
        return Err(Error::InvalidConfiguration(
            "Windows profile-source list is too large".to_owned(),
        ));
    }
    let mut storage = vec![0_u64; (needed as usize + 7) / 8];
    let status = unsafe {
        TraceQueryInformation(
            CONTROLTRACE_HANDLE::default(),
            TraceProfileSourceListInfo,
            storage.as_mut_ptr() as *mut c_void,
            (storage.len() * 8) as u32,
            &mut needed,
        )
    };
    if status != 0 {
        return Err(windows_error(
            "TraceQueryInformation(profile sources)",
            status,
        ));
    }
    if needed as usize > storage.len() * 8 {
        return Err(Error::InvalidConfiguration(
            "Windows profile-source list changed during query".to_owned(),
        ));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(storage.as_ptr() as *const u8, needed as usize) };
    let mut sources = Vec::new();
    let mut offset = 0_usize;
    loop {
        // Description begins at byte 24; size_of includes its one WCHAR and
        // trailing alignment padding, which need not be present in the blob.
        if bytes.len().saturating_sub(offset) < 26 {
            return Err(Error::InvalidConfiguration(
                "malformed Windows profile-source list".to_owned(),
            ));
        }
        let next = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let source = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let end = if next == 0 {
            bytes.len()
        } else {
            offset
                .checked_add(next)
                .filter(|end| *end <= bytes.len() && *end > offset + 24)
                .ok_or_else(|| {
                    Error::InvalidConfiguration(
                        "malformed Windows profile-source offset".to_owned(),
                    )
                })?
        };
        let name = bytes[offset + 24..end]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|word| *word != 0)
            .collect::<Vec<_>>();
        let name = String::from_utf16_lossy(&name);
        if !name.is_empty() && countable_profile_source(&name, source) {
            let counter = profile_counter(&name).unwrap_or_else(|| Counter::Custom(name.clone()));
            // PmcCounterProf carries ProfileSource as a 16-bit field.
            if source <= u16::MAX as u32
                && !sources
                    .iter()
                    .any(|item: &ProfileSource| item.source == source)
            {
                let mut interval = TRACE_PROFILE_INTERVAL {
                    Source: source,
                    Interval: 0,
                };
                let mut returned = 0;
                let status = unsafe {
                    TraceQueryInformation(
                        CONTROLTRACE_HANDLE::default(),
                        TraceSampledProfileIntervalInfo,
                        &mut interval as *mut _ as *mut c_void,
                        size_of::<TRACE_PROFILE_INTERVAL>() as u32,
                        &mut returned,
                    )
                };
                if status == 0 && interval.Interval > 0 {
                    sources.push(ProfileSource {
                        counter,
                        name: name.clone(),
                        source,
                        interval: interval.Interval,
                        event_id: uuid::Uuid::now_v7().as_u128(),
                    });
                }
            }
        }
        if next == 0 {
            break;
        }
        offset = end;
    }
    Ok(sources)
}

pub fn list_supported_counters() -> Vec<Counter> {
    let mut counters = vec![Counter::Cycles, Counter::CpuClock, Counter::PageFaults];
    // Context-switch software counts use ETW scheduling records, so they can
    // work even when no programmable profile source is exposed by the HAL.
    let mut switch_probe = PmcCounting::new(Vec::new(), None, None);
    if switch_probe.start().is_ok() && switch_probe.stop().is_ok() {
        counters.push(Counter::ContextSwitches);
        counters.push(Counter::CpuMigrations);
    }
    if let Ok(sources) = profile_sources() {
        // A token can hold SeSystemProfilePrivilege and still be denied a
        // system logger. Probe the actual ETW path before advertising PMU
        // events to stat's default selection.
        if let Some(first) = sources
            .iter()
            .find(|source| source.counter == Counter::Cycles)
            .or_else(|| sources.first())
            .cloned()
        {
            let mut probe = PmcCounting::new(vec![first], None, None);
            if probe.start().is_ok() && probe.stop().is_ok() {
                for source in sources {
                    if !counters.contains(&source.counter) {
                        counters.push(source.counter);
                    }
                }
            }
        }
    }
    counters
}

pub struct WindowsCountingDriver {
    handle: HANDLE,
    process_scope: bool,
    counters: Vec<Counter>,
    before: Option<Snapshot>,
    after: Option<Snapshot>,
    pmc: Option<PmcCounting>,
}

impl WindowsCountingDriver {
    pub fn new(counters: Vec<Counter>, pid: Option<i32>) -> Result<Self, Error> {
        let process_scope = pid.is_some();
        let available = profile_sources().unwrap_or_default();
        let pmc_sources: Vec<_> = counters
            .iter()
            .filter_map(|counter| selected_profile_source(&available, counter))
            .collect();
        if pmc_sources.iter().enumerate().any(|(index, source)| {
            pmc_sources[..index]
                .iter()
                .any(|other| other.source == source.source)
        }) {
            return Err(Error::InvalidConfiguration(
                "the requested Windows counters include two names for one ETW profile source"
                    .to_owned(),
            ));
        }
        for counter in &counters {
            if (!process_scope && *counter == Counter::PageFaults)
                || !matches!(
                    counter,
                    Counter::Cycles
                        | Counter::CpuClock
                        | Counter::PageFaults
                        | Counter::ContextSwitches
                        | Counter::CpuMigrations
                ) && !pmc_sources.iter().any(|source| &source.counter == counter)
            {
                return Err(Error::UnsupportedCounter {
                    counter: counter.name().to_owned(),
                    family: if process_scope {
                        "Windows process accounting"
                    } else {
                        "Windows thread accounting"
                    }
                    .to_owned(),
                });
            }
        }
        let handle = if let Some(pid) = pid {
            open_process(pid as u32)?
        } else {
            let tid = unsafe { GetCurrentThreadId() };
            let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid) };
            if handle.is_null() {
                return Err(Error::InvalidConfiguration(format!(
                    "cannot open current thread {tid}: {}",
                    io::Error::last_os_error()
                )));
            }
            handle
        };
        let needs_switches = counters
            .iter()
            .any(|counter| matches!(counter, Counter::ContextSwitches | Counter::CpuMigrations));
        Ok(Self {
            handle,
            process_scope,
            counters,
            before: None,
            after: None,
            pmc: if pmc_sources.is_empty() && !needs_switches {
                None
            } else {
                Some(PmcCounting::new(
                    pmc_sources,
                    pid.map(|pid| pid as u32),
                    if process_scope {
                        None
                    } else {
                        Some(unsafe { GetCurrentThreadId() })
                    },
                ))
            },
        })
    }
}

impl Drop for WindowsCountingDriver {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl CountingDriver for WindowsCountingDriver {
    fn start(&mut self) -> Result<(), Error> {
        self.before = Some(snapshot(self.handle, self.process_scope).map_err(|e| {
            Error::InvalidConfiguration(format!("cannot read Windows counters: {e}"))
        })?);
        self.after = None;
        if let Some(pmc) = &mut self.pmc {
            if let Err(error) = pmc.start() {
                if self.counters.iter().all(|counter| {
                    matches!(
                        counter,
                        Counter::Cycles | Counter::CpuClock | Counter::PageFaults
                    )
                }) {
                    self.pmc = None;
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        if let Some(pmc) = &mut self.pmc {
            pmc.stop()?;
            if !pmc.has_observations() {
                if self.counters.iter().all(|counter| {
                    matches!(
                        counter,
                        Counter::Cycles | Counter::CpuClock | Counter::PageFaults
                    )
                }) {
                    self.pmc = None;
                } else {
                    return Err(Error::InvalidConfiguration(
                        "ETW PMC trace contained no attributable context-switch intervals"
                            .to_owned(),
                    ));
                }
            }
        }
        self.after = Some(snapshot(self.handle, self.process_scope).map_err(|e| {
            Error::InvalidConfiguration(format!("cannot read Windows counters: {e}"))
        })?);
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Error> {
        self.start()
    }

    fn counters(&mut self) -> Result<CounterResult, io::Error> {
        let before = self
            .before
            .ok_or_else(|| io::Error::other("Windows counters have not started"))?;
        let after = match self.after {
            Some(value) => value,
            None => snapshot(self.handle, self.process_scope)?,
        };
        let entries = self
            .counters
            .iter()
            .map(|counter| {
                let pmc_value = self.pmc.as_ref().and_then(|pmc| pmc.value(counter));
                let value = if let Some(value) = pmc_value {
                    value
                } else {
                    match counter {
                        Counter::Cycles => after.cycles.saturating_sub(before.cycles),
                        Counter::CpuClock => after
                            .cpu_100ns
                            .saturating_sub(before.cpu_100ns)
                            .saturating_mul(100),
                        Counter::PageFaults => after.page_faults.saturating_sub(before.page_faults),
                        Counter::ContextSwitches | Counter::CpuMigrations => 0,
                        _ => 0,
                    }
                };
                // QueryProcessCycleTime includes kernel time, while perf's ordinary
                // user-space cycles event excludes it. The API value is direct,
                // but its meaning differs from the cross-platform PMU event.
                let quality =
                    if matches!(counter, Counter::ContextSwitches | Counter::CpuMigrations) {
                        MeasurementQuality::Exact
                    } else if pmc_value.is_some() || *counter == Counter::Cycles {
                        MeasurementQuality::Estimated
                    } else {
                        MeasurementQuality::Exact
                    };
                CounterEntry {
                    core: None,
                    counter: counter.clone(),
                    value: CounterValue {
                        value,
                        scaling: 1.0,
                        quality,
                    },
                }
            })
            .collect();
        Ok(CounterResult::from_entries(entries))
    }
}

struct CallbackContext {
    pid: u32,
    sink: Arc<dyn Sink>,
    qpc_frequency: u64,
    sources: Vec<ProfileSource>,
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let record = &*record;
    let provider = record.EventHeader.ProviderId;
    if provider.data1 != PerfInfoGuid.data1
        || provider.data2 != PerfInfoGuid.data2
        || provider.data3 != PerfInfoGuid.data3
        || provider.data4 != PerfInfoGuid.data4
    {
        return;
    }
    let context = &*(record.UserContext as *const CallbackContext);
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    if opcode != 46 && opcode != 47 {
        return;
    }
    let pointer_size = if record.EventHeader.Flags as u32 & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    };
    if record.UserData.is_null()
        || (record.UserDataLength as usize) < pointer_size + 4 + if opcode == 47 { 2 } else { 0 }
    {
        return;
    }
    let bytes =
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize);
    let ip = if pointer_size == 4 {
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as u64
    } else {
        u64::from_le_bytes(bytes[0..8].try_into().unwrap())
    };
    let tid = u32::from_le_bytes(bytes[pointer_size..pointer_size + 4].try_into().unwrap());
    let (counter, value, event_id) = if opcode == 47 {
        let source = u16::from_le_bytes(
            bytes[pointer_size + 4..pointer_size + 6]
                .try_into()
                .unwrap(),
        ) as u32;
        let Some(source) = context.sources.iter().find(|item| item.source == source) else {
            return;
        };
        (
            source.counter.clone(),
            source.interval as u64,
            source.event_id,
        )
    } else {
        let Some(source) = context
            .sources
            .iter()
            .find(|item| item.counter == Counter::CpuClock)
        else {
            return;
        };
        (Counter::CpuClock, 1, source.event_id)
    };
    let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
    if thread.is_null() {
        return;
    }
    let pid = GetProcessIdOfThread(thread);
    CloseHandle(thread);
    if pid != context.pid {
        return;
    }

    let mut callstack = SmallVec::new();
    if !record.ExtendedData.is_null() {
        for item in
            std::slice::from_raw_parts(record.ExtendedData, record.ExtendedDataCount as usize)
        {
            let width = match item.ExtType as u32 {
                EVENT_HEADER_EXT_TYPE_STACK_TRACE64 => 8,
                EVENT_HEADER_EXT_TYPE_STACK_TRACE32 => 4,
                _ => continue,
            };
            if item.DataPtr == 0 || item.DataSize < 8 {
                continue;
            }
            let data =
                std::slice::from_raw_parts(item.DataPtr as *const u8, item.DataSize as usize);
            for address in data[8..].chunks_exact(width).take(256) {
                let ip = if width == 8 {
                    u64::from_le_bytes(address.try_into().unwrap())
                } else {
                    u32::from_le_bytes(address.try_into().unwrap()) as u64
                };
                if ip != 0 {
                    callstack.push(ip);
                }
            }
        }
    }
    let qpc = record.EventHeader.TimeStamp.max(0) as u128;
    let time = (qpc.saturating_mul(1_000_000_000) / context.qpc_frequency as u128)
        .min(u64::MAX as u128) as u64;
    let cpu = unsafe { record.BufferContext.Anonymous.ProcessorIndex } as u32;
    context.sink.record(Record::Sample(Sample {
        event_id,
        ip,
        pid,
        tid,
        cpu,
        core: None,
        time,
        time_enabled: 0,
        time_running: 0,
        counter,
        value,
        callstack,
        lbr_callstack: SmallVec::new(),
        user_regs: None,
        user_stack: Vec::new(),
    }));
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn properties(name: &[u16], timer: bool) -> Vec<u64> {
    let bytes = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
    let mut storage = vec![0_u64; (bytes + 7) / 8];
    let properties = unsafe { &mut *(storage.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES) };
    properties.Wnode.BufferSize = bytes as u32;
    properties.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    properties.Wnode.Guid = GUID::from_u128(uuid::Uuid::now_v7().as_u128());
    properties.Wnode.ClientContext = 1; // QPC timestamps.
    properties.BufferSize = 64;
    properties.MinimumBuffers = 4;
    properties.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
    properties.EnableFlags = if timer { EVENT_TRACE_FLAG_PROFILE } else { 0 };
    properties.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    unsafe {
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            (storage.as_mut_ptr() as *mut u8).add(properties.LoggerNameOffset as usize) as *mut u16,
            name.len(),
        );
    }
    storage
}

// Thread CPU time is available without the system-profile privilege required
// by ETW. The creation timestamp prevents a recycled TID from inheriting the
// preceding thread's CPU-time baseline.
fn thread_cpu_times(pid: u32) -> io::Result<HashMap<(u32, u64), u64>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut times = HashMap::new();
    let mut entry = THREADENTRY32::default();
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    if unsafe { Thread32First(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32OwnerProcessID == pid {
                let handle =
                    unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID) };
                if !handle.is_null() {
                    let (mut creation, mut exit, mut kernel, mut user) =
                        unsafe { std::mem::zeroed() };
                    if unsafe {
                        GetThreadTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
                    } != 0
                    {
                        times.insert(
                            (entry.th32ThreadID, filetime_ticks(creation)),
                            filetime_ticks(kernel).saturating_add(filetime_ticks(user)),
                        );
                    }
                    unsafe { CloseHandle(handle) };
                }
            }
            if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
    Ok(times)
}

// The matching ResumeThread must happen even when reading the context fails.
// Only Windows calls and stack-local work belong between suspend and resume:
// a target thread may hold a lock needed by allocation or by the sink.
struct SuspendedThread(HANDLE);

// The native CONTEXT buffer needs 16-byte alignment on x64, which the
// windows-sys struct type does not express.
#[repr(align(16))]
struct AlignedContext(CONTEXT);

impl Drop for SuspendedThread {
    fn drop(&mut self) {
        unsafe { ResumeThread(self.0) };
    }
}

#[cfg(target_arch = "x86_64")]
fn target_is_wow64(pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut wow64 = 0;
    let result = unsafe { IsWow64Process(process, &mut wow64) } != 0 && wow64 != 0;
    unsafe { CloseHandle(process) };
    result
}

fn sampled_thread_ip(
    pid: u32,
    tid: u32,
    creation: u64,
    #[cfg(target_arch = "x86_64")] wow64: bool,
) -> u64 {
    // A new thread could reuse a TID after thread_cpu_times enumerated it.
    // Confirm the creation time on this handle before taking its context.
    let handle = unsafe {
        OpenThread(
            THREAD_QUERY_LIMITED_INFORMATION | THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME,
            0,
            tid,
        )
    };
    if handle.is_null() {
        return 0;
    }
    let ip = (|| {
        let (mut created, mut exit, mut kernel, mut user) = unsafe { std::mem::zeroed() };
        if unsafe { GetProcessIdOfThread(handle) } != pid
            || unsafe { GetThreadTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) }
                == 0
            || filetime_ticks(created) != creation
        {
            return 0;
        }
        #[cfg(target_arch = "x86_64")]
        let suspended = if wow64 {
            unsafe { Wow64SuspendThread(handle) }
        } else {
            unsafe { SuspendThread(handle) }
        };
        #[cfg(not(target_arch = "x86_64"))]
        let suspended = unsafe { SuspendThread(handle) };
        if suspended == u32::MAX {
            return 0;
        }
        let _resume = SuspendedThread(handle);
        // An already suspended thread cannot have consumed the interval's
        // recent CPU time at this instruction. Avoid attributing it to a
        // possibly stale context while preserving its original suspend count.
        if suspended != 0 {
            return 0;
        }
        #[cfg(target_arch = "x86_64")]
        if wow64 {
            let mut context = WOW64_CONTEXT::default();
            context.ContextFlags = WOW64_CONTEXT_CONTROL;
            return if unsafe { Wow64GetThreadContext(handle, &mut context) } != 0 {
                context.Eip as u64
            } else {
                0
            };
        }
        let mut context = AlignedContext(CONTEXT::default());
        #[cfg(target_arch = "x86_64")]
        {
            context.0.ContextFlags =
                windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_AMD64;
        }
        #[cfg(target_arch = "x86")]
        {
            context.0.ContextFlags =
                windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_X86;
        }
        #[cfg(target_arch = "aarch64")]
        {
            context.0.ContextFlags =
                windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_ARM64;
        }
        if unsafe { GetThreadContext(handle, &mut context.0) } == 0 {
            return 0;
        }
        #[cfg(target_arch = "x86_64")]
        {
            context.0.Rip
        }
        #[cfg(target_arch = "x86")]
        {
            context.0.Eip as u64
        }
        #[cfg(target_arch = "aarch64")]
        {
            context.0.Pc
        }
    })();
    unsafe { CloseHandle(handle) };
    ip
}

fn timer_samples(
    pid: u32,
    source: ProfileSource,
    sink: Arc<dyn Sink>,
    frequency: u64,
    interval: Duration,
    stop: mpsc::Receiver<()>,
    mut previous: HashMap<(u32, u64), u64>,
) {
    #[cfg(target_arch = "x86_64")]
    let wow64 = target_is_wow64(pid);
    while matches!(
        stop.recv_timeout(interval),
        Err(mpsc::RecvTimeoutError::Timeout)
    ) {
        let Ok(current) = thread_cpu_times(pid) else {
            continue;
        };
        let mut qpc = 0_i64;
        if unsafe { QueryPerformanceCounter(&mut qpc) } == 0 {
            previous = current;
            continue;
        }
        let time = ((qpc.max(0) as u128).saturating_mul(1_000_000_000) / frequency as u128)
            .min(u64::MAX as u128) as u64;
        for (&(tid, creation), &cpu_time) in &current {
            let Some(&before) = previous.get(&(tid, creation)) else {
                continue;
            };
            let delta = cpu_time.saturating_sub(before).saturating_mul(100);
            if delta == 0 {
                continue;
            }
            // CPU time belongs to the entire interval. The current RIP is an
            // approximate location for that work, not an interrupt sample.
            // Suspending any thread in our own process could freeze one that
            // holds a sink or allocator lock needed by this worker.
            let ip = if pid == unsafe { GetCurrentProcessId() } {
                0
            } else {
                sampled_thread_ip(
                    pid,
                    tid,
                    creation,
                    #[cfg(target_arch = "x86_64")]
                    wow64,
                )
            };
            sink.record(Record::Sample(Sample {
                event_id: source.event_id,
                ip,
                pid,
                tid,
                cpu: u32::MAX,
                core: None,
                time,
                time_enabled: 0,
                time_running: 0,
                counter: Counter::CpuClock,
                value: delta,
                callstack: SmallVec::new(),
                lbr_callstack: SmallVec::new(),
                user_regs: None,
                user_stack: Vec::new(),
            }));
        }
        previous = current;
    }
}

pub struct WindowsSamplingDriver {
    pid: u32,
    name: Vec<u16>,
    sources: Vec<ProfileSource>,
    controller: Option<CONTROLTRACE_HANDLE>,
    consumer: Option<PROCESSTRACE_HANDLE>,
    worker: Option<thread::JoinHandle<()>>,
    context: Option<Box<CallbackContext>>,
    sample_freq: u64,
    timer_stop: Option<mpsc::Sender<()>>,
    privilege: Option<ProfilePrivilege>,
}

impl WindowsSamplingDriver {
    pub fn new(counters: &[Counter], sample_freq: u64, pid: Option<i32>) -> Result<Self, Error> {
        if sample_freq == 0 {
            return Err(Error::InvalidConfiguration(
                "sample frequency must be positive".to_owned(),
            ));
        }
        // An overflow interval counts hardware events, while sample_freq is
        // measured in samples per second. Use the HAL's current interval;
        // converting Hz to an event period would require guessing the load.
        let available = if counters.iter().any(|counter| *counter != Counter::CpuClock)
            || counters.is_empty()
        {
            // A missing or inaccessible source list still permits CPU-clock
            // sampling. Requested hardware events are retained only if found.
            profile_sources().unwrap_or_default()
        } else {
            Vec::new()
        };
        let requested: Vec<Counter> = if counters.is_empty() {
            vec![Counter::CpuClock, Counter::Cycles]
        } else {
            let mut requested = counters.to_vec();
            if !requested.contains(&Counter::CpuClock) {
                requested.insert(0, Counter::CpuClock);
            }
            requested
        };
        let mut sources = Vec::new();
        for counter in requested {
            if sources
                .iter()
                .any(|source: &ProfileSource| source.counter == counter)
            {
                continue;
            }
            if counter == Counter::CpuClock {
                sources.push(ProfileSource {
                    counter,
                    name: "CPU clock".to_owned(),
                    source: 0,
                    interval: 1,
                    event_id: uuid::Uuid::now_v7().as_u128(),
                });
            } else if let Some(source) = selected_profile_source(&available, &counter) {
                if sources.iter().any(|existing| {
                    let existing: &ProfileSource = existing;
                    existing.source == source.source && existing.counter != Counter::CpuClock
                }) {
                    return Err(Error::InvalidConfiguration(
                        "the requested Windows counters include two names for one ETW profile source"
                            .to_owned(),
                    ));
                }
                sources.push(source);
            }
        }
        let pid = pid
            .map(|p| p as u32)
            .unwrap_or_else(|| unsafe { GetCurrentProcessId() });
        let name = wide(&format!("miniperf-{}-{}", pid, uuid::Uuid::now_v7()));
        Ok(Self {
            pid,
            name,
            sources,
            controller: None,
            consumer: None,
            worker: None,
            context: None,
            sample_freq,
            timer_stop: None,
            privilege: None,
        })
    }

    fn start_timer(&mut self, sink: Arc<dyn Sink>, frequency: u64) -> Result<(), Error> {
        let source = self
            .sources
            .iter()
            .find(|source| source.counter == Counter::CpuClock)
            .cloned()
            .expect("CPU clock source is always present");
        let previous = thread_cpu_times(self.pid).map_err(|error| {
            Error::InvalidConfiguration(format!("cannot enumerate Windows threads: {error}"))
        })?;
        // Only CPU-clock data will be emitted by this worker. Keep the list
        // honest for callers that inspect it after start.
        self.sources
            .retain(|item| item.counter == Counter::CpuClock);
        let interval = Duration::from_nanos((1_000_000_000 / self.sample_freq).max(1));
        let (sender, receiver) = mpsc::channel();
        let pid = self.pid;
        self.worker = Some(thread::spawn(move || {
            timer_samples(pid, source, sink, frequency, interval, receiver, previous);
        }));
        self.timer_stop = Some(sender);
        Ok(())
    }
}

impl SamplingDriver for WindowsSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        self.sources
            .iter()
            .map(|source| source.counter.clone())
            .collect()
    }

    fn start(&mut self, sink: Arc<dyn Sink>) -> Result<(), Error> {
        if self.controller.is_some() || self.worker.is_some() {
            return Err(Error::InvalidConfiguration(
                "ETW sampler already started".to_owned(),
            ));
        }
        let mut freq = 0_i64;
        if unsafe { QueryPerformanceFrequency(&mut freq) } == 0 || freq <= 0 {
            return Err(Error::InvalidConfiguration(
                "QPC frequency is unavailable".to_owned(),
            ));
        }
        let timer = self
            .sources
            .iter()
            .any(|source| source.counter == Counter::CpuClock);
        let pmu: Vec<_> = self
            .sources
            .iter()
            .filter(|source| source.counter != Counter::CpuClock)
            .collect();
        let mut storage = properties(&self.name, timer);
        let props = storage.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        let mut controller = CONTROLTRACE_HANDLE::default();
        self.privilege = ProfilePrivilege::enable()?;
        let status = unsafe { StartTraceW(&mut controller, self.name.as_ptr(), props) };
        if status != 0 {
            self.privilege = None;
            if status == ERROR_ACCESS_DENIED || status == ERROR_PRIVILEGE_NOT_HELD {
                return self.start_timer(sink, freq as u64);
            }
            return Err(windows_error(
                "StartTraceW (system profile privilege may be required)",
                status,
            ));
        }
        self.controller = Some(controller);

        let stack_events: Vec<CLASSIC_EVENT_ID> = [46_u8, 47_u8]
            .into_iter()
            .filter(|type_| if *type_ == 46 { timer } else { !pmu.is_empty() })
            .map(|type_| CLASSIC_EVENT_ID {
                EventGuid: PerfInfoGuid,
                Type: type_,
                Reserved: [0; 7],
            })
            .collect();
        let _ = unsafe {
            TraceSetInformation(
                controller,
                TraceStackTracingInfo,
                stack_events.as_ptr() as *const c_void,
                (stack_events.len() * size_of::<CLASSIC_EVENT_ID>()) as u32,
            )
        };
        if !pmu.is_empty() {
            let ids: Vec<u32> = pmu.iter().map(|source| source.source).collect();
            // ETW's sampled-source configuration is machine wide. Failure is
            // surfaced instead of silently producing timer-only samples.
            let status = unsafe {
                TraceSetInformation(
                    CONTROLTRACE_HANDLE::default(),
                    TraceProfileSourceConfigInfo,
                    ids.as_ptr() as *const c_void,
                    (ids.len() * size_of::<u32>()) as u32,
                )
            };
            if status != 0 {
                self.stop()?;
                return Err(windows_error(
                    "TraceSetInformation(PMU profile sources)",
                    status,
                ));
            }
        }
        // PERF_PMC_PROFILE = 0x20000400: mask group 1, bit 0x400.
        let flags = [
            if timer { EVENT_TRACE_FLAG_PROFILE } else { 0 },
            if pmu.is_empty() { 0 } else { 0x400 },
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let enabled = unsafe {
            TraceSetInformation(
                controller,
                TraceSystemTraceEnableFlagsInfo,
                flags.as_ptr() as *const c_void,
                size_of::<[u32; 8]>() as u32,
            )
        };
        if enabled != 0 {
            self.stop()?;
            return Err(windows_error("TraceSetInformation(profile)", enabled));
        }

        let mut context = Box::new(CallbackContext {
            pid: self.pid,
            sink,
            qpc_frequency: freq as u64,
            sources: self.sources.clone(),
        });
        let mut logfile = EVENT_TRACE_LOGFILEW::default();
        logfile.LoggerName = self.name.as_mut_ptr();
        logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD
            | PROCESS_TRACE_MODE_REAL_TIME
            | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);
        logfile.Context = &mut *context as *mut CallbackContext as *mut c_void;
        let consumer = unsafe { OpenTraceW(&mut logfile) };
        if consumer.Value == u64::MAX {
            self.stop()?;
            return Err(Error::InvalidConfiguration(format!(
                "OpenTraceW failed: {}",
                io::Error::last_os_error()
            )));
        }
        self.context = Some(context);
        self.consumer = Some(consumer);
        self.worker = Some(thread::spawn(move || unsafe {
            ProcessTrace(&consumer, 1, ptr::null(), ptr::null());
            CloseTrace(consumer);
        }));
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        if let Some(sender) = self.timer_stop.take() {
            let _ = sender.send(());
            if let Some(worker) = self.worker.take() {
                worker.join().map_err(|_| Error::WorkerPanicked)?;
            }
        }
        if let Some(controller) = self.controller.take() {
            let mut storage = properties(
                &self.name,
                self.sources
                    .iter()
                    .any(|source| source.counter == Counter::CpuClock),
            );
            let status = unsafe {
                ControlTraceW(
                    controller,
                    self.name.as_ptr(),
                    storage.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            if status != 0 {
                return Err(windows_error("ControlTraceW", status));
            }
            if let Some(worker) = self.worker.take() {
                worker.join().map_err(|_| Error::WorkerPanicked)?;
            }
            self.consumer = None;
            self.context = None;
        }
        self.privilege = None;
        Ok(())
    }
}

impl Drop for WindowsSamplingDriver {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        countable_profile_source, fixed_tma_source_name, profile_counter, selected_profile_source,
        source_matches_counter, ProfileSource,
    };
    use crate::Counter;

    fn source(name: &str, counter: Counter) -> ProfileSource {
        ProfileSource {
            counter,
            name: name.to_owned(),
            source: 1,
            interval: 100,
            event_id: 1,
        }
    }

    #[test]
    fn generic_profile_aliases_keep_the_existing_mapping() {
        assert_eq!(profile_counter("CPU Cycles"), Some(Counter::Cycles));
        assert_eq!(
            profile_counter("Retired Instructions"),
            Some(Counter::Instructions)
        );
        assert_eq!(profile_counter("Mystery Event"), None);
    }

    #[test]
    fn custom_profile_source_matches_its_name_without_fuzzy_normalization() {
        let profile_source = source("Branch Mispredictions", Counter::BranchMisses);
        assert!(source_matches_counter(
            &profile_source,
            &Counter::Custom("branch mispredictions".to_owned())
        ));
        assert!(!source_matches_counter(
            &profile_source,
            &Counter::Custom("BranchMispredictions".to_owned())
        ));
        assert!(!source_matches_counter(
            &profile_source,
            &Counter::Custom("Branch Mispredicts".to_owned())
        ));
        assert!(source_matches_counter(
            &profile_source,
            &Counter::BranchMisses
        ));
    }

    #[test]
    fn selected_custom_source_retains_the_requested_counter_name() {
        let mut alias = source("UnhaltedCoreCycles", Counter::Cycles);
        alias.source = 2;
        let request = Counter::Custom("unhaltedcorecycles".to_owned());
        let selected =
            selected_profile_source(&[source("TotalCycles", Counter::Cycles), alias], &request)
                .expect("the second HAL alias must be retained");
        assert_eq!(selected.source, 2);
        assert_eq!(selected.counter, request);
    }

    #[test]
    fn timer_profiles_are_not_advertised_as_hardware_counters() {
        assert!(!countable_profile_source("Timer", 0));
        assert!(!countable_profile_source("TimerFixed", 36));
        assert!(countable_profile_source("InstructionRetired", 26));
    }

    #[test]
    fn tma_prefers_exposed_fixed_counter_sources() {
        let sources = [
            source("TotalCycles", Counter::Cycles),
            source("InstructionRetired", Counter::Instructions),
            source(
                "UnhaltedCoreCyclesFixed",
                Counter::Custom("fixed cycles".into()),
            ),
            source(
                "InstructionsRetiredFixed",
                Counter::Custom("fixed instructions".into()),
            ),
        ];
        assert_eq!(
            fixed_tma_source_name(&sources, &Counter::Cycles).as_deref(),
            Some("UnhaltedCoreCyclesFixed")
        );
        assert_eq!(
            fixed_tma_source_name(&sources, &Counter::Instructions).as_deref(),
            Some("InstructionsRetiredFixed")
        );
    }
}
