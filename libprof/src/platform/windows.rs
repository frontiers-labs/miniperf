//! Windows implementations of the host facilities in [`super`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::{size_of, zeroed};

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
    MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, GetProcessIoCounters, GetProcessTimes, OpenProcess, WaitForSingleObject,
    IO_COUNTERS, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

use super::{ProcessContext, ProcessIo, ProcessStat};
use crate::sink::ProcAddr;

struct Handle(windows_sys::Win32::Foundation::HANDLE);

impl Handle {
    fn open(pid: u32) -> Option<Self> {
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn configured_cpu_count() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()
        .and_then(|count| u32::try_from(count.get()).ok())
}

pub(super) fn monotonic_timestamp_ns() -> std::io::Result<u64> {
    let mut count = 0_i64;
    let mut frequency = 0_i64;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { QueryPerformanceCounter(&mut count) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(((count as u128) * 1_000_000_000 / frequency as u128) as u64)
}

pub(super) fn ticks_per_second() -> f64 {
    10_000_000.0
}

pub(super) fn process_tree(root_pid: u32) -> Option<Vec<ProcessStat>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let snapshot = Handle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut all = HashMap::<u32, ProcessStat>::new();
    let mut ok = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while ok != 0 {
        let pid = entry.th32ProcessID;
        let mut stat = process_stat(pid);
        stat.ppid = entry.th32ParentProcessID;
        stat.command = wide_string(&entry.szExeFile);
        all.insert(pid, stat);
        ok = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    if !all.contains_key(&root_pid) {
        return Some(Vec::new());
    }
    let mut children = HashMap::<u32, Vec<u32>>::new();
    for stat in all.values() {
        children.entry(stat.ppid).or_default().push(stat.pid);
    }
    let mut result = Vec::new();
    let mut queue = VecDeque::from([root_pid]);
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(stat) = all.get(&pid) {
            result.push(stat.clone());
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    Some(result)
}

pub(super) fn process_tree_supported() -> bool {
    true
}

fn process_stat(pid: u32) -> ProcessStat {
    let mut stat = ProcessStat {
        pid,
        ..Default::default()
    };
    let Some(handle) = Handle::open(pid) else {
        return stat;
    };
    stat.state = if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
        b'Z'
    } else {
        b'R'
    };
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { zeroed() };
    if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } != 0
    {
        stat.start_ticks = filetime_ticks(created);
        stat.user_ticks = filetime_ticks(user);
        stat.system_ticks = filetime_ticks(kernel);
    }
    if let Some(memory) = process_memory(pid) {
        // Windows reports an aggregate page-fault count, without a
        // minor/major split. Attribute it to the reported minor series.
        stat.minor_faults = memory.PageFaultCount as u64;
        stat.rss_pages = (memory.WorkingSetSize as u64 / page_size() as u64) as i64;
    }
    stat
}

fn process_memory(pid: u32) -> Option<PROCESS_MEMORY_COUNTERS> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let handle = Handle(handle);
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
    (unsafe {
        GetProcessMemoryInfo(
            handle.0,
            &mut memory,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } != 0)
        .then_some(memory)
}

pub(super) fn process_rss_bytes(pid: u32) -> Option<u64> {
    process_memory(pid).map(|memory| memory.WorkingSetSize as u64)
}

fn filetime_ticks(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

pub(super) fn process_io(pid: u32) -> ProcessIo {
    let Some(handle) = Handle::open(pid) else {
        return ProcessIo::default();
    };
    let mut counters: IO_COUNTERS = unsafe { zeroed() };
    if unsafe { GetProcessIoCounters(handle.0, &mut counters) } == 0 {
        return ProcessIo::default();
    }
    ProcessIo {
        read_bytes: counters.ReadTransferCount,
        write_bytes: counters.WriteTransferCount,
        read_calls: counters.ReadOperationCount,
        write_calls: counters.WriteOperationCount,
    }
}

pub(super) fn process_context(_pid: u32) -> ProcessContext {
    ProcessContext::default()
}

pub(super) fn process_alive(pid: u32) -> bool {
    let Some(handle) = Handle::open(pid) else {
        return false;
    };
    unsafe { WaitForSingleObject(handle.0, 0) == WAIT_TIMEOUT }
}

pub(super) fn process_modules(pid: u32) -> Vec<ProcAddr> {
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Handle(snapshot);
    let mut entry: MODULEENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    let mut modules = Vec::new();
    let mut ok = unsafe { Module32FirstW(snapshot.0, &mut entry) };
    while ok != 0 {
        modules.push(ProcAddr {
            pid,
            addr: entry.modBaseAddr as u64,
            len: entry.modBaseSize as u64,
            pgoff: 0,
            filename: wide_string(&entry.szExePath),
        });
        ok = unsafe { Module32NextW(snapshot.0, &mut entry) };
    }
    modules
}

pub(super) fn current_thread_id() -> u64 {
    unsafe { GetCurrentThreadId() as u64 }
}

pub(super) fn page_size() -> f64 {
    let mut info: SYSTEM_INFO = unsafe { zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize.max(1) as f64
}

fn wide_string(value: &[u16]) -> String {
    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}
