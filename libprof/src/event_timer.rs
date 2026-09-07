//! Low-overhead, scoped performance-counter measurements.

use std::marker::PhantomData;
use std::ops::Index;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{Counter, Error};

/// The counter read mechanism selected for an [`EventTimer`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadMethod {
    /// Counters are read directly in userspace with `rdpmc`.
    Rdpmc,
    /// Counters are read from Arm PMUv3 EL0 system registers.
    UserPmu,
    /// The group leader is read with the `read(2)` system call.
    ReadSyscall,
}

/// The measured overhead and mechanism of one complete counter snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadCost {
    method: ReadMethod,
    duration: Duration,
}

impl ReadCost {
    /// Returns the mechanism used to read counters.
    pub fn method(self) -> ReadMethod {
        self.method
    }

    /// Returns the median measured duration of one snapshot.
    pub fn duration(self) -> Duration {
        self.duration
    }

    /// Returns the median snapshot cost in nanoseconds.
    pub fn nanoseconds(self) -> u64 {
        self.duration.as_nanos().min(u128::from(u64::MAX)) as u64
    }
}

/// One counter's raw and multiplex-scaled value.
#[derive(Clone, Debug)]
pub struct Measurement {
    counter: Counter,
    raw: u64,
    value: u64,
    scaling: f64,
}

impl Measurement {
    /// Returns the counter represented by this measurement.
    pub fn counter(&self) -> &Counter {
        &self.counter
    }

    /// Returns the unscaled hardware count.
    pub fn raw(&self) -> u64 {
        self.raw
    }

    /// Returns the count scaled for perf multiplexing.
    pub fn value(&self) -> u64 {
        self.value
    }

    /// Returns `time_enabled / time_running`, or one when scaling is unnecessary.
    pub fn scaling(&self) -> f64 {
        self.scaling
    }
}

/// Counter deltas and elapsed wall time for one scope.
#[derive(Clone, Debug)]
pub struct Measurements {
    entries: Vec<Measurement>,
    wall_ns: u64,
}

impl Measurements {
    /// Returns all counter measurements in the order requested from the timer.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Measurement> {
        self.entries.iter()
    }

    /// Returns the elapsed wall-clock time in nanoseconds.
    pub fn wall_ns(&self) -> u64 {
        self.wall_ns
    }

    /// Returns the measurement for `counter`, if it was requested.
    pub fn get(&self, counter: &Counter) -> Option<&Measurement> {
        self.entries.iter().find(|entry| &entry.counter == counter)
    }

    /// Returns retired instructions per cycle.
    ///
    /// Returns `NaN` when either required counter is absent or cycles is zero.
    pub fn ipc(&self) -> f64 {
        let Some(instructions) = self.get(&Counter::Instructions) else {
            return f64::NAN;
        };
        let Some(cycles) = self.get(&Counter::Cycles) else {
            return f64::NAN;
        };
        if cycles.value == 0 {
            f64::NAN
        } else {
            instructions.value as f64 / cycles.value as f64
        }
    }

    /// Returns the multiplex scaling factor for `counter`.
    pub fn scaling(&self, counter: &Counter) -> Option<f64> {
        self.get(counter).map(Measurement::scaling)
    }
}

impl Index<Counter> for Measurements {
    type Output = u64;

    fn index(&self, counter: Counter) -> &Self::Output {
        &self
            .get(&counter)
            .unwrap_or_else(|| panic!("counter '{}' was not requested", counter.name()))
            .value
    }
}

impl Index<&Counter> for Measurements {
    type Output = u64;

    fn index(&self, counter: &Counter) -> &Self::Output {
        &self
            .get(counter)
            .unwrap_or_else(|| panic!("counter '{}' was not requested", counter.name()))
            .value
    }
}

/// Summary statistics for one counter over repeated measurements.
#[derive(Clone, Debug)]
pub struct CounterStatistics {
    counter: Counter,
    min: u64,
    mean: f64,
    p50: u64,
    p99: u64,
}

impl CounterStatistics {
    /// Returns the summarized counter.
    pub fn counter(&self) -> &Counter {
        &self.counter
    }

    /// Returns the minimum scaled count.
    pub fn min(&self) -> u64 {
        self.min
    }

    /// Returns the arithmetic mean scaled count.
    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// Returns the nearest-rank 50th percentile.
    pub fn p50(&self) -> u64 {
        self.p50
    }

    /// Returns the nearest-rank 99th percentile.
    pub fn p99(&self) -> u64 {
        self.p99
    }
}

/// Labeled statistics returned by [`EventTimer::measure_n`].
#[derive(Clone, Debug)]
pub struct MeasurementStatistics {
    label: String,
    iterations: usize,
    counters: Vec<CounterStatistics>,
}

impl MeasurementStatistics {
    /// Returns the user-supplied measurement label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the number of measured invocations.
    pub fn iterations(&self) -> usize {
        self.iterations
    }

    /// Returns all per-counter statistics in timer order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CounterStatistics> {
        self.counters.iter()
    }

    /// Returns statistics for a counter, if requested.
    pub fn get(&self, counter: &Counter) -> Option<&CounterStatistics> {
        self.counters.iter().find(|stats| &stats.counter == counter)
    }
}

impl Index<Counter> for MeasurementStatistics {
    type Output = CounterStatistics;

    fn index(&self, counter: Counter) -> &Self::Output {
        self.get(&counter)
            .unwrap_or_else(|| panic!("counter '{}' was not requested", counter.name()))
    }
}

/// A per-thread group of perf events optimized for scoped measurement.
///
/// The timer counts only the thread that creates it (`pid = 0`, `cpu = -1`). It
/// is deliberately neither [`Send`] nor [`Sync`]; create a timer inside every
/// worker thread with [`EventTimer::new_for_thread`] instead of moving one.
/// Events are scheduled as one perf group, so they run and multiplex together.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<libprof::EventTimer>();
/// ```
pub struct EventTimer {
    counters: Vec<Counter>,
    backend: backend::Backend,
    read_cost: ReadCost,
    _thread_bound: PhantomData<Rc<()>>,
}

/// Opaque starting snapshot used by feature-gated measurement integrations.
#[cfg(feature = "criterion")]
pub struct CounterCheckpoint {
    start: backend::Snapshot,
    wall_start: Instant,
}

impl EventTimer {
    /// Opens and enables a coherent per-thread counter group.
    pub fn new(counters: &[Counter]) -> Result<Self, Error> {
        if counters.is_empty() {
            return Err(Error::InvalidConfiguration(
                "EventTimer requires at least one counter".to_owned(),
            ));
        }
        if counters
            .iter()
            .enumerate()
            .any(|(index, counter)| counters[..index].iter().any(|earlier| earlier == counter))
        {
            return Err(Error::InvalidConfiguration(
                "EventTimer counters must be unique".to_owned(),
            ));
        }

        let backend = backend::Backend::new(counters)?;
        let method = backend.method();
        let duration = calibrate_read_cost(&backend)?;
        Ok(Self {
            counters: counters.to_vec(),
            backend,
            read_cost: ReadCost { method, duration },
            _thread_bound: PhantomData,
        })
    }

    /// Alias for [`EventTimer::new`] emphasizing that each thread needs its own timer.
    pub fn new_for_thread(counters: &[Counter]) -> Result<Self, Error> {
        Self::new(counters)
    }

    /// Returns the selected counter-read mechanism and its measured overhead.
    pub fn read_cost(&self) -> ReadCost {
        self.read_cost
    }

    /// Begins a scoped measurement.
    pub fn start(&self) -> Result<MeasurementSpan<'_>, Error> {
        Ok(MeasurementSpan {
            timer: self,
            start: self.backend.snapshot()?,
            wall_start: Instant::now(),
        })
    }

    #[cfg(feature = "criterion")]
    pub(crate) fn checkpoint(&self) -> Result<CounterCheckpoint, Error> {
        Ok(CounterCheckpoint {
            start: self.backend.snapshot()?,
            wall_start: Instant::now(),
        })
    }

    #[cfg(feature = "criterion")]
    pub(crate) fn since(&self, checkpoint: CounterCheckpoint) -> Result<Measurements, Error> {
        finish_measurement(
            &self.counters,
            &self.backend,
            checkpoint.start,
            checkpoint.wall_start,
        )
    }

    /// Measures a closure repeatedly and computes min/mean/p50/p99 per counter.
    pub fn measure_n<F, R>(
        &self,
        label: impl Into<String>,
        iterations: usize,
        mut work: F,
    ) -> Result<MeasurementStatistics, Error>
    where
        F: FnMut() -> R,
    {
        if iterations == 0 {
            return Err(Error::InvalidConfiguration(
                "measure_n requires at least one iteration".to_owned(),
            ));
        }

        let mut values = vec![Vec::with_capacity(iterations); self.counters.len()];
        for _ in 0..iterations {
            let span = self.start()?;
            std::hint::black_box(work());
            let measured = span.stop()?;
            for (slot, entry) in values.iter_mut().zip(measured.entries) {
                slot.push(entry.value);
            }
        }

        let counters = self
            .counters
            .iter()
            .cloned()
            .zip(values)
            .map(|(counter, mut samples)| {
                samples.sort_unstable();
                let sum = samples.iter().map(|&value| value as u128).sum::<u128>();
                CounterStatistics {
                    counter,
                    min: samples[0],
                    mean: sum as f64 / samples.len() as f64,
                    p50: percentile(&samples, 50),
                    p99: percentile(&samples, 99),
                }
            })
            .collect();

        Ok(MeasurementStatistics {
            label: label.into(),
            iterations,
            counters,
        })
    }
}

/// An in-progress scoped measurement.
#[must_use = "call stop() after the measured block"]
pub struct MeasurementSpan<'timer> {
    timer: &'timer EventTimer,
    start: backend::Snapshot,
    wall_start: Instant,
}

impl MeasurementSpan<'_> {
    /// Stops the scope and returns scaled counter and wall-time deltas.
    pub fn stop(self) -> Result<Measurements, Error> {
        finish_measurement(
            &self.timer.counters,
            &self.timer.backend,
            self.start,
            self.wall_start,
        )
    }
}

fn finish_measurement(
    counters: &[Counter],
    backend: &backend::Backend,
    start: backend::Snapshot,
    wall_start: Instant,
) -> Result<Measurements, Error> {
    let wall_elapsed = wall_start.elapsed();
    let end = backend.snapshot()?;
    let enabled = end.time_enabled.saturating_sub(start.time_enabled);
    let running = end.time_running.saturating_sub(start.time_running);
    // perf time is in nanoseconds and follows the exact interval between
    // counter snapshots. The metadata fast path advances it from TSC when
    // cap_user_time is available; retain Instant as a defensive fallback.
    let wall_ns = if enabled > 0 {
        enabled
    } else {
        wall_elapsed.as_nanos().min(u128::from(u64::MAX)) as u64
    };
    let scaling = multiplex_scaling(enabled, running);
    let entries = counters
        .iter()
        .cloned()
        .zip(end.values.into_iter().zip(start.values))
        .map(|(counter, (end, start))| {
            let raw = end.wrapping_sub(start);
            Measurement {
                counter,
                raw,
                value: (raw as f64 * scaling).round() as u64,
                scaling,
            }
        })
        .collect();
    Ok(Measurements { entries, wall_ns })
}

fn multiplex_scaling(enabled: u64, running: u64) -> f64 {
    if running > 0 {
        enabled as f64 / running as f64
    } else {
        1.0
    }
}

fn calibrate_read_cost(backend: &backend::Backend) -> Result<Duration, Error> {
    const SAMPLES: usize = 31;
    let mut durations = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        std::hint::black_box(backend.snapshot()?);
        durations.push(started.elapsed());
    }
    durations.sort_unstable();
    Ok(durations[SAMPLES / 2])
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

#[cfg(target_os = "linux")]
mod backend {
    use std::sync::atomic::{fence, Ordering};

    use perf_event_open_sys::bindings::{perf_event_attr, perf_event_mmap_page};
    use perf_event_open_sys::{self as sys};

    use super::ReadMethod;
    use crate::{Counter, Error};

    pub(super) struct Snapshot {
        pub(super) values: Vec<u64>,
        pub(super) time_enabled: u64,
        pub(super) time_running: u64,
    }

    struct Event {
        fd: i32,
        metadata: *mut perf_event_mmap_page,
        map_len: usize,
    }

    impl Drop for Event {
        fn drop(&mut self) {
            if !self.metadata.is_null() {
                unsafe { libc::munmap(self.metadata.cast(), self.map_len) };
            }
            unsafe { libc::close(self.fd) };
        }
    }

    pub(super) struct Backend {
        events: Vec<Event>,
        user_read: bool,
    }

    impl Backend {
        pub(super) fn new(counters: &[Counter]) -> Result<Self, Error> {
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
            if page_size <= 0 {
                return Err(Error::InvalidConfiguration(
                    "could not determine system page size".to_owned(),
                ));
            }
            let map_len = page_size as usize;
            let mut events = Vec::with_capacity(counters.len());
            let mut leader_fd = -1;

            for (index, counter) in counters.iter().enumerate() {
                let resolved = resolve_counter(counter)?;
                let mut attr = attr_for(&resolved)?;
                attr.set_disabled((index == 0).into());
                let fd = unsafe { sys::perf_event_open(&mut attr, 0, -1, leader_fd, 0) };
                #[cfg(target_arch = "aarch64")]
                let fd = if fd < 0 {
                    // Kernels predating the arm64 userspace-read ABI may reject
                    // config1's request bit. Reopen without it so EventTimer
                    // retains its grouped read(2) fallback on those kernels.
                    attr.config1 &= !(1 << 1);
                    unsafe { sys::perf_event_open(&mut attr, 0, -1, leader_fd, 0) }
                } else {
                    fd
                };
                if fd < 0 {
                    return Err(Error::perf_event_open(counter, None));
                }
                if index == 0 {
                    leader_fd = fd;
                }
                events.push(Event {
                    fd,
                    metadata: std::ptr::null_mut(),
                    map_len,
                });
            }

            let enable =
                unsafe { sys::ioctls::ENABLE(leader_fd, sys::bindings::PERF_IOC_FLAG_GROUP) };
            if enable < 0 {
                return Err(Error::perf_ioctl("ENABLE group", &counters[0]));
            }

            let mut user_read = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"));
            if user_read {
                for event in &mut events {
                    let ptr = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            map_len,
                            libc::PROT_READ,
                            libc::MAP_SHARED,
                            event.fd,
                            0,
                        )
                    };
                    if ptr == libc::MAP_FAILED {
                        user_read = false;
                        break;
                    }
                    event.metadata = ptr.cast();
                    let supported = unsafe {
                        (*event.metadata)
                            .__bindgen_anon_1
                            .__bindgen_anon_1
                            .cap_user_rdpmc()
                            != 0
                    };
                    if !supported {
                        user_read = false;
                        break;
                    }
                }
            }
            if !user_read {
                for event in &mut events {
                    if !event.metadata.is_null() {
                        unsafe { libc::munmap(event.metadata.cast(), event.map_len) };
                        event.metadata = std::ptr::null_mut();
                    }
                }
            }

            Ok(Self { events, user_read })
        }

        pub(super) fn method(&self) -> ReadMethod {
            if self.user_read {
                #[cfg(target_arch = "aarch64")]
                return ReadMethod::UserPmu;
                #[cfg(not(target_arch = "aarch64"))]
                return ReadMethod::Rdpmc;
            } else {
                ReadMethod::ReadSyscall
            }
        }

        pub(super) fn snapshot(&self) -> Result<Snapshot, Error> {
            if self.user_read {
                Ok(self.snapshot_user())
            } else {
                self.snapshot_read()
            }
        }

        fn snapshot_user(&self) -> Snapshot {
            let mut values = Vec::with_capacity(self.events.len());
            let mut time_enabled = 0;
            let mut time_running = 0;
            for (position, event) in self.events.iter().enumerate() {
                let reading = unsafe { read_metadata(event.metadata) };
                values.push(reading.value);
                if position == 0 {
                    time_enabled = reading.time_enabled;
                    time_running = reading.time_running;
                }
            }
            Snapshot {
                values,
                time_enabled,
                time_running,
            }
        }

        fn snapshot_read(&self) -> Result<Snapshot, Error> {
            let words = 3 + self.events.len() * 2;
            let mut data = vec![0_u64; words];
            let bytes = data.len() * std::mem::size_of::<u64>();
            let read = loop {
                let result =
                    unsafe { libc::read(self.events[0].fd, data.as_mut_ptr().cast(), bytes) };
                if result < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break result;
            };
            if read < 0 || read as usize != bytes {
                return Err(Error::PerfRead {
                    source: if read < 0 {
                        std::io::Error::last_os_error()
                    } else {
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "short perf group read",
                        )
                    },
                });
            }
            if data[0] as usize != self.events.len() {
                return Err(Error::PerfRead {
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "kernel returned the wrong number of grouped counters",
                    ),
                });
            }
            Ok(Snapshot {
                time_enabled: data[1],
                time_running: data[2],
                values: (0..self.events.len())
                    .map(|index| data[3 + index * 2])
                    .collect(),
            })
        }
    }

    fn attr_for(counter: &Counter) -> Result<perf_event_attr, Error> {
        let (type_, config) = match counter {
            Counter::Cycles => (
                sys::bindings::PERF_TYPE_HARDWARE,
                sys::bindings::PERF_COUNT_HW_CPU_CYCLES as u64,
            ),
            Counter::Instructions => (
                sys::bindings::PERF_TYPE_HARDWARE,
                sys::bindings::PERF_COUNT_HW_INSTRUCTIONS as u64,
            ),
            Counter::LLCReferences => (
                sys::bindings::PERF_TYPE_HARDWARE,
                sys::bindings::PERF_COUNT_HW_CACHE_REFERENCES as u64,
            ),
            Counter::LLCMisses => (
                sys::bindings::PERF_TYPE_HARDWARE,
                sys::bindings::PERF_COUNT_HW_CACHE_MISSES as u64,
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
            Counter::PageFaults => (
                sys::bindings::PERF_TYPE_SOFTWARE,
                sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64,
            ),
            Counter::ContextSwitches => (
                sys::bindings::PERF_TYPE_SOFTWARE,
                sys::bindings::PERF_COUNT_SW_CONTEXT_SWITCHES as u64,
            ),
            Counter::CpuMigrations => (
                sys::bindings::PERF_TYPE_SOFTWARE,
                sys::bindings::PERF_COUNT_SW_CPU_MIGRATIONS as u64,
            ),
            Counter::Internal { code, .. } => (sys::bindings::PERF_TYPE_RAW, *code),
            Counter::Custom(name) => {
                return Err(Error::InvalidConfiguration(format!(
                    "custom counter '{name}' must be resolved before use with EventTimer"
                )))
            }
        };
        let mut attr = perf_event_attr::default();
        attr.size = std::mem::size_of::<perf_event_attr>() as u32;
        attr.type_ = type_;
        attr.config = config;
        attr.set_exclude_kernel(1);
        attr.set_exclude_hv(1);
        attr.set_inherit(0);
        attr.read_format = (sys::bindings::PERF_FORMAT_GROUP
            | sys::bindings::PERF_FORMAT_ID
            | sys::bindings::PERF_FORMAT_TOTAL_TIME_ENABLED
            | sys::bindings::PERF_FORMAT_TOTAL_TIME_RUNNING) as u64;
        #[cfg(target_arch = "aarch64")]
        {
            // Linux arm_pmuv3's `rdpmc` format flag is config1 bit 1. The
            // kernel exposes an mmap index only when this is requested and
            // kernel.perf_user_access permits EL0 PMU reads.
            attr.config1 |= 1 << 1;
        }
        Ok(attr)
    }

    fn resolve_counter(counter: &Counter) -> Result<Counter, Error> {
        let Counter::Custom(name) = counter else {
            return Ok(counter.clone());
        };
        let family = crate::cpu_family::get_host_cpu_family();
        let info = crate::cpu_family::find_cpu_family(family).ok_or_else(|| {
            Error::UnsupportedCounter {
                counter: name.clone(),
                family: family.to_owned(),
            }
        })?;
        let event = info
            .events
            .get(name)
            .or_else(|| {
                info.events
                    .values()
                    .find(|event| event.name.eq_ignore_ascii_case(name))
            })
            .ok_or_else(|| Error::UnsupportedCounter {
                counter: name.clone(),
                family: family.to_owned(),
            })?;
        Ok(Counter::Internal {
            name: event.name.clone(),
            desc: event.desc.clone(),
            code: event.code,
        })
    }

    struct MetadataReading {
        value: u64,
        time_enabled: u64,
        time_running: u64,
    }

    unsafe fn read_metadata(page: *const perf_event_mmap_page) -> MetadataReading {
        loop {
            let sequence = std::ptr::read_volatile(std::ptr::addr_of!((*page).lock));
            fence(Ordering::Acquire);
            if sequence & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let index = std::ptr::read_volatile(std::ptr::addr_of!((*page).index));
            let offset = std::ptr::read_volatile(std::ptr::addr_of!((*page).offset));
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let width = std::ptr::read_volatile(std::ptr::addr_of!((*page).pmc_width));
            let base_time_enabled =
                std::ptr::read_volatile(std::ptr::addr_of!((*page).time_enabled));
            let base_time_running =
                std::ptr::read_volatile(std::ptr::addr_of!((*page).time_running));

            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let (time_enabled, time_running) = {
                let mut time_enabled = base_time_enabled;
                let mut time_running = base_time_running;
                let capabilities = (*page).__bindgen_anon_1.__bindgen_anon_1;
                if capabilities.cap_user_time() != 0 && time_enabled != time_running {
                    let time_shift =
                        std::ptr::read_volatile(std::ptr::addr_of!((*page).time_shift));
                    let time_mult = std::ptr::read_volatile(std::ptr::addr_of!((*page).time_mult));
                    let time_offset =
                        std::ptr::read_volatile(std::ptr::addr_of!((*page).time_offset));
                    let mut cycles = read_time_counter();
                    if capabilities.cap_user_time_short() != 0 {
                        let time_cycles =
                            std::ptr::read_volatile(std::ptr::addr_of!((*page).time_cycles));
                        let time_mask =
                            std::ptr::read_volatile(std::ptr::addr_of!((*page).time_mask));
                        cycles = normalize_short_time(cycles, time_cycles, time_mask);
                    }
                    let delta = perf_time_delta(cycles, time_shift, time_mult, time_offset);
                    time_enabled = time_enabled.wrapping_add(delta);
                    if index != 0 {
                        time_running = time_running.wrapping_add(delta);
                    }
                }
                (time_enabled, time_running)
            };

            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let (time_enabled, time_running) = (base_time_enabled, base_time_running);

            let value = if index == 0 {
                offset as u64
            } else {
                #[cfg(target_arch = "x86_64")]
                {
                    let pmc = rdpmc(index - 1);
                    metadata_counter_value(offset, pmc, width)
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let pmc = read_arm_pmc(index);
                    metadata_counter_value(offset, pmc, width)
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                {
                    offset as u64
                }
            };
            fence(Ordering::Acquire);
            if std::ptr::read_volatile(std::ptr::addr_of!((*page).lock)) == sequence {
                return MetadataReading {
                    value,
                    time_enabled,
                    time_running,
                };
            }
        }
    }

    fn sign_extend_counter(value: u64, width: u16) -> i64 {
        if width == 0 || width >= 64 {
            value as i64
        } else {
            ((value << (64 - width)) as i64) >> (64 - width)
        }
    }

    fn metadata_counter_value(offset: i64, raw: u64, width: u16) -> u64 {
        offset.wrapping_add(sign_extend_counter(raw, width)) as u64
    }

    fn perf_time_delta(cycles: u64, shift: u16, mult: u32, offset: u64) -> u64 {
        let quotient = if shift >= 64 { 0 } else { cycles >> shift };
        let mask = if shift == 0 {
            0
        } else if shift >= 64 {
            u64::MAX
        } else {
            (1_u64 << shift) - 1
        };
        let remainder = cycles & mask;
        let fractional = if shift >= 64 {
            0
        } else {
            remainder.wrapping_mul(u64::from(mult)) >> shift
        };
        offset
            .wrapping_add(quotient.wrapping_mul(u64::from(mult)))
            .wrapping_add(fractional)
    }

    fn normalize_short_time(cycles: u64, time_cycles: u64, time_mask: u64) -> u64 {
        time_cycles.wrapping_add(cycles.wrapping_sub(time_cycles) & time_mask)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline]
    fn read_time_counter() -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline]
    fn read_time_counter() -> u64 {
        let value: u64;
        unsafe {
            core::arch::asm!(
                "mrs {value}, cntvct_el0",
                value = out(reg) value,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }

    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn rdpmc(counter: u32) -> u64 {
        let low: u32;
        let high: u32;
        core::arch::asm!(
            "rdpmc",
            in("ecx") counter,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
        (u64::from(high) << 32) | u64::from(low)
    }

    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn read_arm_pmc(perf_index: u32) -> u64 {
        match arm_counter_kind(perf_index)
            .expect("kernel published an invalid Arm PMU userspace index")
        {
            ArmCounterKind::Cycles => {
                let value: u64;
                core::arch::asm!(
                    "mrs {value}, pmccntr_el0",
                    value = out(reg) value,
                    options(nomem, nostack, preserves_flags)
                );
                value
            }
            ArmCounterKind::Event(counter) => {
                let value: u64;
                core::arch::asm!(
                    "msr pmselr_el0, {counter}",
                    "isb",
                    "mrs {value}, pmxevcntr_el0",
                    counter = in(reg) u64::from(counter),
                    value = out(reg) value,
                    options(nostack, preserves_flags)
                );
                value
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[derive(Debug, Eq, PartialEq)]
    enum ArmCounterKind {
        Event(u32),
        Cycles,
    }

    #[cfg(target_arch = "aarch64")]
    fn arm_counter_kind(perf_index: u32) -> Option<ArmCounterKind> {
        match perf_index.checked_sub(1)? {
            index @ 0..=30 => Some(ArmCounterKind::Event(index)),
            31 => Some(ArmCounterKind::Cycles),
            _ => None,
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    use super::ReadMethod;
    use crate::{Counter, Error};

    pub(super) struct Snapshot {
        pub(super) values: Vec<u64>,
        pub(super) time_enabled: u64,
        pub(super) time_running: u64,
    }

    pub(super) struct Backend;

    impl Backend {
        pub(super) fn new(_counters: &[Counter]) -> Result<Self, Error> {
            Err(Error::UnsupportedDriver {
                driver: "EventTimer is currently supported only on Linux".to_owned(),
            })
        }

        pub(super) fn method(&self) -> ReadMethod {
            ReadMethod::ReadSyscall
        }

        pub(super) fn snapshot(&self) -> Result<Snapshot, Error> {
            unreachable!("unsupported EventTimer backend cannot be constructed")
        }
    }
}
