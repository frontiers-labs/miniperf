//! Windows host clock telemetry.

use super::*;
use std::mem::size_of;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessorPowerInformation {
    number: u32,
    max_mhz: u32,
    current_mhz: u32,
    limit_mhz: u32,
    max_idle_state: u32,
    current_idle_state: u32,
}

#[link(name = "PowrProf")]
unsafe extern "system" {
    fn CallNtPowerInformation(
        level: u32,
        input: *const std::ffi::c_void,
        input_len: u32,
        output: *mut std::ffi::c_void,
        output_len: u32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetActiveProcessorCount(group: u16) -> u32;
}

/// Windows processor frequency telemetry from the power information API.
pub struct HostTelemetry {
    count: usize,
    unavailable: Vec<(&'static str, String)>,
}

impl HostTelemetry {
    /// Source label for Windows host power telemetry and availability.
    pub fn source_name() -> &'static str {
        "windows_power"
    }

    /// Discover logical processors whose current frequency can be queried.
    pub fn start(_clusters: &[(String, Vec<u32>)]) -> io::Result<Option<Self>> {
        let count = unsafe { GetActiveProcessorCount(0xffff) } as usize;
        if count == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
                count,
                unavailable: vec![("temperature", "Per-core temperature is unavailable through the Windows power API; exposed ACPI thermal zones are sampled by the Windows resource collector".into()), ("throttle_events", "Windows does not expose a generic CPU throttle counter".into()), ("device_clocks", "no supported GPU/NPU clock provider is available".into())],
            }))
    }
    /// Signals unavailable through this Windows telemetry adapter.
    pub fn unavailable(&self) -> &[(&'static str, String)] {
        &self.unavailable
    }
    /// Number of readings discarded by this adapter.
    pub fn discarded_readings(&self) -> u64 {
        0
    }
    /// Read the current processor frequencies.
    pub fn sample(&mut self) -> io::Result<HostTelemetrySample> {
        let mut values = vec![ProcessorPowerInformation::default(); self.count];
        let result = unsafe {
            CallNtPowerInformation(
                11,
                std::ptr::null(),
                0,
                values.as_mut_ptr().cast(),
                (size_of::<ProcessorPowerInformation>() * values.len()) as u32,
            )
        };
        let clusters = if result == 0 {
            let readings: Vec<f64> = values
                .iter()
                .filter_map(|v| (v.current_mhz > 0).then_some(v.current_mhz as f64 * 1e6))
                .collect();
            if readings.is_empty() {
                Vec::new()
            } else {
                let max_hz = values
                    .iter()
                    .map(|v| v.max_mhz)
                    .max()
                    .filter(|v| *v > 0)
                    .map(|v| v as f64 * 1e6);
                vec![ClusterClocks {
                    id: "host".into(),
                    mean_hz: readings.iter().sum::<f64>() / readings.len() as f64,
                    peak_hz: readings.iter().copied().fold(0.0, f64::max),
                    max_hz,
                    source: "windows_power",
                }]
            }
        } else {
            Vec::new()
        };
        Ok(HostTelemetrySample {
            clusters,
            ..Default::default()
        })
    }
}
