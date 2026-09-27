//! Windows TMA capture through WPR's coherent CSwitch PMC vectors.

#[cfg(target_os = "windows")]
mod imp {

    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};
    use libprof::{
        Counter, CounterEntry, CounterResult, CounterValue, MeasurementQuality, Process, Sink,
    };
    use mperf_data::{ScenarioInfo, SnapshotCollectorStatus, TMAInfo};
    use serde::Deserialize;
    use smallvec::SmallVec;

    use crate::counter_selection::get_tma_counter_groups;
    use crate::event_dispatcher::EventDispatcher;
    use crate::record::publish_process_maps;

    #[derive(Deserialize)]
    struct ProfileSamples {
        qpc_frequency: u64,
        samples: Vec<ProfileSample>,
    }

    #[derive(Deserialize)]
    struct ProfileSample {
        cpu: u16,
        timestamp: i64,
        tid: u32,
        pid: u32,
        ip: u64,
    }

    fn publish_profile_samples(dispatcher: &EventDispatcher, path: &Path) -> Result<usize> {
        let data: ProfileSamples = serde_json::from_slice(&fs::read(path)?)?;
        if data.qpc_frequency == 0 {
            bail!("Windows TMA trace has no QPC frequency");
        }
        let count = data.samples.len();
        for (index, sample) in data.samples.into_iter().enumerate() {
            let time = ((sample.timestamp.max(0) as u128) * 1_000_000_000
                / data.qpc_frequency as u128)
                .min(u64::MAX as u128) as u64;
            dispatcher.record(libprof::Record::Sample(libprof::Sample {
                // The dispatcher uses this as the group ID. Each timer
                // interrupt is its own observation, not one giant group.
                event_id: index as u128 + 1,
                ip: sample.ip,
                pid: sample.pid,
                tid: sample.tid,
                cpu: sample.cpu as u32,
                core: None,
                time,
                time_enabled: 1,
                time_running: 1,
                counter: Counter::CpuClock,
                value: 1,
                callstack: SmallVec::new(),
                lbr_callstack: SmallVec::new(),
                user_regs: None,
                user_stack: Vec::new(),
            }));
        }
        Ok(count)
    }

    fn wpr(args: &[String]) -> Result<Output> {
        let mut busy_retries = 0;
        loop {
            let output = Command::new("wpr")
                .args(args)
                .output()
                .context("Windows Performance Recorder (wpr.exe) is required for TMA capture")?;
            if output.status.success() {
                return Ok(output);
            }
            // Stopping a preceding WPR capture can return before the kernel
            // releases its system-wide PMC reservation. Retry that one error
            // for a bounded period; a genuinely competing logger still fails.
            if args.first().is_some_and(|arg| arg == "-start")
                && output
                    .status
                    .code()
                    .is_some_and(|code| code as u32 == 0x8007_00aa)
                && busy_retries < 20
            {
                busy_retries += 1;
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            bail!(
                "wpr {} failed ({}): {}{}",
                args.first().map(String::as_str).unwrap_or(""),
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    struct ActiveWpr {
        instance: String,
        stopped: bool,
    }

    impl ActiveWpr {
        fn start(profile_path: &Path, profile_name: &str) -> Result<Self> {
            let instance = format!("miniperf-{}", uuid::Uuid::now_v7());
            let spec = format!("{}!{profile_name}.Verbose", profile_path.display());
            wpr(&[
                "-start".to_owned(),
                spec,
                "-filemode".to_owned(),
                "-instancename".to_owned(),
                instance.clone(),
            ])?;
            Ok(Self {
                instance,
                stopped: false,
            })
        }

        fn stop(&mut self, etl_path: &Path) -> Result<()> {
            wpr(&[
                "-stop".to_owned(),
                etl_path.display().to_string(),
                "-instancename".to_owned(),
                self.instance.clone(),
            ])?;
            self.stopped = true;
            Ok(())
        }
    }

    impl Drop for ActiveWpr {
        fn drop(&mut self) {
            if !self.stopped {
                // A failed workload or ETL export must not leave a system-wide
                // logger and programmed PMCs running in the user's session.
                let _ = Command::new("wpr")
                    .args(["-cancel", "-instancename", &self.instance])
                    .output();
            }
        }
    }

    fn stat_target(
        attached_pid: Option<u32>,
        command: &[String],
    ) -> Result<(Option<Process>, u32)> {
        let process = if command.is_empty() {
            None
        } else {
            Some(Process::new(command, &[]).context("could not launch stat command")?)
        };
        let target_pid = attached_pid
            .or_else(|| process.as_ref().map(|process| process.pid() as u32))
            .context("stat requires a command or --pid")?;
        Ok((process, target_pid))
    }

    fn run_stat_target(process: Option<&Process>, target_pid: u32) -> Result<()> {
        if let Some(process) = process {
            process.cont();
            process.wait().context("stat command failed")?;
        } else {
            while libprof::process_alive(target_pid) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    }

    pub(crate) fn topdown(
        dispatcher: Arc<EventDispatcher>,
        command: &[String],
        output_directory: &Path,
    ) -> Result<(ScenarioInfo, Vec<SnapshotCollectorStatus>)> {
        if command.is_empty() {
            bail!("record tma requires a command on Windows");
        }
        let scenario = libprof::tma_scenario().context("TMA is not supported on this CPU")?;
        get_tma_counter_groups(&scenario)?;
        let profile = libprof::windows_tma_profile()
            .context("this CPU's TMA event set cannot be represented by Windows WPR")?;
        let profile_path = output_directory.join("windows-tma.wprp");
        let etl_path = output_directory.join("windows-tma.etl");
        let intervals_path = output_directory.join("windows-tma-intervals.json");
        fs::write(&profile_path, &profile.xml)
            .context("could not write Windows TMA WPR profile")?;

        let process = Process::new(command, &[]).context("could not launch TMA workload")?;
        let pid = process.pid() as u32;
        publish_process_maps(&dispatcher, pid);
        let mut session = ActiveWpr::start(&profile_path, &profile.profile_name)
        .context("could not start Windows TMA PMC capture; an elevated token with SeSystemProfilePrivilege may be required")?;
        process.cont();
        thread::sleep(Duration::from_millis(20));
        publish_process_maps(&dispatcher, pid);
        let wait_result = process.wait();
        let stop_result = session.stop(&etl_path);
        wait_result.context("TMA workload failed")?;
        stop_result.context("could not stop or export the Windows TMA trace")?;

        let counter_order = profile
            .counters
            .iter()
            .map(|(_, counter)| counter.clone())
            .collect::<Vec<_>>();
        libprof::windows_decode_pmc_etl(&etl_path, &intervals_path, pid, &counter_order)
            .context("could not decode coherent Windows TMA counter intervals")?;
        let sample_count = publish_profile_samples(&dispatcher, &intervals_path)
            .context("could not publish Windows TMA instruction-pointer samples")?;
        if sample_count == 0 {
            bail!(
                "WPR trace contains no process-attributed SampledProfile events; function-level TMA is unavailable"
            );
        }
        let recorded_counters = profile
            .counters
            .iter()
            .map(|(name, counter)| (crate::utils::counter_to_event_ty(counter), name.clone()))
            .collect();
        Ok((
            ScenarioInfo::TMA(TMAInfo {
                pid: pid as i32,
                counters: recorded_counters,
                groups: scenario.groups,
                precise_attribution: false,
                metrics: scenario.metrics,
                constants: scenario.constants,
                ui: scenario.ui,
            }),
            vec![SnapshotCollectorStatus {
                name: "coherent_pmu".to_owned(),
                status: "available".to_owned(),
                source: "windows_wpr_etw_cswitch_sampled_profile".to_owned(),
                quality: "measured_intervals_sampled_functions".to_owned(),
                message: "WPR measured coherent TMA intervals and sampled instruction pointers for per-function estimates".to_owned(),
            }],
        ))
    }

    pub(crate) fn stat_topdown(
        attached_pid: Option<u32>,
        command: &[String],
    ) -> Result<(pmu_data::TmaScenario, CounterResult)> {
        let scenario = libprof::tma_scenario().context("TMA is not supported on this CPU")?;
        get_tma_counter_groups(&scenario)?;
        let profile = libprof::windows_tma_profile()
            .context("this CPU's TMA event set cannot be represented by Windows WPR")?;
        let directory = tempfile::tempdir().context("could not create WPR scratch directory")?;
        let profile_path = directory.path().join("windows-tma.wprp");
        let etl_path = directory.path().join("windows-tma.etl");
        fs::write(&profile_path, &profile.xml)
            .context("could not write Windows TMA WPR profile")?;
        let (process, target_pid) = stat_target(attached_pid, command)?;
        let mut session = ActiveWpr::start(&profile_path, &profile.profile_name)
            .context("could not start Windows TMA PMC capture; an elevated token with SeSystemProfilePrivilege may be required")?;
        let wait_result = run_stat_target(process.as_ref(), target_pid);
        let stop_result = session.stop(&etl_path);
        wait_result?;
        stop_result.context("could not stop or export the Windows TMA trace")?;
        let counter_order = profile
            .counters
            .iter()
            .map(|(_, counter)| counter.clone())
            .collect::<Vec<_>>();
        let totals = libprof::windows_pmc_etl_totals(&etl_path, target_pid, &counter_order)
            .context("could not decode Windows TMA counter totals")?;
        let entries: SmallVec<[CounterEntry; 16]> = totals
            .into_iter()
            .map(|(counter, value)| CounterEntry {
                core: None,
                counter,
                value: CounterValue {
                    value,
                    scaling: 1.0,
                    quality: MeasurementQuality::Estimated,
                },
            })
            .collect();
        Ok((scenario, CounterResult::from_entries(entries)))
    }

    pub(crate) fn stat_explicit(
        attached_pid: Option<u32>,
        command: &[String],
        requested: &[Counter],
    ) -> Result<CounterResult> {
        let pmu = requested
            .iter()
            .filter(|counter| !counter.is_software())
            .cloned()
            .collect::<Vec<_>>();
        let accounting = requested
            .iter()
            .filter(|counter| matches!(counter, Counter::CpuClock | Counter::PageFaults))
            .cloned()
            .collect::<Vec<_>>();
        let switches = requested
            .iter()
            .any(|counter| matches!(counter, Counter::ContextSwitches | Counter::CpuMigrations));
        let profile = libprof::windows_counter_profile(&pmu)
            .context("requested PMU events cannot be represented by Windows WPR")?;
        let directory = tempfile::tempdir().context("could not create WPR scratch directory")?;
        let profile_path = directory.path().join("windows-stat.wprp");
        let etl_path = directory.path().join("windows-stat.etl");
        fs::write(&profile_path, &profile.xml).context("could not write Windows WPR profile")?;
        let (process, target_pid) = stat_target(attached_pid, command)?;
        let mut accounting_driver = if accounting.is_empty() {
            None
        } else {
            let driver = libprof::CountingDriverBuilder::new()
                .counters(&accounting)
                .pid(Some(target_pid as i32))
                .build()
                .context("could not configure Windows process accounting")?;
            Some(driver)
        };
        let mut session = ActiveWpr::start(&profile_path, &profile.profile_name)
            .context("could not start Windows PMC capture; an elevated token with SeSystemProfilePrivilege may be required")?;
        if let Some(driver) = accounting_driver.as_mut() {
            driver.start()?;
        }
        let wait_result = run_stat_target(process.as_ref(), target_pid);
        let accounting_stop = accounting_driver.as_mut().map(|driver| driver.stop());
        let wpr_stop = session.stop(&etl_path);
        wait_result?;
        if let Some(result) = accounting_stop {
            result?;
        }
        wpr_stop.context("could not stop or export the Windows PMC trace")?;

        let counter_order = profile
            .counters
            .iter()
            .map(|(_, counter)| counter.clone())
            .collect::<Vec<_>>();
        let totals = libprof::windows_pmc_etl_totals(&etl_path, target_pid, &counter_order)
            .context("could not decode Windows PMC counter totals")?;
        let mut entries: SmallVec<[CounterEntry; 16]> = totals
            .into_iter()
            .map(|(counter, value)| CounterEntry {
                core: None,
                counter,
                value: CounterValue {
                    value,
                    scaling: 1.0,
                    quality: MeasurementQuality::Estimated,
                },
            })
            .collect();
        if let Some(driver) = accounting_driver.as_mut() {
            for counter in &accounting {
                if let Some(value) = driver.counters()?.get(counter.clone()) {
                    entries.push(CounterEntry {
                        core: None,
                        counter: counter.clone(),
                        value,
                    });
                }
            }
        }
        if switches {
            for (counter, value) in libprof::windows_switch_etl_totals(&etl_path, target_pid)
                .context("could not decode Windows context-switch totals")?
            {
                if requested.contains(&counter) {
                    entries.push(CounterEntry {
                        core: None,
                        counter,
                        value: CounterValue {
                            value,
                            scaling: 1.0,
                            quality: MeasurementQuality::Exact,
                        },
                    });
                }
            }
        }
        Ok(CounterResult::from_entries(entries))
    }

    #[cfg(test)]
    #[test]
    fn timer_samples_keep_independent_dispatcher_groups() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples.json");
        fs::write(
            &path,
            r#"{"qpc_frequency":1000000,"samples":[{"cpu":1,"timestamp":10,"tid":7,"pid":42,"ip":4096},{"cpu":1,"timestamp":20,"tid":7,"pid":42,"ip":8192}]}"#,
        )
        .unwrap();
        let (dispatcher, join) = EventDispatcher::new(directory.path());
        assert_eq!(publish_profile_samples(&dispatcher, &path).unwrap(), 2);
        drop(dispatcher);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(join.join());
        let session = store::Session::open(directory.path()).unwrap();
        let distinct: i64 = session
            .connection()
            .query_row(
                "SELECT COUNT(DISTINCT group_id) FROM samples_raw",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(distinct, 2);
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::stat_explicit;
#[cfg(target_os = "windows")]
pub(crate) use imp::stat_topdown;
#[cfg(target_os = "windows")]
pub(crate) use imp::topdown;

#[cfg(target_os = "windows")]
pub(crate) fn max_pmc_sources() -> Option<usize> {
    libprof::windows_max_pmc_sources()
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn max_pmc_sources() -> Option<usize> {
    None
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn topdown(
    _dispatcher: std::sync::Arc<crate::event_dispatcher::EventDispatcher>,
    _command: &[String],
    _output_directory: &std::path::Path,
) -> anyhow::Result<(
    mperf_data::ScenarioInfo,
    Vec<mperf_data::SnapshotCollectorStatus>,
)> {
    unreachable!("Windows TMA capture is selected only on Windows")
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn stat_topdown(
    _attached_pid: Option<u32>,
    _command: &[String],
) -> anyhow::Result<(pmu_data::TmaScenario, libprof::CounterResult)> {
    unreachable!("Windows TMA capture is selected only on Windows")
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn stat_explicit(
    _attached_pid: Option<u32>,
    _command: &[String],
    _requested: &[libprof::Counter],
) -> anyhow::Result<libprof::CounterResult> {
    unreachable!("Windows WPR capture is selected only on Windows")
}
