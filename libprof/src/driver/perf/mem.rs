use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use libc::{close, mmap, munmap, sysconf, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};
use perf_event_open_sys as sys;
use perf_event_open_sys::bindings::{
    perf_event_attr, PERF_SAMPLE_ADDR, PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_CALLCHAIN,
    PERF_SAMPLE_CPU, PERF_SAMPLE_DATA_SRC, PERF_SAMPLE_ID, PERF_SAMPLE_IP, PERF_SAMPLE_PERIOD,
    PERF_SAMPLE_REGS_USER, PERF_SAMPLE_STACK_USER, PERF_SAMPLE_TID, PERF_SAMPLE_TIME,
    PERF_SAMPLE_WEIGHT_STRUCT,
};

use crate::driver::{SamplingDriver, Sink};
use crate::sink::{MemSample, ProcAddr, Record};
use crate::{Counter, Error};

use super::branch::BranchMode;
use super::mmap::{MmapRecord, Records};
use super::sysfs;
use super::{sampling_ring_pages, target_allowed_cpus, UnsafeMmap};

/// Cycle threshold below which the load-latency facility does not arm. `perf
/// mem` uses the same default; the sysfs alias advertises a far lower value
/// that floods the ring with L1 hits.
const DEFAULT_LOAD_LATENCY: u64 = 30;

/// Precise memory sampling (Intel PEBS `mem-loads`/`mem-stores`). Runs as its
/// own event set alongside the cycles-based sampling group, and reports
/// per-access data address, data source and latency.
pub struct PerfMemSamplingDriver {
    fds: Vec<i32>,
    mmaps: Vec<UnsafeMmap>,
    page_size: usize,
    mmap_pages: usize,
    running: Arc<AtomicBool>,
    lost_samples: Arc<AtomicU64>,
    thread_handle: Option<thread::JoinHandle<()>>,
    sample_regs_user: u64,
    branch_mode: Option<BranchMode>,
    events: Vec<String>,
}

unsafe impl Send for PerfMemSamplingDriver {}
unsafe impl Sync for PerfMemSamplingDriver {}

impl PerfMemSamplingDriver {
    /// Open `mem-loads` and `mem-stores` on every core PMU that exposes them,
    /// for each CPU the target may run on.
    pub fn new(
        pid: i32,
        sample_freq: u64,
        stack_dump_size: u32,
        dwarf_mask: u64,
        branch_records: bool,
    ) -> Result<PerfMemSamplingDriver, Error> {
        // Same authoritative probe as the counter groups: ask the branch
        // recorder for its best mode and let the kernel refuse.
        let modes: &[BranchMode] = if branch_records {
            &BranchMode::LADDER
        } else {
            &[]
        };
        let mut last = None;
        for mode in modes.iter().copied().map(Some).chain([None]) {
            match Self::open(pid, sample_freq, stack_dump_size, dwarf_mask, mode) {
                Ok(driver) => return Ok(driver),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::InvalidConfiguration("no precise memory event could be opened".to_owned())
        }))
    }

    fn open(
        pid: i32,
        sample_freq: u64,
        stack_dump_size: u32,
        dwarf_mask: u64,
        branch_mode: Option<BranchMode>,
    ) -> Result<PerfMemSamplingDriver, Error> {
        let cpus = target_allowed_cpus(pid)?;
        let mut fds = Vec::new();
        let mut events = Vec::new();

        for pmu in sysfs::core_pmu_paths() {
            for event in ["mem-loads", "mem-stores"] {
                let Some(mut attr) = sysfs::event_attr(&pmu, event) else {
                    continue;
                };
                // The alias advertises a token latency threshold; raise it so
                // the ring carries real stalls rather than every L1 hit.
                if sysfs::alias_has_term(&pmu, event, "ldlat") {
                    sysfs::set_format_field(&mut attr, &pmu, "ldlat", DEFAULT_LOAD_LATENCY);
                }
                apply_memory_sampling_flags(
                    &mut attr,
                    sample_freq,
                    stack_dump_size,
                    dwarf_mask,
                    branch_mode,
                );
                let opened = cpus
                    .iter()
                    .map(|&cpu| unsafe { sys::perf_event_open(&mut attr, pid, cpu, -1, 0) })
                    .collect::<Vec<_>>();
                if opened.iter().any(|fd| *fd < 0) {
                    for fd in opened.into_iter().filter(|fd| *fd >= 0) {
                        unsafe { close(fd) };
                    }
                    continue;
                }
                events.push(event.to_owned());
                fds.extend(opened);
            }
        }

        if fds.is_empty() {
            return Err(Error::InvalidConfiguration(
                "no precise memory event could be opened".to_owned(),
            ));
        }

        let page_size = unsafe { sysconf(libc::_SC_PAGE_SIZE) } as usize;
        let perf_mlock_kb = std::fs::read_to_string("/proc/sys/kernel/perf_event_mlock_kb")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok());
        let mmap_pages = sampling_ring_pages(page_size, perf_mlock_kb);
        let length = page_size * (mmap_pages + 1);

        let mut mmaps: Vec<UnsafeMmap> = Vec::with_capacity(fds.len());
        for &fd in &fds {
            let ptr = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    length,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    0,
                ) as *mut u8
            };
            if ptr as *mut libc::c_void == MAP_FAILED {
                let source = std::io::Error::last_os_error();
                for entry in &mmaps {
                    unsafe { munmap(entry.ptr.cast(), length) };
                }
                for fd in &fds {
                    unsafe { close(*fd) };
                }
                return Err(Error::PerfMmap {
                    counter: "mem-loads".to_owned(),
                    length,
                    source,
                });
            }
            mmaps.push(UnsafeMmap { ptr });
        }

        Ok(PerfMemSamplingDriver {
            fds,
            mmaps,
            page_size,
            mmap_pages,
            running: Arc::new(AtomicBool::new(false)),
            lost_samples: Arc::new(AtomicU64::new(0)),
            thread_handle: None,
            sample_regs_user: dwarf_mask,
            branch_mode,
            events,
        })
    }
}

impl SamplingDriver for PerfMemSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        self.events
            .iter()
            .map(|event| Counter::Custom(event.clone()))
            .collect()
    }

    fn start(&mut self, callback: Arc<dyn Sink>) -> Result<(), Error> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let lost_samples = self.lost_samples.clone();
        let mmaps = self.mmaps.clone();
        let sample_regs_user = self.sample_regs_user;
        let branch_mode = self.branch_mode;

        self.thread_handle = Some(thread::spawn(move || loop {
            for entry in &mmaps {
                for record in Records::memory(entry.ptr, sample_regs_user, branch_mode) {
                    match record {
                        MmapRecord::MemSample {
                            ip,
                            pid,
                            tid,
                            cpu,
                            time,
                            data_addr,
                            latency,
                            data_src,
                            callstack,
                            lbr_callstack,
                            user_regs,
                            user_stack,
                        } => callback.record(Record::MemSample(MemSample {
                            ip,
                            pid,
                            tid,
                            cpu,
                            time,
                            data_addr,
                            latency,
                            data_src,
                            callstack,
                            lbr_callstack,
                            user_regs,
                            user_stack,
                        })),
                        MmapRecord::Address {
                            pid,
                            start,
                            len,
                            offset,
                            filename,
                        } => callback.record(Record::ProcAddr(ProcAddr {
                            pid,
                            addr: start,
                            len,
                            pgoff: offset,
                            filename,
                        })),
                        MmapRecord::Lost { count } => {
                            lost_samples.fetch_add(count, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            }

            if !running.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_micros(100));
        }));

        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        for &fd in &self.fds {
            unsafe { sys::ioctls::DISABLE(fd, 0) };
        }
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| Error::WorkerPanicked)?;
        }
        let lost = self.lost_samples.load(Ordering::Relaxed);
        if lost != 0 {
            return Err(Error::SamplesLost { count: lost });
        }
        Ok(())
    }
}

impl Drop for PerfMemSamplingDriver {
    fn drop(&mut self) {
        // The worker reads the rings through raw pointers. A driver dropped
        // without `stop()`, as when a later source fails to start, must not
        // unmap them under it.
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        let length = self.page_size * (self.mmap_pages + 1);
        for entry in &self.mmaps {
            unsafe { munmap(entry.ptr as *mut std::ffi::c_void, length) };
        }
        for &fd in &self.fds {
            unsafe { close(fd) };
        }
    }
}

fn apply_memory_sampling_flags(
    attr: &mut perf_event_attr,
    sample_freq: u64,
    stack_dump_size: u32,
    dwarf_mask: u64,
    branch_mode: Option<BranchMode>,
) {
    attr.size = std::mem::size_of::<perf_event_attr>() as u32;
    attr.set_disabled(0);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_inherit(1);
    attr.set_precise_ip(2);
    attr.set_use_clockid(1);
    attr.clockid = libc::CLOCK_MONOTONIC;
    attr.sample_freq = sample_freq;
    attr.set_freq(1);
    attr.set_mmap(1);

    // Field order below must match the layout `MemSampleFormat` parses.
    let mut sample_type = (PERF_SAMPLE_IP
        | PERF_SAMPLE_TID
        | PERF_SAMPLE_TIME
        | PERF_SAMPLE_ADDR
        | PERF_SAMPLE_ID
        | PERF_SAMPLE_CPU
        | PERF_SAMPLE_PERIOD
        | PERF_SAMPLE_CALLCHAIN
        | PERF_SAMPLE_DATA_SRC) as u64
        | PERF_SAMPLE_WEIGHT_STRUCT as u64;
    if let Some(mode) = branch_mode {
        sample_type |= PERF_SAMPLE_BRANCH_STACK as u64;
        attr.branch_sample_type = mode.sample_type();
    }
    if dwarf_mask != 0 {
        sample_type |= (PERF_SAMPLE_REGS_USER | PERF_SAMPLE_STACK_USER) as u64;
        attr.sample_regs_user = dwarf_mask;
        attr.sample_stack_user = stack_dump_size;
    }
    attr.sample_type = sample_type;
}
