use perf_event_open_sys::{self as sys, bindings::perf_event_attr};

use crate::{Counter, Error};

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

/// Open one planned sampling group for a task on one CPU. The first event
/// leads; `sampled` indexes the one that owns the ring.
pub fn open_group(
    events: &mut [(Counter, perf_event_attr)],
    sampled: usize,
    pid: i32,
    cpu: i32,
) -> Result<Vec<NativeCounterHandle>, Error> {
    let mut handles: Vec<NativeCounterHandle> = Vec::with_capacity(events.len());
    for (index, (counter, attr)) in events.iter_mut().enumerate() {
        let leader_fd = handles.first().map_or(-1, |leader| leader.fd);
        let fd = unsafe { sys::perf_event_open(attr, pid, cpu, leader_fd, 0) };
        match get_native_handle(fd, counter.clone(), index == 0, index == sampled, cpu) {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                close_handles(&handles);
                return Err(error);
            }
        }
    }
    Ok(handles)
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

pub fn close_handles(handles: &[NativeCounterHandle]) {
    for handle in handles {
        unsafe { libc::close(handle.fd) };
    }
}

/// The host's ceiling on samples per second per CPU, from
/// `perf_event_max_sample_rate`.
pub fn host_max_sample_rate() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|rate| *rate > 0)
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
pub fn reserved_hardware_counters() -> usize {
    std::fs::read_to_string("/proc/sys/kernel/nmi_watchdog")
        .map(|value| value.trim() == "1")
        .unwrap_or(false) as usize
}
