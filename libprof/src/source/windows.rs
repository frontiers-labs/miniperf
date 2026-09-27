//! Windows resource accounting using documented kernel process APIs.
//!
//! Process accounting is exact for the attached/launched root process. Windows
//! does not provide a stable, permission-free descendant accounting API, so the
//! source labels its scope explicitly and reports descendant coverage as
//! unavailable.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{resource_sample, Availability, SessionContext, Source, SourceDecl};
use crate::{ProcessInfo, Record, Sink, SourceStatus};

const INTERVAL: Duration = Duration::from_secs(1);

use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_ROW2};

/// Collects root-process, system-network, disk, and exposed thermal-zone metrics on Windows.

#[derive(Default)]
pub struct WindowsResourceSource {
    stop: Option<Arc<AtomicBool>>,
    worker: Option<thread::JoinHandle<Vec<SourceStatus>>>,
}

impl Source for WindowsResourceSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "windows_resources",
        }
    }
    fn probe(&self, _directory: &Path) -> Availability {
        Availability::Available
    }
    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()> {
        let pid = context.root_pid();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let sink = context.sink.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("libprof-windows-resources".into())
                .spawn(move || collect(sink, pid, worker_stop))?,
        );
        self.stop = Some(stop);
        Ok(())
    }
    fn stop(&mut self, _context: &SessionContext) -> Vec<SourceStatus> {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            worker.join().unwrap_or_else(|_| {
                vec![SourceStatus::new(
                    "windows_resources",
                    "error",
                    "internal",
                    "unavailable",
                    "Windows resource collector thread did not shut down cleanly",
                )]
            })
        } else {
            Vec::new()
        }
    }
}

fn collect(sink: Arc<dyn Sink>, pid: u32, stop: Arc<AtomicBool>) -> Vec<SourceStatus> {
    {
        use std::mem::size_of;
        use windows_sys::Win32::System::Performance::{
            PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
            PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
            PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER,
            PDH_HQUERY,
        };
        fn wide(value: &str) -> Vec<u16> {
            value.encode_utf16().chain(std::iter::once(0)).collect()
        }
        unsafe fn add_counter(query: PDH_HQUERY, path: &str) -> Option<PDH_HCOUNTER> {
            let path = wide(path);
            let mut counter = std::ptr::null_mut();
            (PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) == 0).then_some(counter)
        }
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct IoCounters {
            read_ops: u64,
            write_ops: u64,
            other_ops: u64,
            read_bytes: u64,
            write_bytes: u64,
            other_bytes: u64,
        }
        #[repr(C)]
        #[derive(Default)]
        struct MemoryCounters {
            cb: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged: usize,
            quota_paged: usize,
            quota_peak_nonpaged: usize,
            quota_nonpaged: usize,
            pagefile: usize,
            pagefile_peak: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
            fn GetProcessTimes(
                handle: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
            fn GetProcessIoCounters(
                handle: *mut std::ffi::c_void,
                counters: *mut IoCounters,
            ) -> i32;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(
                handle: *mut std::ffi::c_void,
                counters: *mut MemoryCounters,
                size: u32,
            ) -> i32;
        }
        fn filetime(t: FileTime) -> u64 {
            ((t.high as u64) << 32) | t.low as u64
        }
        let handle = unsafe { OpenProcess(0x1000, 0, pid) };
        if handle.is_null() {
            return vec![SourceStatus::new(
                "process_resources",
                "permission_denied",
                "windows_process_api",
                "unavailable",
                "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) failed",
            )];
        }
        let start = Instant::now();
        let mut query: PDH_HQUERY = std::ptr::null_mut();
        let query_ready = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } == 0;
        let (disk_read, disk_write, disk_reads, disk_writes, thermal) = if query_ready {
            unsafe {
                (
                    add_counter(query, r"\PhysicalDisk(_Total)\Disk Read Bytes/sec"),
                    add_counter(query, r"\PhysicalDisk(_Total)\Disk Write Bytes/sec"),
                    add_counter(query, r"\PhysicalDisk(_Total)\Disk Reads/sec"),
                    add_counter(query, r"\PhysicalDisk(_Total)\Disk Writes/sec"),
                    add_counter(query, r"\Thermal Zone Information(*)\Temperature"),
                )
            }
        } else {
            (None, None, None, None, None)
        };
        let mut have_disk = false;
        let mut have_thermal = false;
        let mut previous: Option<(u64, Option<IoCounters>, Option<u32>)> = None;
        let mut processes = HashMap::<(u32, u64), ProcessInfo>::new();
        loop {
            let timestamp = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            for stat in crate::platform::process_tree(pid).unwrap_or_default() {
                let process = processes
                    .entry((stat.pid, stat.start_ticks))
                    .or_insert_with(|| ProcessInfo {
                        pid: stat.pid,
                        ppid: stat.ppid,
                        start_ticks: stat.start_ticks,
                        first_seen_ns: timestamp,
                        last_seen_ns: timestamp,
                        command: stat.command.clone(),
                        quality: "windows_toolhelp_best_effort".to_owned(),
                    });
                process.last_seen_ns = timestamp;
            }
            if query_ready && unsafe { PdhCollectQueryData(query) } == 0 {
                for (counter, metric, unit) in [
                    (disk_read, "read_bytes_per_second", "bytes_per_second"),
                    (disk_write, "write_bytes_per_second", "bytes_per_second"),
                    (
                        disk_reads,
                        "read_operations_per_second",
                        "operations_per_second",
                    ),
                    (
                        disk_writes,
                        "write_operations_per_second",
                        "operations_per_second",
                    ),
                ] {
                    if let Some(counter) = counter {
                        let mut value = PDH_FMT_COUNTERVALUE::default();
                        if unsafe {
                            PdhGetFormattedCounterValue(
                                counter,
                                PDH_FMT_DOUBLE,
                                std::ptr::null_mut(),
                                &mut value,
                            )
                        } == 0
                            && matches!(value.CStatus, 0 | 1)
                        {
                            let number = unsafe { value.Anonymous.doubleValue };
                            if number.is_finite() {
                                have_disk = true;
                                sink.record(Record::Resource(resource_sample(
                                    timestamp,
                                    "disk",
                                    "system_physical_disks",
                                    "utilization",
                                    metric,
                                    number,
                                    unit,
                                    "system_during_target",
                                    "windows_pdh_english",
                                    "exact_system",
                                )));
                            }
                        }
                    }
                }
                if let Some(counter) = thermal {
                    let mut size = 0u32;
                    let mut count = 0u32;
                    let first = unsafe {
                        PdhGetFormattedCounterArrayW(
                            counter,
                            PDH_FMT_DOUBLE,
                            &mut size,
                            &mut count,
                            std::ptr::null_mut(),
                        )
                    };
                    if first == 0x800007D2 && size > 0 {
                        let mut storage = vec![0u64; (size as usize + 7) / 8];
                        let result = unsafe {
                            PdhGetFormattedCounterArrayW(
                                counter,
                                PDH_FMT_DOUBLE,
                                &mut size,
                                &mut count,
                                storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                            )
                        };
                        if result == 0 {
                            let items = storage.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
                            for item in unsafe { std::slice::from_raw_parts(items, count as usize) }
                            {
                                if !matches!(item.FmtValue.CStatus, 0 | 1) || item.szName.is_null()
                                {
                                    continue;
                                }
                                let mut len = 0usize;
                                while unsafe { *item.szName.add(len) } != 0 {
                                    len += 1;
                                }
                                let name = String::from_utf16_lossy(unsafe {
                                    std::slice::from_raw_parts(item.szName, len)
                                });
                                let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                                if value.is_finite() {
                                    have_thermal = true;
                                    sink.record(Record::Resource(resource_sample(
                                        timestamp,
                                        "thermal",
                                        &name,
                                        "temperature",
                                        "temperature",
                                        value,
                                        "kelvin",
                                        "system_thermal_zone",
                                        "windows_pdh_english",
                                        "exact_system_thermal_zone",
                                    )));
                                }
                            }
                        }
                    }
                }
            }
            let (mut creation, mut exit, mut kernel, mut user) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            let mut io = IoCounters::default();
            let mut memory = MemoryCounters {
                cb: size_of::<MemoryCounters>() as u32,
                ..Default::default()
            };
            let ok = unsafe {
                GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
            } != 0;
            if !ok {
                break;
            }
            let io_ok = unsafe { GetProcessIoCounters(handle, &mut io) } != 0;
            let memory_ok = unsafe {
                GetProcessMemoryInfo(handle, &mut memory, size_of::<MemoryCounters>() as u32)
            } != 0;
            let cpu = filetime(user).saturating_add(filetime(kernel));
            if let Some((old_cpu, old_io, old_faults)) = previous {
                sink.record(Record::Resource(resource_sample(
                    timestamp,
                    "cpu",
                    "root_process",
                    "utilization",
                    "process_cpu_time",
                    cpu.saturating_sub(old_cpu) as f64 / 10_000_000.0,
                    "seconds",
                    "root_process",
                    "windows_process_api",
                    "root_process_only",
                )));
                if let (Some(old_io), true) = (old_io, io_ok) {
                    for (metric, value, unit) in [
                        (
                            "read_bytes",
                            io.read_bytes.saturating_sub(old_io.read_bytes) as f64,
                            "bytes",
                        ),
                        (
                            "write_bytes",
                            io.write_bytes.saturating_sub(old_io.write_bytes) as f64,
                            "bytes",
                        ),
                        (
                            "read_calls",
                            io.read_ops.saturating_sub(old_io.read_ops) as f64,
                            "operations",
                        ),
                        (
                            "write_calls",
                            io.write_ops.saturating_sub(old_io.write_ops) as f64,
                            "operations",
                        ),
                    ] {
                        sink.record(Record::Resource(resource_sample(
                            timestamp,
                            "io",
                            "root_process",
                            "utilization",
                            metric,
                            value,
                            unit,
                            "root_process",
                            "windows_process_api",
                            "root_process_only",
                        )));
                    }
                }
                if let (Some(before), true) = (old_faults, memory_ok) {
                    sink.record(Record::Resource(resource_sample(
                        timestamp,
                        "memory",
                        "root_process",
                        "errors",
                        "page_faults",
                        memory.page_faults.saturating_sub(before) as f64,
                        "events",
                        "root_process",
                        "windows_process_api",
                        "root_process_only",
                    )));
                }
            }
            if memory_ok {
                sink.record(Record::Resource(resource_sample(
                    timestamp,
                    "memory",
                    "root_process",
                    "utilization",
                    "working_set",
                    memory.working_set as f64,
                    "bytes",
                    "root_process",
                    "windows_process_api",
                    "root_process_only",
                )));
            }
            let mut table = std::ptr::null_mut();
            if unsafe { GetIfTable2(&mut table) } == 0 && !table.is_null() {
                let count = unsafe { (*table).NumEntries as usize };
                let rows = unsafe { std::ptr::addr_of!((*table).Table).cast::<MIB_IF_ROW2>() };
                for i in 0..count {
                    let row = unsafe { &*rows.add(i) };
                    let id = format!("if{}", row.InterfaceIndex);
                    for (metric, value, category, unit) in [
                        ("receive_bytes", row.InOctets as f64, "utilization", "bytes"),
                        ("receive_errors", row.InErrors as f64, "errors", "events"),
                        ("receive_drops", row.InDiscards as f64, "errors", "events"),
                        (
                            "transmit_bytes",
                            row.OutOctets as f64,
                            "utilization",
                            "bytes",
                        ),
                        ("transmit_errors", row.OutErrors as f64, "errors", "events"),
                        ("transmit_drops", row.OutDiscards as f64, "errors", "events"),
                    ] {
                        sink.record(Record::Resource(resource_sample(
                            timestamp,
                            "network",
                            &id,
                            category,
                            metric,
                            value,
                            unit,
                            "system_during_target",
                            "windows_iphlpapi",
                            "exact_system",
                        )));
                    }
                    if row.ReceiveLinkSpeed > 0 || row.TransmitLinkSpeed > 0 {
                        sink.record(Record::Resource(resource_sample(
                            timestamp,
                            "network",
                            &id,
                            "utilization",
                            "link_capacity",
                            row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed) as f64,
                            "bits_per_second",
                            "system_during_target",
                            "windows_iphlpapi",
                            "exact_system",
                        )));
                    }
                }
                unsafe {
                    FreeMibTable(table.cast());
                }
            }
            previous = Some((
                cpu,
                io_ok.then_some(io),
                memory_ok.then_some(memory.page_faults),
            ));
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::park_timeout(INTERVAL);
        }
        unsafe {
            CloseHandle(handle);
            if query_ready {
                PdhCloseQuery(query);
            }
        }
        for process in processes.into_values() {
            sink.record(Record::Process(process));
        }
        let mut statuses = vec![SourceStatus::new(
            "process_resources",
            "degraded",
            "windows_process_api",
            "root_process_only",
            "CPU, memory, and I/O totals cover the root process; descendant aggregation is unavailable",
        ), SourceStatus::new(
            "process_tree",
            "available",
            "windows_toolhelp",
            "best_effort",
            "Existing and future descendants are polled by PID and creation time",
        )];
        if !have_disk {
            statuses.push(SourceStatus::new(
                "host_disk_io",
                "unavailable",
                "windows_pdh_english",
                "unavailable",
                "Windows physical disk performance counters were not exposed by PDH",
            ));
        }
        if !have_thermal {
            statuses.push(SourceStatus::new(
                "host_thermal_zone",
                "unavailable",
                "windows_pdh_english",
                "unavailable",
                "No ACPI thermal-zone temperature counters were exposed by PDH",
            ));
        }
        statuses
    }
}
