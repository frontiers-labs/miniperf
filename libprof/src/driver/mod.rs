#[cfg(target_os = "linux")]
pub(crate) mod perf;

#[cfg(target_os = "macos")]
mod kperf;

#[cfg(target_os = "linux")]
use perf::{PerfCountingDriver, PerfSamplingDriver};

#[cfg(target_os = "macos")]
use kperf::{KPerfCountingDriver, KPerfSamplingDriver};

use smallvec::SmallVec;
use std::sync::Arc;

pub(crate) use crate::sink::Sink;
use crate::{cpu_family, Counter, Error, Process};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Operating-system backend used to access performance counters.
pub enum DriverKind {
    /// Select the native backend automatically.
    Default,
    /// Linux `perf_event_open` backend.
    Perf,
    /// Apple kperf backend.
    KPerf,
}

/// Strategy used to collect user-space call stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnwindMode {
    /// Request the kernel's frame-pointer callchain.
    FramePointer,
    #[default]
    /// Capture registers and stack bytes for post-hoc DWARF unwinding.
    Dwarf,
}

#[derive(Debug, Clone)]
/// One counter value and its perf multiplexing scale.
pub struct CounterValue {
    /// Scaled counter value.
    pub value: u64,
    /// Ratio of enabled time to running time.
    pub scaling: f64,
    /// Reliability of the value after multiplexing or estimation.
    pub quality: MeasurementQuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Reliability classification for a counter measurement.
pub enum MeasurementQuality {
    /// Directly observed without multiplexing.
    Exact,
    /// Multiplexed and scaled by enabled/running time.
    Scaled,
    /// Estimated from an indirect platform source.
    Estimated,
}

/// Counting driver is used for simple collection of system's performance counters values. On Linux,
/// counter multiplexing is supported.
pub trait CountingDriver {
    /// Enables configured counters.
    fn start(&mut self) -> Result<(), Error>;
    /// Disables configured counters.
    fn stop(&mut self) -> Result<(), Error>;
    /// Resets configured counter values to zero.
    fn reset(&mut self) -> Result<(), Error>;
    /// Reads the current counter values.
    fn counters(&mut self) -> Result<CounterResult, std::io::Error>;
}

/// Common interface for streaming PMU samples.
pub trait SamplingDriver {
    /// Counters that were successfully activated after capability fallbacks.
    fn counters(&self) -> Vec<Counter>;

    /// The sampling frequency actually in use and the one asked for, when they
    /// differ. A host ceiling shared between several groups can force the rate
    /// down, which makes a recording sparser than the caller expects, so this
    /// is reported rather than applied silently.
    fn sample_rate(&self) -> Option<(u64, u64)> {
        None
    }

    /// Starts sampling and forwards records to `callback`.
    fn start(&mut self, sink: Arc<dyn Sink>) -> Result<(), Error>;

    /// Stops sampling, drains pending records, and joins the reader thread.
    fn stop(&mut self) -> Result<(), Error>;
}

/// Identifies the core cluster a counter value was measured on, on a
/// heterogeneous (big.LITTLE) system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreId {
    /// Family id, e.g. `"cortex_a720"`.
    pub family_id: String,
    /// Human readable name, e.g. `"ARM Cortex-A720"`.
    pub name: String,
    /// sysfs cpumask for the cluster, e.g. `"0,5-11"`.
    pub cpus: String,
}

/// A single measured counter value, tagged with the core it was measured on.
#[derive(Debug, Clone)]
pub struct CounterEntry {
    /// The core cluster this value came from. `None` on homogeneous systems and
    /// for software counters, which are not PMU-specific.
    pub core: Option<CoreId>,
    /// Counter that produced this entry.
    pub counter: Counter,
    /// Measured value and scale.
    pub value: CounterValue,
}

#[derive(Debug, Clone, Default)]
/// Values returned by a counting driver.
pub struct CounterResult {
    entries: SmallVec<[CounterEntry; 16]>,
}

/// Builder for a counting driver.
pub struct CountingDriverBuilder {
    counters: Vec<Counter>,
    pid: Option<i32>,
    kind: DriverKind,
    pinned: bool,
}

/// Builder for a sampling driver.
pub struct SamplingDriverBuilder {
    counters: Vec<Counter>,
    sample_freq: u64,
    pid: Option<i32>,
    prefer_raw_events: bool,
    kind: DriverKind,
    unwind_mode: UnwindMode,
    stack_dump_size: u32,
    precise_ip: bool,
    lbr_callstack: bool,
}

/// Open precise memory sampling (data address, data source and latency per
/// access) for a target process, alongside whatever counter-based sampling is
/// already running: Arm SPE where an `arm_spe_*` PMU exists, Intel PEBS
/// `mem-loads`/`mem-stores` otherwise.
///
/// Fails with [`Error::UnsupportedDriver`] on a host with no such facility;
/// callers resolve [`crate::Feature::PreciseMem`] first to find out.
pub fn mem_sampling_driver(
    pid: i32,
    sample_freq: u64,
    stack_dump_size: u32,
    lbr_callstack: bool,
) -> Result<Box<dyn SamplingDriver>, Error> {
    let _ = (pid, sample_freq, stack_dump_size, lbr_callstack);
    #[cfg(target_os = "linux")]
    {
        if perf::spe_pmu_path().is_some() {
            return Ok(Box::new(perf::PerfSpeSamplingDriver::new(pid)?));
        }
        #[cfg(target_arch = "x86_64")]
        return Ok(Box::new(perf::PerfMemSamplingDriver::new(
            pid,
            sample_freq,
            stack_dump_size,
            perf::dwarf_register_mask(),
            lbr_callstack,
        )?));
    }
    #[allow(unreachable_code)]
    Err(Error::UnsupportedDriver {
        driver: "precise memory sampling".to_owned(),
    })
}

/// Whether this kernel accepts an inherited sampling group whose samples carry
/// grouped counter reads. `None` where the host has no such concept, `false`
/// where threads created after exec will go unsampled.
pub fn inherited_sampling_supported() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        Some(perf::inherited_sample_read_supported())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Lists counters known to the selected host driver and event table.
pub fn list_supported_counters(driver: DriverKind) -> Vec<Counter> {
    cfg_if::cfg_if! {
        if #[cfg(target_os="linux")] {
            if driver == DriverKind::Default || driver == DriverKind::Perf {
                return perf::list_supported_counters();
            }
        } else if #[cfg(target_os="macos")] {
            if driver == DriverKind::Default || driver == DriverKind::KPerf {
                return kperf::list_supported_counters();
            }
        }
    }

    vec![]
}

/// The PMU event a counter resolves to on `family`, which is what the hardware
/// actually has to find a counter for.
fn resolved_event(counter: &Counter, family: Option<&cpu_family::CPUFamily>) -> String {
    let name = counter.name();
    family
        .and_then(|family| family.aliases.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

/// Orders a sampling group: one counter per PMU event, the family's leader
/// event first.
///
/// A group that names one event twice asks the PMU for two counters to count
/// the same thing. Where the event-to-counter map is fixed (RISC-V sscofpmf)
/// the kernel accepts such a group and then never schedules it, so every
/// counter in it is lost, not just the duplicate. Families whose leader event
/// is the one `Counter::Cycles` already resolves to therefore get no separate
/// leader: cycles leads the group itself.
pub(crate) fn plan_sampling_group(
    counters: &[Counter],
    family: Option<&cpu_family::CPUFamily>,
) -> Vec<Counter> {
    let mut seen = std::collections::HashSet::new();
    let mut group: Vec<Counter> = counters
        .iter()
        .filter(|counter| seen.insert(resolved_event(counter, family)))
        .cloned()
        .collect();

    if let Some(leader) = family.and_then(|family| family.leader_event.clone()) {
        match group
            .iter()
            .position(|counter| resolved_event(counter, family) == leader)
        {
            Some(index) => {
                let counter = group.remove(index);
                group.insert(0, counter);
            }
            None => group.insert(0, Counter::Custom(leader)),
        }
    }

    group
}

impl CountingDriverBuilder {
    /// Creates an empty counting-driver configuration.
    pub fn new() -> Self {
        CountingDriverBuilder {
            counters: vec![],
            pid: None,
            kind: DriverKind::Default,
            pinned: true,
        }
    }

    /// Selects counters to collect.
    pub fn counters(mut self, counters: &[Counter]) -> Self {
        self.counters = counters.to_vec();
        self
    }

    /// Lets the counters share the PMU with a sampling group instead of
    /// holding their counter for the whole run.
    ///
    /// Cycles and instructions are pinned by default, which is what a
    /// standalone `stat` wants. Pinned events own their counter permanently,
    /// and where the event-to-counter map is fixed (RISC-V sscofpmf) that
    /// starves every sampling group naming the same event: the group is
    /// accepted and then never scheduled, so the recording silently loses
    /// every hardware counter. Any counting driver that runs while a sampler
    /// is open must ask for this.
    pub fn shared_with_sampler(mut self) -> Self {
        self.pinned = false;
        self
    }

    /// Selects a child process, or the current thread when `None`.
    pub fn process(mut self, process: Option<&Process>) -> Self {
        self.pid = process.map(|p| p.pid());
        self
    }

    /// Attaches counting to an already-running process.
    pub fn pid(mut self, pid: Option<i32>) -> Self {
        // `process()` supplies a suspended child PID. Callers commonly chain
        // `.pid(optional_pid)` afterwards; an absent optional attachment must
        // not silently replace that child with PID 0 (the profiler itself).
        if pid.is_some() {
            self.pid = pid;
        }
        self
    }

    /// Opens the configured counters and returns the native driver.
    pub fn build(self) -> Result<Box<dyn CountingDriver>, Error> {
        cfg_if::cfg_if! {
            if #[cfg(target_os="linux")] {
                if self.kind == DriverKind::Default || self.kind == DriverKind::Perf {
                    return Ok(Box::new(PerfCountingDriver::new(
                        self.counters,
                        self.pid,
                        self.pinned,
                    )?));
                }
            } else if #[cfg(target_os="macos")] {
                if self.kind == DriverKind::Default || self.kind == DriverKind::KPerf {
                    return Ok(Box::new(KPerfCountingDriver::new(self.counters, self.pid)?));
                }
            }
        }

        Err(Error::UnsupportedDriver {
            driver: format!("{:?}", self.kind),
        })
    }
}

impl Default for CountingDriverBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SamplingDriverBuilder {
    /// Creates a sampling configuration with a 1 kHz rate and DWARF stacks.
    pub fn new() -> Self {
        SamplingDriverBuilder {
            counters: vec![],
            sample_freq: crate::DEFAULT_SAMPLE_FREQUENCY_HZ,
            pid: None,
            prefer_raw_events: true,
            kind: DriverKind::Default,
            unwind_mode: UnwindMode::Dwarf,
            stack_dump_size: 8 * 1024,
            precise_ip: false,
            lbr_callstack: false,
        }
    }

    /// Selects counters included in each sample.
    ///
    /// The group is deduplicated by the PMU event each counter resolves to,
    /// and the family's leader event is moved to the front. A group that names
    /// one event twice asks the PMU for two counters to count the same thing;
    /// where the event-to-counter map is fixed (RISC-V sscofpmf) the kernel
    /// accepts such a group and then never schedules it, which costs every
    /// counter in it, not just the duplicate.
    pub fn counters(mut self, counters: &[Counter]) -> Self {
        self.counters = plan_sampling_group(
            counters,
            cpu_family::find_cpu_family(cpu_family::get_host_cpu_family()),
        );
        self
    }

    /// Attaches sampling to a suspended child process.
    pub fn process(mut self, process: &Process) -> Self {
        self.pid = Some(process.pid());
        self
    }

    /// Attaches sampling to an already-running process.
    pub fn pid(mut self, pid: i32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// Sets the target interrupt frequency in hertz.
    pub fn sample_freq(mut self, sample_freq: u64) -> Self {
        self.sample_freq = sample_freq;
        self
    }

    /// Selects the user call-stack collection strategy.
    pub fn unwind_mode(mut self, unwind_mode: UnwindMode) -> Self {
        self.unwind_mode = unwind_mode;
        self
    }

    /// Requests hardware branch records in call-stack mode (Intel LBR) as an
    /// extra stack source alongside the selected unwind mode. Silently ignored
    /// where the hardware or kernel rejects it.
    pub fn lbr_callstack(mut self, enabled: bool) -> Self {
        self.lbr_callstack = enabled;
        self
    }

    /// Maximum number of user stack bytes captured for each DWARF sample.
    pub fn stack_dump_size(mut self, bytes: u32) -> Self {
        self.stack_dump_size = bytes;
        self
    }

    /// Requests PEBS/SPE-quality instruction pointers for supported events.
    /// Opening the event remains the kernel's authoritative capability check.
    pub fn precise_ip(mut self) -> Self {
        self.precise_ip = true;
        self
    }

    /// Prefers raw CPU-family event encodings over generic perf aliases.
    pub fn prefer_raw_events(mut self) -> Self {
        self.prefer_raw_events = true;
        self
    }

    /// Opens events and creates the native sampling driver.
    pub fn build(self) -> Result<Box<dyn SamplingDriver>, Error> {
        cfg_if::cfg_if! {
            if #[cfg(target_os="linux")] {
                if self.kind == DriverKind::Default || self.kind == DriverKind::Perf {
                    let options = perf::SampleOptions {
                        sample_freq: self.sample_freq,
                        unwind_mode: self.unwind_mode,
                        stack_dump_size: self.stack_dump_size,
                        precise_ip: self.precise_ip,
                        branch_mode: None,
                    };
                    let driver = sampling_with_fallback(
                        self.counters,
                        self.lbr_callstack,
                        |counters, branch_mode| PerfSamplingDriver::new(
                            counters,
                            &perf::SampleOptions {
                                branch_mode,
                                ..options
                            },
                            self.pid,
                            self.prefer_raw_events,
                        ),
                    )?;
                    return Ok(Box::new(driver));
                }
            } else if #[cfg(target_os="macos")] {
                if self.kind == DriverKind::Default || self.kind == DriverKind::KPerf {
                    return Ok(Box::new(KPerfSamplingDriver::new(
                        &self.counters,
                        self.sample_freq,
                        self.pid,
                    )?));
                }
            }
        }

        Err(Error::UnsupportedDriver {
            driver: format!("{:?}", self.kind),
        })
    }
}

#[cfg(target_os = "linux")]
fn sampling_with_fallback<T, F>(
    mut counters: Vec<Counter>,
    branch_records: bool,
    mut open: F,
) -> Result<T, Error>
where
    F: FnMut(&[Counter], Option<perf::branch::BranchMode>) -> Result<T, Error>,
{
    let mut modes = if branch_records {
        perf::branch::BranchMode::LADDER.iter()
    } else {
        [].iter()
    };
    let mut branch_mode = modes.next().copied();

    loop {
        match open(&counters, branch_mode) {
            Ok(driver) => return Ok(driver),
            // An event this PMU simply does not implement says nothing about
            // branch records: drop the event, not the fidelity rung.
            Err(error)
                if error.is_event_unsupported()
                    && error.counter_name() != Some(Counter::Cycles.name()) =>
            {
                let unsupported = error.counter_name().unwrap_or_default();
                let Some(index) = counters
                    .iter()
                    .position(|counter| counter.name() == unsupported)
                else {
                    return Err(error);
                };
                counters.remove(index);
            }
            // The kernel took the group but the PMU never runs it: too many
            // hardware events for its counters, or one it does not implement.
            // Shed hardware events from the end until the group fits; when
            // not even cycles alone runs, sample on the cpu-clock timer.
            Err(Error::SamplingGroupNeverScheduled { groups }) => {
                let leader =
                    crate::cpu_family::find_cpu_family(crate::cpu_family::get_host_cpu_family())
                        .and_then(|family| family.leader_event.clone());
                let essential = |counter: &Counter| match counter {
                    Counter::Cycles | Counter::Instructions => true,
                    Counter::Custom(name) => leader.as_deref() == Some(name.as_str()),
                    _ => false,
                };
                match counters
                    .iter()
                    .rposition(|counter| !counter.is_software() && !essential(counter))
                {
                    Some(index) => {
                        counters.remove(index);
                    }
                    None if counters.contains(&Counter::Cycles) => {
                        counters.retain(Counter::is_software);
                        if !counters.contains(&Counter::CpuClock) {
                            counters.insert(0, Counter::CpuClock);
                        }
                    }
                    None => return Err(Error::SamplingGroupNeverScheduled { groups }),
                }
            }
            // Opening the event is the authoritative support probe for a
            // branch recorder: Intel LBR takes call-stack mode, AMD LbrV2 only
            // call filtering, AMD BRS only a plain history, and a VM none at
            // all. Step down the modes before touching the counters.
            Err(_) if branch_mode.is_some() => branch_mode = modes.next().copied(),
            Err(error) if error.counter_name() == Some(Counter::Cycles.name()) => {
                counters.retain(Counter::is_software);
                if !counters.contains(&Counter::CpuClock) {
                    counters.insert(0, Counter::CpuClock);
                }
            }
            Err(error) => return Err(error),
        }

        if counters.is_empty() {
            return Err(Error::InvalidConfiguration(
                "no sampling counters are available".to_owned(),
            ));
        }
    }
}

impl Default for SamplingDriverBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterResult {
    /// Constructs a result from individual counter entries.
    pub fn from_entries(entries: SmallVec<[CounterEntry; 16]>) -> Self {
        CounterResult { entries }
    }

    /// Faithful total for a counter, summed across every core it was measured
    /// on. On a homogeneous system this is simply the single value.
    pub fn get(&self, kind: Counter) -> Option<CounterValue> {
        let matching: SmallVec<[&CounterEntry; 8]> =
            self.entries.iter().filter(|e| e.counter == kind).collect();

        if matching.is_empty() {
            return None;
        }

        let value = matching.iter().map(|e| e.value.value).sum();
        let scaling = matching.iter().map(|e| e.value.scaling).sum::<f64>() / matching.len() as f64;

        Some(CounterValue {
            value,
            scaling,
            quality: MeasurementQuality::Exact,
        })
    }

    /// Value of a counter on one specific core.
    pub fn get_for(&self, core: &Option<CoreId>, kind: Counter) -> Option<CounterValue> {
        self.entries
            .iter()
            .find(|e| e.core == *core && e.counter == kind)
            .map(|e| e.value.clone())
    }

    /// The distinct cores present, in first-seen order. Empty on homogeneous
    /// systems (all entries are untagged).
    pub fn cores(&self) -> Vec<CoreId> {
        let mut cores: Vec<CoreId> = Vec::new();
        for entry in &self.entries {
            if let Some(core) = &entry.core {
                if !cores.contains(core) {
                    cores.push(core.clone());
                }
            }
        }
        cores
    }

    /// Returns all counter entries in collection order.
    pub fn entries(&self) -> &[CounterEntry] {
        &self.entries
    }
}

impl IntoIterator for CounterResult {
    type Item = CounterEntry;

    type IntoIter = <SmallVec<[CounterEntry; 16]> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn an_unsupported_event_does_not_cost_the_branch_records() {
        let mut attempts = Vec::new();
        let selected = sampling_with_fallback(
            vec![
                Counter::Cycles,
                Counter::StalledCyclesBackend,
                Counter::CpuClock,
            ],
            true,
            |counters, branch_mode| {
                attempts.push((counters.to_vec(), branch_mode));
                if counters.contains(&Counter::StalledCyclesBackend) {
                    Err(Error::perf_event_open_with(
                        &Counter::StalledCyclesBackend,
                        None,
                        std::io::Error::from_raw_os_error(libc::ENOENT),
                        Some(4),
                    ))
                } else {
                    Ok(counters.to_vec())
                }
            },
        )
        .expect("dropping the unsupported event should open");

        assert_eq!(selected, vec![Counter::Cycles, Counter::CpuClock]);
        assert!(
            attempts
                .iter()
                .all(|(_, mode)| *mode == Some(perf::branch::BranchMode::CallStack)),
            "branch records must survive an unrelated counter failure"
        );
    }

    #[test]
    fn a_group_the_pmu_never_runs_sheds_hardware_events_until_it_fits() {
        let selected = sampling_with_fallback(
            vec![
                Counter::Cycles,
                Counter::Instructions,
                Counter::LLCReferences,
                Counter::LLCMisses,
                Counter::CpuClock,
            ],
            false,
            |counters, _| {
                if counters.contains(&Counter::LLCReferences) {
                    Err(Error::SamplingGroupNeverScheduled {
                        groups: String::new(),
                    })
                } else {
                    Ok(counters.to_vec())
                }
            },
        )
        .expect("shedding the unschedulable event should open");

        assert_eq!(
            selected,
            vec![Counter::Cycles, Counter::Instructions, Counter::CpuClock]
        );

        // A PMU on which not even cycles and instructions run leaves the
        // cpu-clock timer; instructions are never shed on their own, since
        // hardware sampling needs them next to cycles.
        let software = sampling_with_fallback(
            vec![Counter::Cycles, Counter::Instructions, Counter::PageFaults],
            false,
            |counters, _| {
                if counters.contains(&Counter::Cycles) {
                    Err(Error::SamplingGroupNeverScheduled {
                        groups: String::new(),
                    })
                } else {
                    Ok(counters.to_vec())
                }
            },
        )
        .unwrap();
        assert_eq!(software, vec![Counter::CpuClock, Counter::PageFaults]);

        let error = sampling_with_fallback(
            vec![Counter::CpuClock],
            false,
            |_, _| -> Result<Vec<Counter>, Error> {
                Err(Error::SamplingGroupNeverScheduled {
                    groups: String::new(),
                })
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::SamplingGroupNeverScheduled { .. }));
    }

    #[test]
    fn sampling_falls_back_to_cpu_clock_when_cycles_cannot_open() {
        let mut attempts = Vec::new();
        let selected = sampling_with_fallback(
            vec![Counter::Cycles, Counter::Instructions],
            false,
            |counters, _| {
                attempts.push(counters.to_vec());
                if counters.contains(&Counter::Cycles) {
                    Err(Error::perf_event_open_with(
                        &Counter::Cycles,
                        None,
                        std::io::Error::from_raw_os_error(libc::ENOENT),
                        Some(4),
                    ))
                } else {
                    Ok(counters.to_vec())
                }
            },
        )
        .expect("software fallback should open");

        assert_eq!(attempts.len(), 2);
        assert_eq!(
            selected,
            vec![Counter::CpuClock],
            "hardware-only sampling must become a cpu-clock-only group"
        );
    }

    /// Every shipped event table must produce a sampling group the hardware can
    /// actually schedule. A group naming one PMU event twice is accepted by the
    /// kernel and then never scheduled, which loses every counter in it; this
    /// caught `spacemit_x100`, whose `leader_event` is the event
    /// `Counter::Cycles` already resolves to.
    #[test]
    fn no_shipped_event_table_plans_a_duplicated_event() {
        let requested = [
            Counter::Cycles,
            Counter::Instructions,
            Counter::LLCReferences,
            Counter::LLCMisses,
            Counter::BranchMisses,
            Counter::BranchInstructions,
            Counter::StalledCyclesBackend,
            Counter::StalledCyclesFrontend,
            Counter::CpuClock,
            Counter::CpuMigrations,
            Counter::PageFaults,
            Counter::ContextSwitches,
        ];

        for (id, family) in crate::cpu_family::families() {
            let group = plan_sampling_group(&requested, Some(family));
            let mut events = group
                .iter()
                .map(|counter| resolved_event(counter, Some(family)))
                .collect::<Vec<_>>();
            let planned = events.len();
            events.sort();
            events.dedup();
            assert_eq!(
                planned,
                events.len(),
                "{id} plans the same PMU event more than once: {group:?}"
            );

            // The binding layer requires both to be present to build a group.
            assert!(
                group.contains(&Counter::Cycles),
                "{id} dropped cycles from the sampling group"
            );
            assert!(
                group.contains(&Counter::Instructions),
                "{id} dropped instructions from the sampling group"
            );
        }
    }
}
