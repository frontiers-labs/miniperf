use std::iter::zip;

use perf_event_open_sys::{self as sys, bindings::perf_event_attr};

use crate::{cpu_family, Counter, Error};

use super::NativeCounterHandle;

pub fn direct(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: Option<i32>,
    pinned: bool,
) -> Result<Vec<NativeCounterHandle>, Error> {
    let mut handles: Vec<NativeCounterHandle> = vec![];

    for (cntr, attr) in std::iter::zip(counters, attrs) {
        // cycles and instructions are typically fixed counters and thus always
        // on. A pinned event owns its counter for the whole run, so a caller
        // that runs alongside a sampling group must opt out: on a PMU with a
        // fixed event-to-counter map, a pinned event starves every group that
        // names it.
        match cntr {
            Counter::Cycles | Counter::Instructions if pinned => attr.set_pinned(1),
            _ => attr.set_pinned(0),
        };
        let new_fd = unsafe {
            sys::perf_event_open(
                &mut *attr as *mut perf_event_attr,
                pid.unwrap_or(0),
                -1,
                -1,
                0,
            )
        };

        if new_fd < 0 {
            close_handles(&handles);
            return Err(Error::perf_event_open(cntr, None));
        }

        let mut id: u64 = 0;

        let result = unsafe { sys::ioctls::ID(new_fd, &mut id) };
        if result < 0 {
            let error = Error::perf_ioctl("ID", cntr);
            unsafe { libc::close(new_fd) };
            close_handles(&handles);
            return Err(error);
        }

        handles.push(NativeCounterHandle {
            kind: cntr.clone(),
            core: None,
            id,
            fd: new_fd,
            leader: false,
            sampled: false,
        });
    }

    Ok(handles)
}

/// Open coherent hardware sampling groups for one task on one CPU.
///
/// Linux cannot mmap a sampling ring for an inherited event opened with
/// `cpu == -1`. Process-wide inherited sampling therefore opens this form once
/// for every CPU in the target's affinity mask.
pub fn grouped_on_cpu(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: i32,
    cpu: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    if counters.iter().any(is_topdown) {
        return grouped_topdown_on_cpu(counters, attrs, pid, cpu);
    }

    // TMA passes one complete group after another, each beginning with cycles.
    // Do not flatten these into the historical arbitrary chunks: doing so
    // breaks the common denominator that makes a Top-down ratio meaningful.
    let boundaries = counters
        .iter()
        .enumerate()
        .filter_map(|(index, counter)| (*counter == Counter::Cycles).then_some(index))
        .collect::<Vec<_>>();
    if boundaries.len() > 1 {
        let mut handles = Vec::new();
        for (position, start) in boundaries.iter().enumerate() {
            let end = boundaries
                .get(position + 1)
                .copied()
                .unwrap_or(counters.len());
            if *start == end || counters[*start] != Counter::Cycles {
                return Err(Error::InvalidConfiguration(
                    "invalid coherent sampling group".to_owned(),
                ));
            }
            let group_handles =
                grouped_all_on_cpu(&counters[*start..end], &mut attrs[*start..end], pid, cpu);
            match group_handles {
                Ok(group_handles) => handles.extend(group_handles),
                Err(error) => {
                    close_handles(&handles);
                    return Err(error);
                }
            }
        }
        return Ok(handles);
    }

    let cpu_family = cpu_family::get_host_cpu_family();
    let info = cpu_family::find_cpu_family(cpu_family);

    let leader = info.and_then(|info| info.leader_event.clone());

    let leader_cntr = counters.iter().find(|cntr| match (cntr, leader.as_ref()) {
        (Counter::Custom(name), Some(leader)) => name == leader,
        _ => false,
    });

    // A family whose leader event is the one `Counter::Cycles` already
    // resolves to needs no separate ring owner: cycles is it. Opening one
    // anyway would name the same PMU event twice, and a PMU with a fixed
    // event-to-counter map accepts that group and then never schedules it.
    let has_leader = leader_cntr.is_some();

    // The NMI watchdog permanently occupies one hardware counter. A sampling
    // group sized to the full PMU then never schedules and silently produces
    // zero samples, so shrink every group by the counters the kernel keeps.
    let max_counters_in_group = info
        .and_then(|info| info.max_counters)
        .unwrap_or_else(|| if has_leader { 2 } else { 3 })
        .saturating_sub(reserved_hardware_counters())
        .max(1);

    let mut cycles_attrs = zip(counters, attrs.iter())
        .find(|(cntr, _)| **cntr == Counter::Cycles)
        .map(|(_, attrs)| attrs)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidConfiguration("cycles are required for hardware sampling".to_owned())
        })?;
    let mut instr_attrs = zip(counters, attrs.iter())
        .find(|(cntr, _)| **cntr == Counter::Instructions)
        .map(|(_, attrs)| attrs)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidConfiguration(
                "instructions are required for hardware sampling".to_owned(),
            )
        })?;

    let mut leader_attrs = zip(counters, attrs.iter())
        .find(|(cntr, _)| leader_cntr == Some(*cntr))
        .map(|(_, attrs)| attrs)
        .cloned();

    let group_plan = sampling_group_plan(counters, leader_cntr, max_counters_in_group);

    // Every group in the plan opens its own sampling leader on this CPU, so the
    // rate each one may ask for depends on how many there are.
    let sample_freq = group_sample_freq(
        cycles_attrs.sample_freq,
        group_plan.len(),
        host_max_sample_rate(),
    );
    LAST_SAMPLE_FREQ_REQUESTED.store(
        cycles_attrs.sample_freq,
        std::sync::atomic::Ordering::Relaxed,
    );
    LAST_SAMPLE_FREQ.store(sample_freq, std::sync::atomic::Ordering::Relaxed);
    cycles_attrs.sample_freq = sample_freq;
    if let Some(attrs) = leader_attrs.as_mut() {
        attrs.sample_freq = sample_freq;
    }
    let software_indices = counters
        .iter()
        .enumerate()
        .filter_map(|(index, counter)| counter.is_software().then_some(index))
        .collect::<Vec<_>>();

    let mut handles: Vec<NativeCounterHandle> = vec![];

    // The ring owner carries the sampling configuration for its whole group.
    // Every other member is read through it, so nothing else is left able to
    // overflow.
    if has_leader {
        make_counting_member(&mut cycles_attrs);
    }
    make_counting_member(&mut instr_attrs);
    for index in group_plan
        .iter()
        .flat_map(|group| &group.hardware_indices)
        .chain(&software_indices)
    {
        make_counting_member(&mut attrs[*index]);
    }

    for group in group_plan {
        let cycles_leader_fd = if has_leader {
            let mut leader_attr = leader_attrs.ok_or_else(|| {
                Error::InvalidConfiguration("configured sampling leader is missing".to_owned())
            })?;
            let leader_counter = leader_cntr.ok_or_else(|| {
                Error::InvalidConfiguration("configured sampling leader is missing".to_owned())
            })?;
            let leader_fd = unsafe { sys::perf_event_open(&mut leader_attr, pid, cpu, -1, 0) };
            push_handle(&mut handles, leader_fd, leader_counter.clone(), true, cpu)?;
            leader_fd
        } else {
            -1
        };

        let cycles_fd =
            unsafe { sys::perf_event_open(&mut cycles_attrs, pid, cpu, cycles_leader_fd, 0) };

        let leader_fd = if has_leader {
            cycles_leader_fd
        } else {
            cycles_fd
        };

        push_handle(&mut handles, cycles_fd, Counter::Cycles, !has_leader, cpu)?;

        let instr_fd = unsafe { sys::perf_event_open(&mut instr_attrs, pid, cpu, leader_fd, 0) };

        push_handle(&mut handles, instr_fd, Counter::Instructions, false, cpu)?;

        for index in group.hardware_indices {
            let cntr = &counters[index];
            let attrs = &mut attrs[index];
            let new_fd = unsafe { sys::perf_event_open(&mut *attrs, pid, cpu, leader_fd, 0) };
            push_handle(&mut handles, new_fd, cntr.clone(), false, cpu)?;
        }

        if !group.include_software {
            continue;
        }
        for &index in &software_indices {
            let cntr = &counters[index];
            let attrs = &mut attrs[index];
            let new_fd = unsafe { sys::perf_event_open(&mut *attrs, pid, cpu, leader_fd, 0) };
            push_handle(&mut handles, new_fd, cntr.clone(), false, cpu)?;
        }
    }

    Ok(handles)
}

/// Strip the sampling configuration from a group member.
///
/// Only the handle that owns the ring buffer needs to overflow: every other
/// counter in the group is reported through the leader's `PERF_SAMPLE_READ`.
/// Leaving a member configured to sample costs one PMU interrupt per member
/// per period whose record has nowhere to be written, so a group of N events
/// perturbs the workload N times harder than the caller asked for and records
/// nothing extra for it.
fn make_counting_member(attr: &mut perf_event_attr) {
    attr.sample_freq = 0;
    attr.set_freq(0);
    attr.set_precise_ip(0);
    attr.sample_type = 0;
    attr.sample_regs_user = 0;
    attr.sample_stack_user = 0;
    attr.branch_sample_type = 0;
    attr.set_mmap(0);
}

fn is_topdown(counter: &Counter) -> bool {
    matches!(counter, Counter::Custom(name) if crate::is_topdown_event(name))
}

/// Open the fixed-topdown group for one task on one CPU.
///
/// The whole event set forms a single scheduling domain: a level-one breakdown
/// is only meaningful when every counter in it was enabled over exactly the
/// same interval. Intel PERF_METRICS additionally demands that `slots` leads
/// the group and that neither the leader nor the metric events sample, so the
/// cycles sibling owns the ring buffer and reports the rest through its
/// grouped read.
fn grouped_topdown_on_cpu(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: i32,
    cpu: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    let sampled = counters
        .iter()
        .position(|counter| *counter == Counter::Cycles)
        .ok_or_else(|| {
            Error::InvalidConfiguration("a topdown group must sample cycles".to_owned())
        })?;
    let leader = counters
        .iter()
        .position(|counter| matches!(counter, Counter::Custom(name) if name == crate::GROUP_LEADER))
        .unwrap_or(sampled);

    for (index, attr) in attrs.iter_mut().enumerate() {
        if index != sampled {
            make_counting_member(attr);
        }
    }

    let mut handles = Vec::with_capacity(counters.len());
    let leader_fd = unsafe { sys::perf_event_open(&mut attrs[leader], pid, cpu, -1, 0) };
    push_handle_with(
        &mut handles,
        leader_fd,
        counters[leader].clone(),
        true,
        leader == sampled,
        cpu,
    )?;
    for (index, (counter, attr)) in zip(counters, attrs).enumerate() {
        if index == leader {
            continue;
        }
        let fd = unsafe { sys::perf_event_open(attr, pid, cpu, leader_fd, 0) };
        push_handle_with(
            &mut handles,
            fd,
            counter.clone(),
            false,
            index == sampled,
            cpu,
        )?;
    }
    Ok(handles)
}

#[derive(Debug, PartialEq, Eq)]
struct SamplingGroupPlan {
    hardware_indices: Vec<usize>,
    include_software: bool,
}

/// Split hardware events into sampling groups while assigning software events
/// to exactly one authoritative stream. Repeating task-clock in every group
/// duplicates the same cumulative counter and overstates CPU occupancy.
fn sampling_group_plan(
    counters: &[Counter],
    leader_counter: Option<&Counter>,
    max_counters_in_group: usize,
) -> Vec<SamplingGroupPlan> {
    let hardware_indices = counters
        .iter()
        .enumerate()
        .filter_map(|(index, counter)| {
            (*counter != Counter::Cycles
                && *counter != Counter::Instructions
                && !counter.is_software()
                && leader_counter != Some(counter))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let chunk_size = max_counters_in_group.max(1);

    if hardware_indices.is_empty() {
        return vec![SamplingGroupPlan {
            hardware_indices,
            include_software: true,
        }];
    }

    hardware_indices
        .chunks(chunk_size)
        .enumerate()
        .map(|(group_index, indices)| SamplingGroupPlan {
            hardware_indices: indices.to_vec(),
            include_software: group_index == 0,
        })
        .collect()
}

/// Build a single software-event sampling group used when the hardware PMU is
/// unavailable. `cpu-clock` is the group leader and therefore owns the mmap
/// ring buffer that carries samples and grouped counter reads.
/// Open the software fallback sampling group for one task on one CPU.
pub fn grouped_software_on_cpu(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: i32,
    cpu: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    let Some(leader_index) = counters
        .iter()
        .position(|counter| *counter == Counter::CpuClock)
    else {
        return Err(Error::InvalidConfiguration(
            "software sampling fallback requires cpu_clock".to_owned(),
        ));
    };

    for (index, attr) in attrs.iter_mut().enumerate() {
        if index != leader_index {
            make_counting_member(attr);
        }
    }

    let mut handles = Vec::with_capacity(counters.len());
    let leader_fd = unsafe { sys::perf_event_open(&mut attrs[leader_index], pid, cpu, -1, 0) };
    push_handle(&mut handles, leader_fd, Counter::CpuClock, true, cpu)?;

    for (index, (counter, attr)) in zip(counters, attrs).enumerate() {
        if index == leader_index {
            continue;
        }
        let fd = unsafe { sys::perf_event_open(attr, pid, cpu, leader_fd, 0) };
        push_handle(&mut handles, fd, counter.clone(), false, cpu)?;
    }

    Ok(handles)
}

/// Open one coherent group containing every requested self-monitoring event.
/// The first event is the leader and owns the sampling mmap buffer.
pub fn grouped_all(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: Option<i32>,
) -> Result<Vec<NativeCounterHandle>, Error> {
    grouped_all_on_cpu(counters, attrs, pid.unwrap_or(0), -1)
}

fn grouped_all_on_cpu(
    counters: &[Counter],
    attrs: &mut [perf_event_attr],
    pid: i32,
    cpu: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    if counters.is_empty() || counters.len() != attrs.len() {
        return Err(Error::InvalidConfiguration(
            "a sampling group requires matching non-empty counters and attributes".to_owned(),
        ));
    }

    for attr in &mut attrs[1..] {
        make_counting_member(attr);
    }

    let mut handles = Vec::with_capacity(counters.len());
    let leader_fd = unsafe { sys::perf_event_open(&mut attrs[0], pid, cpu, -1, 0) };
    push_handle(&mut handles, leader_fd, counters[0].clone(), true, cpu)?;
    for (counter, attr) in zip(&counters[1..], &mut attrs[1..]) {
        let fd = unsafe { sys::perf_event_open(attr, pid, cpu, leader_fd, 0) };
        push_handle(&mut handles, fd, counter.clone(), false, cpu)?;
    }
    Ok(handles)
}

fn push_handle(
    handles: &mut Vec<NativeCounterHandle>,
    fd: i32,
    counter: Counter,
    leader: bool,
    cpu: i32,
) -> Result<(), Error> {
    push_handle_with(handles, fd, counter, leader, leader, cpu)
}

fn push_handle_with(
    handles: &mut Vec<NativeCounterHandle>,
    fd: i32,
    counter: Counter,
    leader: bool,
    sampled: bool,
    cpu: i32,
) -> Result<(), Error> {
    match get_native_handle(fd, counter, leader, sampled, cpu) {
        Ok(handle) => {
            handles.push(handle);
            Ok(())
        }
        Err(error) => {
            close_handles(handles);
            handles.clear();
            Err(error)
        }
    }
}

fn get_native_handle(
    fd: i32,
    cntr: Counter,
    leader: bool,
    sampled: bool,
    cpu: i32,
) -> Result<NativeCounterHandle, Error> {
    if fd < 0 {
        return Err(Error::perf_event_open(&cntr, (cpu >= 0).then_some(cpu)));
    }

    let mut id: u64 = 0;

    let result = unsafe { sys::ioctls::ID(fd, &mut id) };
    if result < 0 {
        let error = Error::perf_ioctl("ID", &cntr);
        unsafe { libc::close(fd) };
        return Err(error);
    }

    Ok(NativeCounterHandle {
        kind: cntr,
        core: None,
        id,
        fd,
        leader,
        sampled,
    })
}

fn close_handles(handles: &[NativeCounterHandle]) {
    for handle in handles {
        unsafe { libc::close(handle.fd) };
    }
}

/// The frequency the last opened sampling plan settled on, and what it was
/// asked for. Read by the driver so a lowered rate reaches the user instead of
/// quietly changing what a recording means.
pub static LAST_SAMPLE_FREQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static LAST_SAMPLE_FREQ_REQUESTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The host's ceiling on samples per second per CPU, from
/// `perf_event_max_sample_rate`.
fn host_max_sample_rate() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|rate| *rate > 0)
}

/// The per-group sampling frequency to ask for, given how many sampling groups
/// this plan opens on one CPU.
///
/// Every group has its own sampling leader, so `groups` leaders at `requested`
/// Hz ask the CPU for `groups * requested` interrupts a second. Past
/// `perf_event_max_sample_rate` the kernel throttles, and throttling stops the
/// whole group while its `time_running` keeps accruing: the counters then
/// accumulate almost nothing and the recording is quietly off by orders of
/// magnitude rather than merely sparse. Measured on a 48-core Zen with a
/// 1000 Hz cap, a TMA recording asking 3x1000 Hz reported 4.3M cycles for a
/// 3-second run that actually retired 8.8G.
///
/// Four fifths of the ceiling leaves room for the other sampling events a
/// scenario may open, such as precise memory sampling.
fn group_sample_freq(requested: u64, groups: usize, max_rate: Option<u64>) -> u64 {
    let Some(max_rate) = max_rate else {
        return requested;
    };
    let groups = groups.max(1) as u64;
    let budget = (max_rate * 4 / 5 / groups).max(1);
    requested.min(budget)
}

/// Hardware counters no sampling group can use: one for an active NMI
/// watchdog, which the kernel holds for as long as it is enabled.
///
/// Counters a concurrent counting driver holds are deliberately not subtracted
/// here. Shrinking the per-group budget does not reduce PMU pressure, it
/// raises it: every group the plan splits into re-opens cycles and
/// instructions, so k groups cost `2k + optional` counters where one group
/// costs `2 + optional`. Scenarios that count and sample at once (mem,
/// roofline) split into eight three-event groups on a 14-counter PMU and then
/// nothing was scheduled at all. The kernel time-slices an over-subscribed
/// group set on its own, and `MeasurementQuality::Scaled` already reports the
/// resulting scaling.
fn reserved_hardware_counters() -> usize {
    std::fs::read_to_string("/proc/sys/kernel/nmi_watchdog")
        .map(|value| value.trim() == "1")
        .unwrap_or(false) as usize
}

#[cfg(test)]
mod tests {
    use super::{group_sample_freq, sampling_group_plan};
    use crate::Counter;

    #[test]
    fn sampling_rate_stays_under_the_host_ceiling() {
        // One group may use four fifths of the ceiling; three groups split it,
        // because each one interrupts the CPU at its own rate.
        assert_eq!(group_sample_freq(1000, 1, Some(1000)), 800);
        assert_eq!(group_sample_freq(1000, 3, Some(1000)), 266);
        // A host that allows more than we ask for does not raise the rate.
        assert_eq!(group_sample_freq(1000, 1, Some(100_000)), 1000);
        assert_eq!(group_sample_freq(1000, 3, Some(100_000)), 1000);
        // An unreadable ceiling leaves the request alone rather than guessing.
        assert_eq!(group_sample_freq(1000, 4, None), 1000);
        // A ceiling too small to divide still yields a usable rate.
        assert_eq!(group_sample_freq(1000, 8, Some(4)), 1);
    }

    #[test]
    fn software_counters_have_one_authoritative_hardware_group() {
        let counters = vec![
            Counter::Cycles,
            Counter::Instructions,
            Counter::LLCReferences,
            Counter::LLCMisses,
            Counter::BranchInstructions,
            Counter::CpuClock,
            Counter::CpuMigrations,
        ];

        let plan = sampling_group_plan(&counters, None, 1);

        assert_eq!(plan.len(), 3);
        assert!(plan[0].include_software);
        assert!(plan[1..].iter().all(|group| !group.include_software));
        assert_eq!(
            plan.iter().filter(|group| group.include_software).count(),
            1
        );
    }

    #[test]
    fn base_hardware_group_still_owns_software_without_extra_events() {
        let counters = vec![Counter::Cycles, Counter::Instructions, Counter::CpuClock];

        let plan = sampling_group_plan(&counters, None, 3);

        assert_eq!(plan.len(), 1);
        assert!(plan[0].hardware_indices.is_empty());
        assert!(plan[0].include_software);
    }
}
