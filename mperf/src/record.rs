use anyhow::{Context, Result, bail};
use mperf_data::{CpuClockSource, RecordInfo, ScenarioInfo};
use std::{collections::HashSet, fs::File, path::Path, rc::Rc, sync::Arc};

use libprof::{Process, Record, SessionContext, Sink, probe_sampling_group};

use crate::{
    Scenario, counter_selection::get_tma_counter_groups, event_dispatcher::EventDispatcher,
    postprocess::perform_postprocessing, roofline, source::Pass,
};

/// Tells the user when the host's sampling ceiling forced the rate down, before
/// the run rather than after it: a recording made at a fraction of the
/// requested rate is thinner than the caller asked for, and saying so only in
/// the collector status at the end is too late to act on.
fn warn_if_sample_rate_lowered(pass: &crate::source::ResolvedPass) {
    if let Some((effective, requested)) = pass.lowered_sample_rate() {
        eprintln!(
            "Warning: sampling at {effective} Hz, not the requested {requested} Hz: this host's \
             perf_event_max_sample_rate is {} Hz and the scenario's counters need more than one \
             group, which share it. Raise it with `sudo sysctl -w kernel.perf_event_max_sample_rate=<n>` \
             for a denser profile.",
            std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
                .map(|value| value.trim().to_owned())
                .unwrap_or_else(|_| "unknown".to_owned())
        );
    }
}

/// How long the pre-flight group probe samples for, in milliseconds.
///
/// Matches `mperf doctor`, which probes the same groups: short enough to be
/// invisible next to a profiling run, long enough that a working PMU always
/// delivers samples.
const GROUP_PROBE_MILLIS: u64 = 200;

/// Refuses to record a scenario whose sampling group carries no hardware
/// counter.
///
/// Such a run still exits 0 and still writes every parquet file, but the
/// recording has no `pmu_*` columns: no IPC, no topdown, no counter-weighted
/// hotspots. Every question the scenario exists to answer comes back empty from
/// a file that looks complete, which is worse than no recording at all. The
/// group is probed rather than the host capability, because a host can open
/// `cycles` on its own and still refuse the scenario's group.
fn reject_software_only_group(scenario: Scenario) -> Result<()> {
    let counters = crate::source::scenario_counters(scenario);
    let probe = probe_sampling_group(&counters, GROUP_PROBE_MILLIS).with_context(|| {
        format!(
            "could not probe the {} sampling group; run `mperf doctor` for the cause on this host",
            scenario_name(scenario)
        )
    })?;

    if probe.collapsed_to_software() {
        bail!(
            "no hardware counter opened for the {} scenario, so this recording would carry \
             software events only and no pmu_* data.\n\
             Run `mperf doctor` for the cause on this host.",
            scenario_name(scenario)
        );
    }
    Ok(())
}

fn scenario_name(scenario: Scenario) -> String {
    format!("{scenario:?}").to_lowercase()
}

pub async fn do_record(
    scenario: Scenario,
    output_directory: &Path,
    pid: Option<u32>,
    command: Vec<String>,
    roofline_options: roofline::Options,
    duration: Option<std::time::Duration>,
) -> Result<()> {
    println!("Record profile with {scenario:?} scenario");

    let fidelity = crate::source::resolve_fidelity(scenario);
    println!(
        "Capture fidelity: {} at '{}'{}",
        fidelity.scenario,
        fidelity.rung,
        fidelity
            .rejected
            .first()
            .map(|rejected| format!(" ({})", rejected.reason))
            .unwrap_or_default()
    );

    // Snapshot and TMA exist to report hardware counters. Mem and Roofline keep
    // their primary artifact, the instrumentation trace, without a PMU, so they
    // are not rejected here.
    if matches!(scenario, Scenario::Snapshot | Scenario::TMA) {
        reject_software_only_group(scenario)?;
    }

    let cpu_info = if scenario == Scenario::Mem
        && roofline::uses_native_performance(&roofline_options, &command)?
    {
        println!("Calibrating sustainable host memory bandwidth...");
        let calibration = roofline::calibrate_memory_host()?;
        println!(
            "Host memory ceiling: {:.2} GB/s ({} Rayon threads)",
            calibration.gbytes_per_second, calibration.threads
        );
        mperf_data::CpuInfo {
            memory_calibration: Some(Box::new(calibration)),
            roofline_calibration: None,
        }
    } else if scenario == Scenario::Roofline
        && roofline::uses_native_performance(&roofline_options, &command)?
    {
        println!("Calibrating host Roofline ceilings...");
        let calibration = roofline::calibrate_host()?;
        println!(
            "Host ceilings: {:.2} GFLOP/s FP64, {:.2} GB/s memory ({} Rayon threads)",
            calibration.fp64_gflops, calibration.memory_gbytes_per_second, calibration.threads
        );
        if let Some(single) = &calibration.single_thread {
            println!(
                "Single-thread ceilings: {:.2} GFLOP/s FP64, {:.2} GB/s memory",
                single.fp64_gflops, single.memory_gbytes_per_second
            );
        }
        mperf_data::CpuInfo {
            memory_calibration: Some(Box::new((&calibration).into())),
            roofline_calibration: Some(Box::new(calibration)),
        }
    } else {
        if matches!(scenario, Scenario::Roofline | Scenario::Mem) {
            println!(
                "Skipping host Roofline calibration: the selected method does not measure native performance"
            );
        }
        mperf_data::CpuInfo::default()
    };

    let (dispatcher, join_handle) = EventDispatcher::new(output_directory);

    let (info, collectors) = match scenario {
        Scenario::Snapshot => snapshot(
            dispatcher.clone(),
            pid,
            &command,
            output_directory,
            duration,
        )?,
        Scenario::Mem => {
            if pid.is_some() {
                anyhow::bail!("record mem requires a command and does not support --pid");
            }
            (
                roofline::record_memory(
                    &roofline_options,
                    dispatcher.clone(),
                    &command,
                    output_directory,
                )
                .await?,
                Vec::new(),
            )
        }
        Scenario::Roofline => (
            roofline::record(
                &roofline_options,
                dispatcher.clone(),
                &command,
                output_directory,
            )
            .await?,
            Vec::new(),
        ),
        Scenario::TMA => topdown(dispatcher.clone(), &command, output_directory)?,
    };

    drop(dispatcher);

    join_handle.join().await;

    let json_command = if !command.is_empty() {
        Some(command.clone())
    } else {
        None
    };

    let (cpu_vendor, cpu_model) = libprof::host_cpu_description();

    let cores = libprof::host_core_clusters()
        .into_iter()
        .map(|c| mperf_data::CoreCluster {
            family_id: c.family_id,
            name: c.name,
            cpus: c.cpus,
        })
        .collect();

    let ri = RecordInfo {
        format_version: mperf_data::CURRENT_FORMAT_VERSION,
        scenario,
        command: json_command,
        cpu_model,
        cpu_vendor,
        sampling_frequency_hz: Some(if scenario == Scenario::Snapshot {
            crate::source::SNAPSHOT_SAMPLE_FREQUENCY_HZ
        } else {
            libprof::DEFAULT_SAMPLE_FREQUENCY_HZ
        }),
        cpu_clock_source: Some(if cfg!(any(target_os = "macos", target_os = "linux")) {
            CpuClockSource::SampledOccupancy
        } else {
            CpuClockSource::CounterDelta
        }),
        logical_cpu_count: host_logical_cpu_count(),
        cores,
        cpu_info,
        capture_fidelity: vec![fidelity],
        collectors,
        scenario_info: info,
    };

    {
        let mut info_file = File::create(output_directory.join("info.json"))?;
        serde_json::to_writer(&mut info_file, &ri)?;
    }

    println!("Postprocessing...");
    kdam::term::init(false);
    kdam::term::hide_cursor()?;

    let pb = kdam::tqdm!(total = 100);
    perform_postprocessing(output_directory, pb).await?;

    kdam::term::show_cursor()?;

    Ok(())
}

fn host_logical_cpu_count() -> Option<u32> {
    let configured = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    (configured > 0)
        .then(|| u32::try_from(configured).ok())
        .flatten()
}

fn snapshot(
    dispatcher: Arc<EventDispatcher>,
    pid: Option<u32>,
    command: &[String],
    output_directory: &Path,
    duration: Option<std::time::Duration>,
) -> Result<(ScenarioInfo, Vec<mperf_data::SnapshotCollectorStatus>)> {
    if pid.is_none() && command.is_empty() {
        anyhow::bail!("record snapshot requires a command or --pid");
    }

    let pass = Pass {
        name: "snapshot",
        required: vec![Box::new(crate::source::pmu_sampling_source(
            Scenario::Snapshot,
        ))],
        optional: vec![
            Box::new(libprof::InternalEventsSource {
                roofline_instrumented: false,
            }),
            Box::new(libprof::HostTelemetrySource::default()),
            Box::new(libprof::ProcfsSource::default()),
            Box::new(libprof::BpfSource::default()),
        ],
    };
    let mut pass = pass.resolve(output_directory)?;
    let child_env = pass.child_environment(output_directory);

    let process = if pid.is_none() {
        Some(Rc::new(Process::new(command, &child_env)?))
    } else {
        None
    };

    let context = SessionContext {
        directory: output_directory.to_owned(),
        sink: dispatcher.clone(),
        process: process.clone(),
        attached_pid: pid,
    };
    let recorded_pid = context.root_pid();
    // A launched macOS child is already exec'd and suspended, so its dyld
    // mappings exist before the first instruction is profiled. Attached
    // processes are live on every platform. Elsewhere the mappings only appear
    // once the child is released, and the polling loop picks them up.
    if cfg!(target_os = "macos") || pid.is_some() {
        publish_process_maps(&dispatcher, recorded_pid);
    }

    pass.start(&context)?;
    let stop_reason = wait_for_target(&dispatcher, process.as_deref(), recorded_pid, duration)?;
    let statuses = pass.stop(&context);
    warn_if_sample_rate_lowered(&pass);
    let recorded_counters = pass.recorded_counters();
    let collectors: Vec<mperf_data::SnapshotCollectorStatus> = statuses
        .into_iter()
        .filter(|status| status.name != "pmu_sampling" || status.status != "available")
        .collect();

    let warnings = collectors
        .iter()
        .filter(|collector| collector.status != "available")
        .map(|collector| format!("{}: {}", collector.name, collector.message))
        .collect();

    // A host that exposes its process tree measures every descendant; one that
    // does not measures the root, and the scope says which happened.
    let tree = libprof::process_tree(recorded_pid).is_some();
    Ok((
        ScenarioInfo::Snapshot(mperf_data::SnapshotInfo {
            pid: recorded_pid as i32,
            counters: recorded_counters,
            scope: match (tree, pid.is_some()) {
                (true, true) => "attached_tree_best_effort",
                (true, false) => "launched_tree_inherited",
                (false, _) => "legacy_root_only",
            }
            .to_string(),
            interval_ms: if tree { 1_000 } else { 0 },
            stop_reason,
            collectors: collectors.clone(),
            warnings,
        }),
        collectors,
    ))
}

/// Wait for the measured workload to finish, publishing module maps for every
/// process that joins it, and report why the recording stopped.
fn wait_for_target(
    dispatcher: &Arc<EventDispatcher>,
    process: Option<&Process>,
    root_pid: u32,
    duration: Option<std::time::Duration>,
) -> Result<String> {
    let started = std::time::Instant::now();
    let mut known_pids = HashSet::new();
    let mut root_waited = false;
    if let Some(process) = process {
        process.cont();
    }
    loop {
        let tree = libprof::process_tree(root_pid);
        for member in tree.iter().flatten() {
            if known_pids.insert((member.pid, member.start_ticks)) {
                publish_process_maps(dispatcher, member.pid);
            }
        }
        // Collect a launched root's exit status once it is gone. It stays a
        // zombie on purpose, so its final resource accounting is still
        // queryable; the session reaps it on drop.
        if !root_waited && !root_alive(&tree, process, root_pid)? {
            if let Some(process) = process {
                process.wait()?;
            }
            root_waited = true;
        }
        let live = match &tree {
            Some(tree) => tree.iter().any(|member| member.state != b'Z'),
            None => !root_waited,
        };
        if root_waited && !live {
            return Ok(if tree.is_some() {
                "tree_exit"
            } else {
                "root_exit"
            }
            .to_string());
        }
        if duration.is_some_and(|limit| started.elapsed() >= limit) {
            if let Some(process) = process {
                // A launched command belongs to this recording. Terminate every
                // member still observed so a bounded snapshot cannot leave an
                // unexpected orphan workload behind.
                terminate(process, root_pid);
                if !root_waited {
                    process.wait()?;
                }
            }
            return Ok("duration".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Whether the root process is still running. A launched child is asked
/// directly, because a zombie of ours still answers `kill(pid, 0)`.
fn root_alive(
    tree: &Option<Vec<libprof::ProcessStat>>,
    process: Option<&Process>,
    root_pid: u32,
) -> Result<bool> {
    if let Some(tree) = tree {
        return Ok(tree
            .iter()
            .any(|member| member.pid == root_pid && member.state != b'Z'));
    }
    match process {
        Some(process) => Ok(!process.try_wait()?),
        None => Ok(libprof::process_alive(root_pid)),
    }
}

/// Ask the tree to exit, then insist. Members that ignore SIGTERM would
/// otherwise outlive the recording that started them.
fn terminate(process: &Process, root_pid: u32) {
    let signal = |signal: i32| {
        for member in libprof::process_tree(root_pid)
            .unwrap_or_else(|| {
                vec![libprof::ProcessStat {
                    pid: root_pid,
                    ..Default::default()
                }]
            })
            .iter()
            .filter(|member| member.state != b'Z')
        {
            unsafe { libc::kill(member.pid as i32, signal) };
        }
    };
    signal(libc::SIGTERM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        // Without a process tree the root is the only thing to wait on, and
        // asking it directly is what keeps a well-behaved child from costing
        // the full grace period.
        let gone = match libprof::process_tree(root_pid) {
            Some(tree) => tree
                .iter()
                .find(|member| member.pid == root_pid)
                .is_none_or(|member| member.state == b'Z'),
            None => process.try_wait().unwrap_or(true),
        };
        if gone {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    signal(libc::SIGKILL);
}

/// Publish a live process's executable mappings so its samples can be
/// symbolized even when perf never reported them.
fn publish_process_maps(dispatcher: &Arc<EventDispatcher>, pid: u32) {
    for module in libprof::process_modules(pid) {
        dispatcher.record(Record::ProcAddr(module));
    }
}

fn topdown(
    dispatcher: Arc<EventDispatcher>,
    command: &[String],
    output_directory: &Path,
) -> Result<(ScenarioInfo, Vec<mperf_data::SnapshotCollectorStatus>)> {
    let scenario = libprof::tma_scenario().context("TMA is not supported on this CPU")?;
    // Validate the formula groups, but do not turn each one into an independent
    // sampling leader. Multiple cycle leaders multiply the interrupt rate and
    // severely perturb the workload (especially while capturing DWARF stacks).
    // The original TMA collector sampled the deduplicated event set once.
    get_tma_counter_groups(&scenario)?;

    // TMA uses the same sampling engine and attribution mode as Snapshot.
    // Only the counter set differs.
    // Precise memory samples (PEBS/SPE) run in their own event slots and do
    // not compete with the topdown counter group, so a TMA recording gets
    // instruction-level memory attribution for free where the host has it.
    let pass = Pass {
        name: "tma",
        required: vec![Box::new(crate::source::pmu_sampling_source(Scenario::TMA))],
        optional: vec![
            Box::new(libprof::InternalEventsSource {
                roofline_instrumented: false,
            }),
            Box::new(libprof::PreciseMemorySource::default()),
            Box::new(libprof::HostTelemetrySource::default()),
        ],
    };
    let mut pass = pass.resolve(output_directory)?;
    let child_env = pass.child_environment(output_directory);
    let process = Rc::new(Process::new(command, &child_env)?);
    let context = SessionContext {
        directory: output_directory.to_owned(),
        sink: dispatcher.clone(),
        process: Some(process.clone()),
        attached_pid: None,
    };
    let recorded_pid = process.pid();
    if cfg!(target_os = "macos") {
        publish_process_maps(&dispatcher, recorded_pid as u32);
    }

    pass.start(&context)?;
    warn_if_sample_rate_lowered(&pass);
    let recorded_counters = pass.recorded_counters();
    let missing = scenario
        .events
        .iter()
        .filter(|event| !recorded_counters.iter().any(|(_, name)| name == *event))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        pass.stop(&context);
        anyhow::bail!(
            "TMA needs hardware counters this host cannot open ({}); run `mperf doctor`, or use `record -s snapshot`",
            missing.join(", ")
        );
    }

    process.cont();
    std::thread::sleep(std::time::Duration::from_millis(20));
    publish_process_maps(&dispatcher, recorded_pid as u32);
    process.wait()?;
    let statuses = pass.stop(&context);
    for status in statuses
        .iter()
        .filter(|status| status.status != "available")
    {
        eprintln!("Warning: {}: {}", status.name, status.message);
    }

    Ok((
        ScenarioInfo::TMA(mperf_data::TMAInfo {
            pid: recorded_pid,
            counters: recorded_counters,
            groups: scenario.groups,
            precise_attribution: scenario.precise_attribution,
            metrics: scenario.metrics,
            constants: scenario.constants,
            ui: scenario.ui,
        }),
        statuses,
    ))
}
