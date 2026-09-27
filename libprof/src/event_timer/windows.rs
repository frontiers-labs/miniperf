//! Windows thread-accounting timer backend.

use super::ReadMethod;
use crate::{Counter, Error};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

pub(super) struct Snapshot {
    pub(super) values: Vec<u64>,
    pub(super) time_enabled: u64,
    pub(super) time_running: u64,
}

pub(super) struct Backend {
    counters: Vec<Counter>,
}

impl Backend {
    pub(super) fn new(counters: &[Counter]) -> Result<Self, Error> {
        for counter in counters {
            if !matches!(counter, Counter::Cycles | Counter::CpuClock) {
                return Err(Error::UnsupportedCounter {
                    counter: counter.name().to_owned(),
                    family: "Windows thread accounting".to_owned(),
                });
            }
        }
        Ok(Self {
            counters: counters.to_vec(),
        })
    }

    pub(super) fn method(&self) -> ReadMethod {
        ReadMethod::WindowsApi
    }

    pub(super) fn snapshot(&self) -> Result<Snapshot, Error> {
        let thread = unsafe { GetCurrentThread() };
        let mut cycles = 0_u64;
        let (mut created, mut exited, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = unsafe { std::mem::zeroed() };
        if self.counters.contains(&Counter::Cycles)
            && unsafe { QueryThreadCycleTime(thread, &mut cycles) } == 0
        {
            return Err(Error::InvalidConfiguration(format!(
                "QueryThreadCycleTime failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if self.counters.contains(&Counter::CpuClock)
            && unsafe { GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) }
                == 0
        {
            return Err(Error::InvalidConfiguration(format!(
                "GetThreadTimes failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let ticks =
            |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
        let cpu_ns = ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100);
        let values = self
            .counters
            .iter()
            .map(|counter| match counter {
                Counter::Cycles => cycles,
                Counter::CpuClock => cpu_ns,
                _ => unreachable!(),
            })
            .collect();
        Ok(Snapshot {
            values,
            time_enabled: 0,
            time_running: 0,
        })
    }
}
