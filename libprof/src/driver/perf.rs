mod binding;
pub(crate) mod branch;
mod events;
#[cfg(target_arch = "x86_64")]
mod mem;
mod mmap;
mod spe;
pub(crate) mod sysfs;

use hashbrown::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use branch::BranchMode;
use events::process_counter;
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
use events::resolve_counter_for_family;
use libc::{close, mmap, munmap, sysconf, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};
use mmap::{EventValue, ReadFormat, Records};
use perf_event_open_sys::bindings::{
    perf_event_attr, PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CPU,
    PERF_SAMPLE_ID, PERF_SAMPLE_IP, PERF_SAMPLE_READ, PERF_SAMPLE_REGS_USER,
    PERF_SAMPLE_STACK_USER, PERF_SAMPLE_TID, PERF_SAMPLE_TIME,
};
use perf_event_open_sys::{self as sys, bindings::PERF_SAMPLE_IDENTIFIER};
use smallvec::SmallVec;

use crate::driver::UnwindMode;
use crate::sink::{ProcAddr, Sample};
use crate::{Counter, Error, Record};

pub use events::list_supported_counters;
#[cfg(target_arch = "x86_64")]
pub use mem::PerfMemSamplingDriver;
pub use spe::{spe_pmu_path, PerfSpeSamplingDriver};

use super::{
    CoreId, CounterEntry, CounterResult, CounterValue, CountingDriver, MeasurementQuality,
    SamplingDriver, Sink,
};

/// Counting driver is used for simple collection of system's performance counters values. On Linux,
/// counter multiplexing is supported.
pub struct PerfCountingDriver {
    native_handles: Vec<NativeCounterHandle>,
}

impl Drop for PerfCountingDriver {
    fn drop(&mut self) {
        for handle in &self.native_handles {
            unsafe { close(handle.fd) };
        }
    }
}

/// Sampling driver performs PMU event sampling. That is, every N cycles, the process is
/// interrupted and counters values are recorded for future post processing.
pub struct PerfSamplingDriver {
    native_handles: Vec<NativeCounterHandle>,
    mmaps: Vec<UnsafeMmap>,
    page_size: usize,
    mmap_pages: usize,
    running: Arc<AtomicBool>,
    lost_samples: Arc<std::sync::atomic::AtomicU64>,
    thread_handle: Option<thread::JoinHandle<()>>,
    enable_on_start: bool,
    sample_regs_user: u64,
    branch_mode: Option<BranchMode>,
}

#[derive(Debug, Clone)]
struct NativeCounterHandle {
    pub kind: Counter,
    /// The core cluster this handle counts on, on a heterogeneous system.
    /// `None` for software counters and on homogeneous systems.
    pub core: Option<CoreId>,
    pub id: u64,
    pub fd: i32,
    pub leader: bool,
    /// Whether this handle owns a sampling ring buffer. Normally the group
    /// leader; in a fixed-topdown group the leader must not sample, so the
    /// sampling sibling owns the ring instead.
    pub sampled: bool,
}

#[derive(Debug, Clone, Copy)]
struct UnsafeMmap {
    ptr: *mut u8,
}

unsafe impl Send for UnsafeMmap {}
unsafe impl Sync for UnsafeMmap {}

impl PerfCountingDriver {
    pub fn new(counters: Vec<Counter>, pid: Option<i32>, pinned: bool) -> Result<Self, Error> {
        // On a heterogeneous (big.LITTLE) host we open every hardware counter on
        // each cluster's PMU so a migrating task is faithfully counted wherever
        // it runs. `host_core_pmus` returns more than one entry only in that
        // case; otherwise fall through to the single-PMU path below.
        let core_pmus = crate::cpu_family::host_core_pmus();
        if core_pmus.len() > 1 {
            #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
            return Self::new_per_core(counters, pid, &core_pmus);
        }

        let mut attrs = get_native_counters(&counters, true)?;

        for attr in &mut attrs {
            attr.set_exclude_kernel(1);
            attr.set_exclude_hv(1);
            attr.set_inherit(1);
            attr.set_exclusive(0);
            attr.sample_type = PERF_SAMPLE_IDENTIFIER as u64;
            if pid.is_some() {
                attr.set_enable_on_exec(1);
            }
        }

        let native_handles = binding::direct(&counters, &mut attrs, pid, pinned)?;

        Ok(PerfCountingDriver { native_handles })
    }

    /// Open each PMU counter once per core cluster (faithful per-core counting).
    /// Software counters, which are not PMU-specific, are opened a single time.
    /// A counter that a cluster's family does not implement is skipped there.
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    fn new_per_core(
        counters: Vec<Counter>,
        pid: Option<i32>,
        core_pmus: &[crate::cpu_family::CorePmu],
    ) -> Result<Self, Error> {
        let apply_flags = |attr: &mut perf_event_attr| {
            attr.set_exclude_kernel(1);
            attr.set_exclude_hv(1);
            attr.set_inherit(1);
            attr.set_exclusive(0);
            attr.sample_type = PERF_SAMPLE_IDENTIFIER as u64;
            if pid.is_some() {
                attr.set_enable_on_exec(1);
            }
        };

        let open = |counter: &Counter, attr: &mut perf_event_attr| -> Result<(i32, u64), Error> {
            let fd = unsafe { sys::perf_event_open(attr, pid.unwrap_or(0), -1, -1, 0) };
            if fd < 0 {
                return Err(Error::perf_event_open(counter, None));
            }
            let mut id: u64 = 0;
            if unsafe { sys::ioctls::ID(fd, &mut id) } < 0 {
                let error = Error::perf_ioctl("ID", counter);
                unsafe { close(fd) };
                return Err(error);
            }
            Ok((fd, id))
        };

        let mut native_handles: Vec<NativeCounterHandle> = Vec::new();

        for cntr in &counters {
            if cntr.is_software() {
                let (type_, config) = counter_type_config(cntr)?;
                let mut attr = base_counter_attr();
                attr.type_ = type_;
                attr.config = config;
                apply_flags(&mut attr);

                let (fd, id) = open(cntr, &mut attr)?;
                native_handles.push(NativeCounterHandle {
                    kind: cntr.clone(),
                    core: None,
                    id,
                    fd,
                    leader: false,
                    sampled: false,
                });
                continue;
            }

            for pmu in core_pmus {
                let Some(resolved) = resolve_counter_for_family(cntr, pmu.family_id, true) else {
                    continue; // this cluster's family does not implement it
                };

                let mut attr = build_pmu_attr(&resolved, pmu.pmu_type)?;
                apply_flags(&mut attr);

                let (fd, id) = open(cntr, &mut attr)?;
                native_handles.push(NativeCounterHandle {
                    kind: cntr.clone(),
                    core: Some(core_id_of(pmu)),
                    id,
                    fd,
                    leader: false,
                    sampled: false,
                });
            }
        }

        if native_handles.is_empty() {
            return Err(Error::InvalidConfiguration(
                "no counters could be opened".to_owned(),
            ));
        }

        Ok(PerfCountingDriver { native_handles })
    }
}

/// Build a display-friendly [`CoreId`] for a core PMU, resolving the family's
/// human readable name where known.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn core_id_of(pmu: &crate::cpu_family::CorePmu) -> CoreId {
    let name = crate::cpu_family::find_cpu_family(pmu.family_id)
        .map(|f| f.name.clone())
        .unwrap_or_else(|| pmu.family_id.to_string());

    CoreId {
        family_id: pmu.family_id.to_string(),
        name,
        cpus: pmu.cpus.clone(),
    }
}

impl CountingDriver for PerfCountingDriver {
    fn start(&mut self) -> Result<(), Error> {
        for handle in &self.native_handles {
            let res_enable = unsafe { sys::ioctls::ENABLE(handle.fd, 0) };

            if res_enable < 0 {
                return Err(Error::perf_ioctl("ENABLE", &handle.kind));
            }
        }

        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        for handle in &self.native_handles {
            let res_enable = unsafe { sys::ioctls::DISABLE(handle.fd, 0) };

            if res_enable < 0 {
                return Err(Error::perf_ioctl("DISABLE", &handle.kind));
            }
        }

        Ok(())
    }

    fn reset(&mut self) -> Result<(), Error> {
        // Per-core counters are opened as independent events (not a group), so
        // reset each one individually rather than relying on the group flag.
        for handle in &self.native_handles {
            let res_enable = unsafe { sys::ioctls::RESET(handle.fd, 0) };

            if res_enable < 0 {
                return Err(Error::perf_ioctl("RESET", &handle.kind));
            }
        }

        Ok(())
    }

    fn counters(&mut self) -> Result<CounterResult, std::io::Error> {
        let read_size = std::mem::size_of::<ReadFormat>() + (std::mem::size_of::<EventValue>());

        let mut buffer = vec![0_u8; read_size];
        let mut entries = SmallVec::<[CounterEntry; 16]>::with_capacity(self.native_handles.len());

        for handle in self.native_handles.iter() {
            let result = unsafe {
                libc::read(
                    handle.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    read_size,
                )
            };

            if result == -1 {
                return Err(std::io::Error::last_os_error());
            }

            let header = unsafe { &*(buffer.as_ptr() as *const ReadFormat) };

            let values = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().add(std::mem::size_of::<ReadFormat>()) as *const EventValue,
                    header.nr as usize,
                )
            };

            // For now it is guaranteed there's exactly 1 event
            let value = &values[0];

            let scaling_factor = if header.time_running > 0 {
                (header.time_enabled as f64) / (header.time_running as f64)
            } else {
                1.0_f64
            };

            // For a per-core counter opened on a specific cluster's PMU, the
            // "enabled but not running" time is mostly time the task spent on
            // the *other* cluster, not counter multiplexing. Extrapolating over
            // it would massively inflate the value, so report the raw on-cluster
            // count instead — the true work done on that cluster. Summing the
            // raw per-cluster counts then yields a faithful total.
            //
            // Homogeneous and software counters keep the usual multiplexing
            // extrapolation, where enabled/running reflects real time-sharing.
            let reported_value = if handle.core.is_some() {
                value.value
            } else if header.time_running > 0 {
                (value.value as f64 * scaling_factor) as u64
            } else {
                value.value
            };

            entries.push(CounterEntry {
                core: handle.core.clone(),
                counter: handle.kind.clone(),
                value: CounterValue {
                    value: reported_value,
                    scaling: scaling_factor,
                    quality: if handle.core.is_none() && scaling_factor > 1.0 {
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

unsafe impl Send for PerfSamplingDriver {}
unsafe impl Sync for PerfSamplingDriver {}

impl SamplingDriver for PerfSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        let mut counters = Vec::new();
        for handle in &self.native_handles {
            if !counters.contains(&handle.kind) {
                counters.push(handle.kind.clone());
            }
        }
        counters
    }

    fn sample_rate(&self) -> Option<(u64, u64)> {
        let effective = binding::LAST_SAMPLE_FREQ.load(Ordering::Relaxed);
        let requested = binding::LAST_SAMPLE_FREQ_REQUESTED.load(Ordering::Relaxed);
        (effective > 0 && requested > 0).then_some((effective, requested))
    }

    fn start(&mut self, callback: Arc<dyn Sink>) -> Result<(), Error> {
        if self.enable_on_start {
            for handle in self.native_handles.iter().filter(|handle| handle.leader) {
                let result =
                    unsafe { sys::ioctls::ENABLE(handle.fd, sys::bindings::PERF_IOC_FLAG_GROUP) };
                if result < 0 {
                    return Err(Error::perf_ioctl("ENABLE group", &handle.kind));
                }
            }
        }
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let lost_samples = self.lost_samples.clone();
        let mmaps = self.mmaps.clone();
        let native_handles = self.native_handles.clone();
        let sample_regs_user = self.sample_regs_user;
        let branch_mode = self.branch_mode;

        #[derive(Clone, Default)]
        struct LastSample {
            time_enabled: u64,
            time_running: u64,
            value: u64,
        }

        let handle = thread::spawn(move || {
            // Perf event values are cumulative for the task/event. The CPU in
            // PERF_SAMPLE_CPU is attribution metadata, not part of the counter
            // identity; including it here would restart the baseline after
            // every migration and over-count the first sample on the new CPU.
            let mut last_samples_map = HashMap::<(usize, u32, u32, u64), LastSample>::new();

            loop {
                for (idx, &mmap) in mmaps.iter().enumerate() {
                    let records = Records::from_ptr(mmap.ptr, sample_regs_user, branch_mode);

                    for record in records.into_iter() {
                        match record {
                            mmap::MmapRecord::Sample {
                                ip,
                                pid,
                                tid,
                                cpu,
                                time,
                                time_enabled,
                                time_running,
                                values,
                                callstack,
                                lbr_callstack,
                                user_regs,
                                user_stack,
                            } => {
                                let uid = uuid::Uuid::now_v7();
                                let mut user_regs = user_regs;
                                let mut user_stack = Some(user_stack);

                                for value in values {
                                    let Some(handle) =
                                        native_handles.iter().find(|handle| handle.id == value.id)
                                    else {
                                        continue;
                                    };
                                    let last_sample = last_samples_map
                                        .get(&(idx, pid, tid, value.id))
                                        .cloned()
                                        .unwrap_or_default();

                                    let sample = Record::Sample(Sample {
                                        event_id: uid.as_u128(),
                                        ip,
                                        pid,
                                        tid,
                                        cpu,
                                        core: handle.core.as_ref().map(|c| c.family_id.clone()),
                                        time,
                                        time_enabled: time_enabled
                                            .saturating_sub(last_sample.time_enabled),
                                        time_running: time_running
                                            .saturating_sub(last_sample.time_running),
                                        counter: handle.kind.clone(),
                                        value: value.value.saturating_sub(last_sample.value),
                                        callstack: callstack.clone(),
                                        lbr_callstack: lbr_callstack.clone(),
                                        // Every grouped counter shares this correlation id and
                                        // call stack. Carry the large raw state once; postprocess
                                        // reuses its result for the sibling counter events.
                                        user_regs: user_regs.take(),
                                        user_stack: user_stack.take().unwrap_or_default(),
                                    });

                                    last_samples_map.insert(
                                        (idx, pid, tid, value.id),
                                        LastSample {
                                            time_enabled,
                                            time_running,
                                            value: value.value,
                                        },
                                    );

                                    callback.record(sample);
                                }
                            }
                            mmap::MmapRecord::Address {
                                pid,
                                start,
                                len,
                                offset,
                                filename,
                            } => {
                                callback.record(Record::ProcAddr(ProcAddr {
                                    pid,
                                    addr: start,
                                    len,
                                    pgoff: offset,
                                    filename,
                                }));
                            }
                            mmap::MmapRecord::Lost { count } => {
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
            }
        });

        self.thread_handle = Some(handle);

        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        for handle in &self.native_handles {
            if !handle.leader {
                continue;
            }

            let res_enable =
                unsafe { sys::ioctls::DISABLE(handle.fd, sys::bindings::PERF_IOC_FLAG_GROUP) };

            if res_enable < 0 {
                return Err(Error::perf_ioctl("DISABLE", &handle.kind));
            }
        }

        self.running.store(false, Ordering::SeqCst);

        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| Error::WorkerPanicked)?;
        }

        if self.groups_scheduled() == Some(false) {
            return Err(Error::SamplingGroupNeverScheduled {
                groups: self.group_times_summary(),
            });
        }

        let lost = self.lost_samples.load(Ordering::Relaxed);
        if lost != 0 {
            return Err(Error::SamplesLost { count: lost });
        }

        Ok(())
    }
}

/// How long a group leader reports its group was enabled and running.
///
/// `None` when the kernel returned less than the fixed header, which leaves
/// the caller no basis to say anything about the group.
fn read_group_times(fd: i32) -> Option<(u64, u64)> {
    const MEMBERS: usize = 64;
    const HEADER: usize = 3;

    let mut buffer = [0_u64; HEADER + MEMBERS * 2];
    let size = std::mem::size_of_val(&buffer);
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), size) };
    if read < (HEADER * std::mem::size_of::<u64>()) as isize {
        return None;
    }
    Some((buffer[1], buffer[2]))
}

/// Whether this host runs the sampling groups that `attrs` describe for
/// `counters`. `perf_event_open` accepts a group wider than the PMU, or one
/// naming an event the PMU does not implement, and then never runs it; one
/// RISC-V host runs such groups for a forked task but never for a task that
/// has exec'd. So the probe does what a recording does: fork a child, open
/// the driver's own group plan on it with enable-on-exec, let it exec a
/// spinning shell, and read the leaders after a moment. Software-only groups
/// always schedule.
fn group_schedules(counters: &[Counter], attrs: &[perf_event_attr]) -> Result<bool, Error> {
    if counters.iter().all(Counter::is_software) {
        return Ok(true);
    }
    let cpu = target_allowed_cpus(0)?[0];
    let mut gate = [0_i32; 2];
    if unsafe { libc::pipe(gate.as_mut_ptr()) } != 0 {
        return Ok(true);
    }
    let child = unsafe { libc::fork() };
    if child < 0 {
        unsafe {
            close(gate[0]);
            close(gate[1]);
        }
        return Ok(true);
    }
    if child == 0 {
        unsafe {
            close(gate[1]);
            pin_to_cpu(cpu);
            let mut byte = 0_u8;
            libc::read(gate[0], (&mut byte as *mut u8).cast(), 1);
            close(gate[0]);
            let shell = c"/bin/sh";
            let argv = [
                shell.as_ptr(),
                c"-c".as_ptr(),
                c"while :; do :; done".as_ptr(),
                std::ptr::null(),
            ];
            libc::execv(shell.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { close(gate[0]) };
    let opened = open_inherited_target_groups_on_cpus(counters, attrs, child, &[cpu]);
    unsafe {
        libc::write(gate[1], [1_u8].as_ptr().cast(), 1);
        close(gate[1]);
    }
    let scheduled = opened.map(|handles| {
        // Multiplexed groups take turns, and the child needs a moment to
        // exec: wait until every leader has been enabled for a while, and
        // call the group unschedulable only if none of that time ran.
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        let verdict = loop {
            thread::sleep(Duration::from_millis(5));
            let times = handles
                .iter()
                .filter(|handle| handle.leader)
                .filter_map(|handle| read_group_times(handle.fd))
                .collect::<Vec<_>>();
            if times.iter().any(|(_, running)| *running > 0) {
                break true;
            }
            let settled = times.iter().all(|(enabled, _)| *enabled >= 50_000_000);
            if settled || std::time::Instant::now() >= deadline {
                // A child that never reached the group leaves no verdict.
                break times.iter().all(|(enabled, _)| *enabled == 0);
            }
        };
        for handle in &handles {
            unsafe { close(handle.fd) };
        }
        verdict
    });
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    scheduled
}

fn pin_to_cpu(cpu: i32) -> bool {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe { libc::CPU_SET(cpu as usize, &mut set) };
    unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 }
}

impl PerfSamplingDriver {
    /// One entry per group leader: how long the kernel reports it enabled
    /// and running.
    fn group_times_summary(&self) -> String {
        self.native_handles
            .iter()
            .filter(|handle| handle.leader)
            .enumerate()
            .map(|(index, handle)| {
                let (enabled, running) = read_group_times(handle.fd).unwrap_or((0, 0));
                format!(
                    "group {index} led by {}: enabled {} ms, running {} ms",
                    handle.kind.name(),
                    enabled / 1_000_000,
                    running / 1_000_000
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Whether any group the target actually reached ran on the PMU.
    ///
    /// `perf_event_open` accepts a group too large for the hardware and the
    /// kernel then silently declines to schedule it: enabled time accrues,
    /// running time stays at zero and not one sample is written. `None` when
    /// no group was ever enabled, which is a target that never ran rather than
    /// a group that never fit.
    fn groups_scheduled(&self) -> Option<bool> {
        let mut enabled_anywhere = false;
        for handle in self.native_handles.iter().filter(|handle| handle.leader) {
            let (enabled, running) = read_group_times(handle.fd)?;
            if enabled == 0 {
                continue;
            }
            if running > 0 {
                return Some(true);
            }
            enabled_anywhere = true;
        }
        enabled_anywhere.then_some(false)
    }
}

/// Whether a counter belongs to a fixed-topdown group.
fn is_topdown(counter: &Counter) -> bool {
    topdown_event_name(counter).is_some()
}

/// The topdown event name a counter carries, if it is one.
fn topdown_event_name(counter: &Counter) -> Option<&str> {
    match counter {
        Counter::Custom(name) if crate::is_topdown_event(name) => Some(name),
        _ => None,
    }
}

/// Flags for a counting member of a sampling group. Intel PERF_METRICS events
/// and their `slots` leader are rejected by the kernel when they sample, so
/// they join the group as plain counters and are reported through the sampling
/// sibling's grouped read.
fn apply_grouped_counting_flags(attr: &mut perf_event_attr, enable_on_exec: bool) {
    attr.set_exclude_kernel(1);
    attr.set_exclude_user(0);
    attr.set_exclusive(0);
    attr.set_inherit(inherited_sample_read_supported().into());
    attr.set_enable_on_exec(enable_on_exec.into());
}

/// Whether this kernel accepts an inherited sampling group whose samples carry
/// grouped counter reads. Linux rejected `inherit` together with
/// `PERF_SAMPLE_READ` before 6.12 (EINVAL; newer kernels want `PERF_SAMPLE_TID`
/// alongside, which every sampling group here carries). On such hosts the
/// groups are opened without inheritance and threads created after exec go
/// unsampled.
pub fn inherited_sample_read_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut attr = base_counter_attr();
        attr.type_ = sys::bindings::PERF_TYPE_SOFTWARE;
        attr.config = sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64;
        attr.set_exclude_kernel(1);
        attr.set_inherit(1);
        attr.sample_type = (PERF_SAMPLE_READ | PERF_SAMPLE_TID) as u64;
        attr.sample_period = 1_000_000;
        let fd = unsafe { sys::perf_event_open(&mut attr, 0, -1, -1, 0) };
        if fd < 0 {
            return std::io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL);
        }
        unsafe { close(fd) };
        true
    })
}

/// What each sample of a group captures, independent of the counters in it.
#[derive(Clone, Copy)]
pub struct SampleOptions {
    /// Target interrupt frequency in hertz.
    pub sample_freq: u64,
    /// User call-stack collection strategy.
    pub unwind_mode: UnwindMode,
    /// User stack bytes captured for DWARF unwinding.
    pub stack_dump_size: u32,
    /// Whether to request PEBS/SPE-quality instruction pointers.
    pub precise_ip: bool,
    /// How to ask the branch recorder for call stacks, when at all.
    pub branch_mode: Option<BranchMode>,
}

/// Apply the sampling-specific attribute flags shared by every counter.
fn apply_sampling_flags(
    attr: &mut perf_event_attr,
    options: &SampleOptions,
    enable_on_exec: bool,
    branch_mode: Option<BranchMode>,
) {
    let SampleOptions {
        sample_freq,
        unwind_mode,
        stack_dump_size,
        precise_ip,
        ..
    } = *options;
    attr.set_exclude_kernel(1);
    attr.set_exclude_user(0);
    attr.set_exclusive(0);
    // Process profiles must include worker threads created after exec. Without
    // inheritance, OpenMP/Rayon/pthread work silently disappears and any
    // loop-level timing or counter attribution is fundamentally incomplete.
    // Kernels before 6.12 refuse inheritance together with PERF_SAMPLE_READ;
    // there the group is opened uninherited and the loss is reported.
    attr.set_inherit(inherited_sample_read_supported().into());
    attr.set_enable_on_exec(enable_on_exec.into());
    if precise_ip {
        attr.set_precise_ip(2);
    }

    attr.sample_freq = sample_freq;
    attr.set_freq(1);

    let mut sample_type = (PERF_SAMPLE_IP
        | PERF_SAMPLE_TID
        | PERF_SAMPLE_TIME
        | PERF_SAMPLE_ID
        | PERF_SAMPLE_CPU
        | PERF_SAMPLE_READ
        | PERF_SAMPLE_CALLCHAIN) as u64;

    if unwind_mode == UnwindMode::Dwarf {
        let regs = dwarf_register_mask();
        if regs != 0 {
            sample_type |= (PERF_SAMPLE_REGS_USER | PERF_SAMPLE_STACK_USER) as u64;
            attr.sample_regs_user = regs;
            attr.sample_stack_user = stack_dump_size;
        }
    }
    // Branch records exist only on the core PMU: asking a software event for a
    // branch stack fails the whole group open, so callers gate this per counter.
    if let Some(mode) = branch_mode {
        sample_type |= PERF_SAMPLE_BRANCH_STACK as u64;
        attr.branch_sample_type = mode.sample_type();
    }
    attr.sample_type = sample_type;

    attr.set_mmap(1);
}

impl PerfSamplingDriver {
    pub fn new(
        counters: &[Counter],
        options: &SampleOptions,
        pid: Option<i32>,
        prefer_raw_events: bool,
    ) -> Result<PerfSamplingDriver, Error> {
        // On a heterogeneous (big.LITTLE) host, open a sampling group on each
        // cluster's PMU so the profile captures execution wherever the task
        // runs, not just on one cluster.
        let core_pmus = crate::cpu_family::host_core_pmus();
        if core_pmus.len() > 1 {
            #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
            return Self::new_per_core(counters, options, pid, &core_pmus);
        }

        let mut attrs = get_native_counters(counters, prefer_raw_events)?;

        for (counter, attr) in std::iter::zip(counters, &mut attrs) {
            if is_topdown(counter) {
                apply_grouped_counting_flags(attr, pid.is_some());
                continue;
            }
            apply_sampling_flags(
                attr,
                options,
                pid.is_some(),
                options.branch_mode.filter(|_| !counter.is_software()),
            );
        }

        if !group_schedules(counters, &attrs)? {
            return Err(Error::SamplingGroupNeverScheduled {
                groups: "probe".to_owned(),
            });
        }
        let native_handles = match pid {
            None => binding::grouped_all(counters, &mut attrs, None)?,
            Some(pid) => open_inherited_target_groups(counters, &attrs, pid)?,
        };

        Self::from_handles(
            native_handles,
            dwarf_mask_for_mode(options.unwind_mode),
            options.branch_mode,
            pid.is_none(),
        )
    }

    /// Faithful per-core sampling: open a sampling group on every cluster's PMU
    /// (each with that cluster's event codes), so no cluster is invisible in the
    /// profile. Each handle is tagged with the cluster it samples so downstream
    /// consumers can attribute samples per core.
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    fn new_per_core(
        counters: &[Counter],
        options: &SampleOptions,
        pid: Option<i32>,
        core_pmus: &[crate::cpu_family::CorePmu],
    ) -> Result<PerfSamplingDriver, Error> {
        let mut native_handles: Vec<NativeCounterHandle> = Vec::new();

        for pmu in core_pmus {
            let core = core_id_of(pmu);

            let mut attrs: Vec<perf_event_attr> = counters
                .iter()
                .map(|cntr| {
                    let resolved = resolve_counter_for_family(cntr, pmu.family_id, true)
                        .unwrap_or_else(|| cntr.clone());
                    let mut attr = build_pmu_attr(&resolved, pmu.pmu_type)?;
                    if is_topdown(cntr) {
                        apply_grouped_counting_flags(&mut attr, pid.is_some());
                        return Ok(attr);
                    }
                    apply_sampling_flags(
                        &mut attr,
                        options,
                        pid.is_some(),
                        options.branch_mode.filter(|_| !cntr.is_software()),
                    );
                    Ok(attr)
                })
                .collect::<Result<Vec<_>, Error>>()?;

            let mut handles = if let Some(pid) = pid {
                let cluster_cpus = parse_cpu_list(&pmu.cpus);
                let cpus = target_allowed_cpus(pid)?
                    .into_iter()
                    .filter(|cpu| cluster_cpus.contains(cpu))
                    .collect::<Vec<_>>();
                if cpus.is_empty() {
                    continue;
                }
                open_inherited_target_groups_on_cpus(counters, &attrs, pid, &cpus)?
            } else {
                binding::grouped_all(counters, &mut attrs, None)?
            };
            for handle in &mut handles {
                handle.core = Some(core.clone());
            }

            native_handles.extend(handles);
        }

        Self::from_handles(
            native_handles,
            dwarf_mask_for_mode(options.unwind_mode),
            options.branch_mode,
            pid.is_none(),
        )
    }

    /// Map every group-leader handle's ring buffer and assemble the driver.
    fn from_handles(
        native_handles: Vec<NativeCounterHandle>,
        sample_regs_user: u64,
        branch_mode: Option<BranchMode>,
        enable_on_start: bool,
    ) -> Result<PerfSamplingDriver, Error> {
        // Only hardware events carry a branch stack. When the counter fallback
        // left a software event owning the ring, its records have no branch
        // stack and parsing one would desynchronize the whole sample layout.
        let branch_mode = branch_mode.filter(|_| {
            native_handles
                .iter()
                .filter(|handle| handle.sampled)
                .all(|handle| !handle.kind.is_software())
        });
        let page_size = unsafe { sysconf(libc::_SC_PAGE_SIZE) } as usize;
        let perf_mlock_kb = std::fs::read_to_string("/proc/sys/kernel/perf_event_mlock_kb")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok());
        let mmap_pages = sampling_ring_pages(page_size, perf_mlock_kb);

        let length = page_size * (mmap_pages + 1);
        let mut mmaps: Vec<UnsafeMmap> = Vec::new();
        for handle in native_handles.iter().filter(|handle| handle.sampled) {
            let ptr = unsafe {
                let ptr = mmap(
                    std::ptr::null_mut(),
                    length,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    handle.fd,
                    0,
                ) as *mut u8;
                if ptr as *mut libc::c_void == MAP_FAILED {
                    let source = std::io::Error::last_os_error();
                    for mmap in &mmaps {
                        munmap(mmap.ptr.cast(), length);
                    }
                    for native_handle in &native_handles {
                        close(native_handle.fd);
                    }
                    return Err(Error::PerfMmap {
                        counter: handle.kind.name().to_owned(),
                        length,
                        source,
                    });
                }
                ptr
            };
            mmaps.push(UnsafeMmap { ptr });
        }

        Ok(PerfSamplingDriver {
            native_handles,
            mmaps,
            page_size,
            mmap_pages,
            running: Arc::new(AtomicBool::new(false)),
            lost_samples: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            thread_handle: None,
            sample_regs_user,
            branch_mode,
            enable_on_start,
        })
    }
}

/// Open one inherited task group on every CPU where the target may run.
///
/// `perf_event_open(pid, -1, ...)` follows a task across CPUs, but Linux
/// explicitly disallows mmap sampling for that combination when `inherit` is
/// enabled. Pairing the target PID with each allowed CPU provides equivalent
/// process-wide coverage and gives every group a valid sampling ring.
fn open_inherited_target_groups(
    counters: &[Counter],
    attrs: &[perf_event_attr],
    pid: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    let cpus = target_allowed_cpus(pid)?;
    open_inherited_target_groups_on_cpus(counters, attrs, pid, &cpus)
}

fn open_inherited_target_groups_on_cpus(
    counters: &[Counter],
    attrs: &[perf_event_attr],
    pid: i32,
    cpus: &[i32],
) -> Result<Vec<NativeCounterHandle>, Error> {
    if cpus.is_empty() {
        return Err(Error::InvalidConfiguration(format!(
            "no profiled CPUs are available for target PID {pid}"
        )));
    }
    let mut handles = Vec::new();

    for &cpu in cpus {
        let mut cpu_attrs = attrs.to_vec();
        let opened = if counters.contains(&Counter::Cycles) {
            binding::grouped_on_cpu(counters, &mut cpu_attrs, pid, cpu)
        } else {
            binding::grouped_software_on_cpu(counters, &mut cpu_attrs, pid, cpu)
        };

        match opened {
            Ok(mut cpu_handles) => handles.append(&mut cpu_handles),
            Err(error) => {
                for handle in &handles {
                    unsafe { close(handle.fd) };
                }
                return Err(error);
            }
        }
    }

    Ok(handles)
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn parse_cpu_list(mask: &str) -> std::collections::BTreeSet<i32> {
    let mut cpus = std::collections::BTreeSet::new();
    for range in mask.trim().split(',') {
        let mut bounds = range.splitn(2, '-');
        let Some(start) = bounds.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let end = bounds
            .next()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(start);
        if end >= start {
            cpus.extend(start..=end);
        }
    }
    cpus
}

fn target_allowed_cpus(pid: i32) -> Result<Vec<i32>, Error> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let result =
        unsafe { libc::sched_getaffinity(pid, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if result != 0 {
        return Err(Error::InvalidConfiguration(format!(
            "cannot read CPU affinity for target PID {pid}: {}",
            std::io::Error::last_os_error()
        )));
    }

    let cpus = (0..libc::CPU_SETSIZE as usize)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .map(|cpu| cpu as i32)
        .collect::<Vec<_>>();
    if cpus.is_empty() {
        return Err(Error::InvalidConfiguration(format!(
            "target PID {pid} has an empty CPU affinity mask"
        )));
    }
    Ok(cpus)
}

/// Select a power-of-two perf data ring which fits the kernel's per-CPU
/// `perf_event_mlock_kb` allowance, including its metadata page. A larger ring
/// reduces loss during bursts, but asking for more than the host allowance can
/// make `mmap` fail with `EINVAL` before recording starts.
fn sampling_ring_pages(page_size: usize, perf_mlock_kb: Option<usize>) -> usize {
    const DESIRED_DATA_PAGES: usize = 512;
    const CONSERVATIVE_DATA_PAGES: usize = 128;

    let Some(limit_kb) = perf_mlock_kb else {
        return CONSERVATIVE_DATA_PAGES;
    };
    let total_pages = limit_kb.saturating_mul(1024) / page_size.max(1);
    let available_data_pages = total_pages.saturating_sub(1).max(1);
    let exponent = (usize::BITS - 1) - available_data_pages.leading_zeros();
    (1_usize << exponent).min(DESIRED_DATA_PAGES)
}

fn dwarf_mask_for_mode(mode: UnwindMode) -> u64 {
    if mode == UnwindMode::Dwarf {
        dwarf_register_mask()
    } else {
        0
    }
}

#[cfg(target_arch = "x86_64")]
pub(super) fn dwarf_register_mask() -> u64 {
    // Linux's PERF_REGS_MASK: segment registers DS/ES/FS/GS (bits 12..15)
    // cannot be requested through PERF_SAMPLE_REGS_USER on x86-64.
    ((1_u64 << sys::bindings::PERF_REG_X86_64_MAX) - 1) & !(0xf_u64 << 12)
}

#[cfg(target_arch = "aarch64")]
pub(super) fn dwarf_register_mask() -> u64 {
    // x0..x29, link register, SP, and PC.
    (1_u64 << sys::bindings::PERF_REG_ARM64_MAX) - 1
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(super) fn dwarf_register_mask() -> u64 {
    0
}

impl Drop for PerfSamplingDriver {
    fn drop(&mut self) {
        for &mmap in &self.mmaps {
            unsafe {
                munmap(
                    mmap.ptr as *mut std::ffi::c_void,
                    self.page_size * (self.mmap_pages + 1),
                );
            }
        }
        for handle in &self.native_handles {
            unsafe { close(handle.fd) };
        }
    }
}

/// Base `perf_event_attr` shared by every counter: disabled at creation and
/// configured for grouped reads with scaling info.
fn base_counter_attr() -> perf_event_attr {
    let mut attrs = perf_event_attr::default();

    attrs.size = std::mem::size_of::<perf_event_attr>() as u32;
    attrs.set_disabled(1);

    // Sample timestamps must be on CLOCK_MONOTONIC so they correlate with
    // every other source (eBPF, collector, preloads). Without this perf uses
    // sched_clock, which nothing else can observe.
    attrs.set_use_clockid(1);
    attrs.clockid = libc::CLOCK_MONOTONIC;

    attrs.read_format = sys::bindings::PERF_FORMAT_GROUP as u64
        | sys::bindings::PERF_FORMAT_ID as u64
        | sys::bindings::PERF_FORMAT_TOTAL_TIME_ENABLED as u64
        | sys::bindings::PERF_FORMAT_TOTAL_TIME_RUNNING as u64;

    attrs
}

/// Map a resolved counter to its `(type_, config)` pair using the legacy
/// generic encodings (`PERF_TYPE_HARDWARE`/`SOFTWARE`/`RAW`).
fn counter_type_config(cntr: &Counter) -> Result<(u32, u64), Error> {
    Ok(match cntr {
        Counter::Cycles => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_CPU_CYCLES as u64,
        ),
        Counter::Instructions => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_INSTRUCTIONS as u64,
        ),
        Counter::LLCMisses => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_CACHE_MISSES as u64,
        ),
        Counter::LLCReferences => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_CACHE_REFERENCES as u64,
        ),
        Counter::BranchInstructions => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_BRANCH_INSTRUCTIONS as u64,
        ),
        Counter::BranchMisses => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_BRANCH_MISSES as u64,
        ),
        Counter::StalledCyclesFrontend => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_STALLED_CYCLES_FRONTEND as u64,
        ),
        Counter::StalledCyclesBackend => (
            sys::bindings::PERF_TYPE_HARDWARE,
            sys::bindings::PERF_COUNT_HW_STALLED_CYCLES_BACKEND as u64,
        ),
        Counter::CpuClock => (
            sys::bindings::PERF_TYPE_SOFTWARE,
            sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
        ),
        Counter::ContextSwitches => (
            sys::bindings::PERF_TYPE_SOFTWARE,
            sys::bindings::PERF_COUNT_SW_CONTEXT_SWITCHES as u64,
        ),
        Counter::CpuMigrations => (
            sys::bindings::PERF_TYPE_SOFTWARE,
            sys::bindings::PERF_COUNT_SW_CPU_MIGRATIONS as u64,
        ),
        Counter::PageFaults => (
            sys::bindings::PERF_TYPE_SOFTWARE,
            sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64,
        ),
        Counter::Internal { code, .. } => (sys::bindings::PERF_TYPE_RAW, *code),
        Counter::Custom(name) => {
            return Err(Error::InvalidConfiguration(format!(
                "custom counter '{name}' was not resolved"
            )))
        }
    })
}

fn get_native_counters(
    counters: &[Counter],
    prefer_raw_counters: bool,
) -> Result<Vec<perf_event_attr>, Error> {
    let attrs = counters
        .iter()
        .map(|cntr| {
            let mut attrs = base_counter_attr();

            if let Some(name) = topdown_event_name(cntr) {
                let event = sysfs::core_event_attr(&crate::sysfs_alias(name)).ok_or_else(|| {
                    Error::InvalidConfiguration(format!(
                        "topdown event '{name}' is not exposed by any core PMU"
                    ))
                })?;
                attrs.type_ = event.type_;
                attrs.config = event.config;
                return Ok(attrs);
            }

            let cntr = process_counter(cntr, prefer_raw_counters)?;
            let (type_, config) = counter_type_config(&cntr)?;
            attrs.type_ = type_;
            attrs.config = config;

            // On heterogeneous (big.LITTLE) AArch64 systems the legacy
            // PERF_TYPE_RAW / PERF_TYPE_HARDWARE encodings bind to a single
            // cluster's PMU, so events silently fail to count when the task
            // runs on the other cluster. Route hardware and raw events to the
            // dynamic PMU type that backs the detected host CPU family instead.
            // (The full per-core counting path uses `build_pmu_attr` to open on
            // every cluster; this keeps the single-PMU/sampling path correct.)
            #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
            if let Some(pmu_type) = crate::cpu_family::host_pmu_type() {
                if attrs.type_ == sys::bindings::PERF_TYPE_RAW {
                    attrs.type_ = pmu_type;
                } else if attrs.type_ == sys::bindings::PERF_TYPE_HARDWARE {
                    if let Some(code) = aarch64_hw_event_code(attrs.config) {
                        attrs.type_ = pmu_type;
                        attrs.config = code;
                    }
                }
            }

            Ok(attrs)
        })
        .collect::<Result<Vec<_>, Error>>()?;

    Ok(attrs)
}

/// Build a `perf_event_attr` for a resolved counter bound to a *specific* core
/// PMU (`pmu_type`). Hardware and raw events are routed to that PMU so they
/// count only while the task runs on that cluster; software events are left on
/// the generic software PMU.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn build_pmu_attr(resolved: &Counter, pmu_type: u32) -> Result<perf_event_attr, Error> {
    let mut attrs = base_counter_attr();
    let (type_, config) = counter_type_config(resolved)?;

    if type_ == sys::bindings::PERF_TYPE_RAW {
        attrs.type_ = pmu_type;
        attrs.config = config;
    } else if type_ == sys::bindings::PERF_TYPE_HARDWARE {
        if let Some(code) = aarch64_hw_event_code(config) {
            attrs.type_ = pmu_type;
            attrs.config = code;
        } else {
            attrs.type_ = type_;
            attrs.config = config;
        }
    } else {
        attrs.type_ = type_;
        attrs.config = config;
    }

    Ok(attrs)
}

/// Map a generic `PERF_COUNT_HW_*` config value to the equivalent AArch64
/// architectural raw event code, so hardware counters can be opened against a
/// specific core PMU on heterogeneous systems.
///
/// In practice the counting and sampling drivers request raw counters, so
/// generic hardware events are already remapped via the platform aliases before
/// they reach here; this is a defensive fallback for the non-raw path.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn aarch64_hw_event_code(config: u64) -> Option<u64> {
    let code = match config as u32 {
        sys::bindings::PERF_COUNT_HW_CPU_CYCLES => 0x11, // CPU_CYCLES
        sys::bindings::PERF_COUNT_HW_INSTRUCTIONS => 0x08, // INST_RETIRED
        sys::bindings::PERF_COUNT_HW_CACHE_REFERENCES => 0x36, // LL_CACHE_RD
        sys::bindings::PERF_COUNT_HW_CACHE_MISSES => 0x37, // LL_CACHE_MISS_RD
        sys::bindings::PERF_COUNT_HW_BRANCH_INSTRUCTIONS => 0x21, // BR_RETIRED
        sys::bindings::PERF_COUNT_HW_BRANCH_MISSES => 0x22, // BR_MIS_PRED_RETIRED
        sys::bindings::PERF_COUNT_HW_STALLED_CYCLES_FRONTEND => 0x23, // STALL_FRONTEND
        sys::bindings::PERF_COUNT_HW_STALLED_CYCLES_BACKEND => 0x24, // STALL_BACKEND
        _ => return None,
    };
    Some(code)
}
