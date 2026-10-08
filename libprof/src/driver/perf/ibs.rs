use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use libc::{close, mmap, munmap, sysconf, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};
use perf_event_open_sys as sys;
use perf_event_open_sys::bindings::{
    perf_event_attr, PERF_SAMPLE_ADDR, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CPU,
    PERF_SAMPLE_DATA_SRC, PERF_SAMPLE_ID, PERF_SAMPLE_IP, PERF_SAMPLE_PERIOD,
    PERF_SAMPLE_REGS_USER, PERF_SAMPLE_STACK_USER, PERF_SAMPLE_TID, PERF_SAMPLE_TIME,
    PERF_SAMPLE_WEIGHT_STRUCT,
};

use crate::driver::{SamplingDriver, Sink};
use crate::sink::{MemSample, ProcAddr, Record};
use crate::{Counter, Error};

use super::mmap::{MmapRecord, Records};
use super::sysfs;
use super::{sampling_ring_pages, target_allowed_cpus, UnsafeMmap};

const SYSFS_ROOT: &str = "/sys/bus/event_source/devices";
const IBS_OP_PMU: &str = "ibs_op";

/// The `ibs_op` sysfs PMU directory, when the kernel exposes AMD IBS.
pub fn ibs_pmu_path() -> Option<PathBuf> {
    let path = Path::new(SYSFS_ROOT).join(IBS_OP_PMU);
    path.is_dir().then_some(path)
}

/// Whether a `PERF_SAMPLE_DATA_SRC` word describes a load or a store. IBS tags
/// every retired micro-op; the precise-memory tables only want the stream
/// `mem-loads` and `mem-stores` would have produced on PEBS.
fn is_mem_op(data_src: u64) -> bool {
    // perf_event.h `perf_mem_op`: LOAD=0x02, STORE=0x04.
    data_src & 0x06 != 0
}

/// Precise memory sampling through AMD IBS op (`ibs_op` PMU). Runs as its own
/// event set alongside the cycles-based sampling group, and reports per-access
/// data address, data source and latency for sampled loads and stores.
///
/// Unlike Intel PEBS, IBS samples every micro-op and tags the memory ones, so
/// the sample period counts ops rather than loads and the rate cannot be read
/// as a load rate (hence the `Estimated` quality). Non-memory ops are filtered
/// here so downstream tables see the same load/store stream as PEBS.
///
/// The IBS PMU accepts no branch-stack filter (`has_branch_stack` is rejected
/// by the kernel), so call stacks come from the kernel callchain and DWARF
/// unwinding only; `branch_records` is accepted and ignored for API symmetry
/// with the PEBS driver.
pub struct PerfIbsSamplingDriver {
    fds: Vec<i32>,
    mmaps: Vec<UnsafeMmap>,
    page_size: usize,
    mmap_pages: usize,
    running: Arc<AtomicBool>,
    lost_samples: Arc<AtomicU64>,
    thread_handle: Option<thread::JoinHandle<()>>,
    sample_regs_user: u64,
}

unsafe impl Send for PerfIbsSamplingDriver {}
unsafe impl Sync for PerfIbsSamplingDriver {}

impl PerfIbsSamplingDriver {
    /// Open the `ibs_op` cycles event for the target PID on each CPU it may
    /// run on.
    pub fn new(
        pid: i32,
        sample_freq: u64,
        stack_dump_size: u32,
        dwarf_mask: u64,
        _branch_records: bool,
    ) -> Result<PerfIbsSamplingDriver, Error> {
        let pmu = ibs_pmu_path().ok_or_else(|| {
            Error::InvalidConfiguration("no `ibs_op` PMU exposed by the kernel".to_owned())
        })?;
        let type_id = std::fs::read_to_string(pmu.join("type"))
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .ok_or_else(|| {
                Error::InvalidConfiguration("`ibs_op` PMU advertises no type id".to_owned())
            })?;

        let mut attr = perf_event_attr::default();
        attr.type_ = type_id;
        // Default cycles counting (config=0). Micro-op counting via
        // `cnt_ctl=1` is a profiling choice, not a memory-sampling one.
        attr.config = 0;
        // IBS has no hardware privilege filter: the kernel accepts
        // `exclude_kernel` only together with its software filter.
        sysfs::set_format_field(&mut attr, &pmu, "swfilt", 1);
        apply_ibs_sampling_flags(&mut attr, sample_freq, stack_dump_size, dwarf_mask);

        // One fd per CPU: the kernel refuses to mmap an inherited event that
        // is not bound to a CPU.
        let counter = Counter::Custom("ibs_op".to_owned());
        let cpus = target_allowed_cpus(pid)?;
        let mut fds = Vec::with_capacity(cpus.len());
        for &cpu in &cpus {
            let fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, -1, 0) };
            if fd < 0 {
                for fd in &fds {
                    unsafe { close(*fd) };
                }
                return Err(Error::perf_event_open(&counter, Some(cpu)));
            }
            fds.push(fd);
        }
        if fds.is_empty() {
            return Err(Error::InvalidConfiguration(
                "no profiled CPUs are available for IBS sampling".to_owned(),
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
                    counter: "ibs_op".to_owned(),
                    length,
                    source,
                });
            }
            mmaps.push(UnsafeMmap { ptr });
        }

        Ok(PerfIbsSamplingDriver {
            fds,
            mmaps,
            page_size,
            mmap_pages,
            running: Arc::new(AtomicBool::new(false)),
            lost_samples: Arc::new(AtomicU64::new(0)),
            thread_handle: None,
            sample_regs_user: dwarf_mask,
        })
    }
}

impl SamplingDriver for PerfIbsSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        vec![Counter::Custom("ibs_op".to_owned())]
    }

    fn start(&mut self, callback: Arc<dyn Sink>) -> Result<(), Error> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let lost_samples = self.lost_samples.clone();
        let mmaps = self.mmaps.clone();
        let sample_regs_user = self.sample_regs_user;

        self.thread_handle = Some(thread::spawn(move || loop {
            for entry in &mmaps {
                // No branch mode: the IBS PMU rejects `PERF_SAMPLE_BRANCH_STACK`.
                for record in Records::memory(entry.ptr, sample_regs_user, None) {
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
                        } => {
                            // IBS tags every op; keep the memory stream only.
                            if !is_mem_op(data_src) {
                                continue;
                            }
                            callback.record(Record::MemSample(MemSample {
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
                            }));
                        }
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

impl Drop for PerfIbsSamplingDriver {
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

fn apply_ibs_sampling_flags(
    attr: &mut perf_event_attr,
    sample_freq: u64,
    stack_dump_size: u32,
    dwarf_mask: u64,
) {
    attr.size = std::mem::size_of::<perf_event_attr>() as u32;
    attr.set_disabled(0);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_inherit(1);
    attr.set_use_clockid(1);
    attr.clockid = libc::CLOCK_MONOTONIC;
    // IBS periods count micro-ops, not loads; frequency mode keeps the
    // interrupt rate stable while the hardware tags whatever retires.
    // The hardware mandates a low-nibble-zero minimum period; the kernel
    // rounds frequency-mode periods itself.
    attr.sample_freq = sample_freq;
    attr.set_freq(1);
    attr.set_mmap(1);

    // Field order below must match the layout `MemSampleFormat` parses.
    // No `PERF_SAMPLE_BRANCH_STACK`: the IBS PMU rejects it.
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
    if dwarf_mask != 0 {
        sample_type |= (PERF_SAMPLE_REGS_USER | PERF_SAMPLE_STACK_USER) as u64;
        attr.sample_regs_user = dwarf_mask;
        attr.sample_stack_user = stack_dump_size;
    }
    attr.sample_type = sample_type;
}
