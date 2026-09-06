use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use clap::ValueEnum;
use libprof::{Counter, Process};
use mperf_data::{RooflineInfo, RooflineMethodInfo, ScenarioInfo};
use object::{Object, ObjectSymbol};

use crate::{Scenario, event_dispatcher::EventDispatcher, utils::counter_to_event_ty};

mod calibrate;
mod loops;
mod qemu;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum BackendKind {
    #[default]
    Auto,
    Compiler,
    Qemu,
    #[value(name = "dynamorio")]
    DynamoRio,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub backend: BackendKind,
    pub qemu: Option<PathBuf>,
    pub qemu_plugin: Option<PathBuf>,
    pub qemu_args: Vec<String>,
    pub dynamorio: Option<PathBuf>,
    pub dynamorio_client: Option<PathBuf>,
}

impl Options {
    pub fn validate_for(&self, scenario: Scenario) -> Result<()> {
        let has_qemu_options =
            self.qemu.is_some() || self.qemu_plugin.is_some() || !self.qemu_args.is_empty();
        let has_dynamorio_options = self.dynamorio.is_some() || self.dynamorio_client.is_some();
        if !matches!(scenario, Scenario::Roofline | Scenario::Mem)
            && self.backend != BackendKind::Auto
        {
            anyhow::bail!("--roofline-backend is only valid with the roofline or mem scenario");
        }
        if !matches!(scenario, Scenario::Roofline | Scenario::Mem)
            && (has_qemu_options || has_dynamorio_options)
        {
            anyhow::bail!(
                "--qemu, --qemu-plugin, --qemu-arg, --dynamorio, and --dynamorio-client are only valid with the roofline or mem scenario"
            );
        }
        if self.backend == BackendKind::Compiler && (has_qemu_options || has_dynamorio_options) {
            anyhow::bail!(
                "--qemu, --qemu-plugin, --qemu-arg, --dynamorio, and --dynamorio-client cannot be used with --roofline-backend compiler"
            );
        }
        if self.backend == BackendKind::Qemu && has_dynamorio_options {
            anyhow::bail!(
                "--dynamorio and --dynamorio-client cannot be used with --roofline-backend qemu"
            );
        }
        if self.backend == BackendKind::DynamoRio && has_qemu_options {
            anyhow::bail!(
                "--qemu, --qemu-plugin, and --qemu-arg cannot be used with --roofline-backend dynamorio"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceSource {
    Native,
    Qemu,
}

struct SelectedMethod {
    backend: BackendKind,
    performance: PerformanceSource,
    executable: PathBuf,
    info: RooflineMethodInfo,
}

type BackendFuture<'a> = Pin<Box<dyn Future<Output = Result<ScenarioInfo>> + 'a>>;

trait RooflineBackend {
    fn record<'a>(
        &'a self,
        dispatcher: Arc<EventDispatcher>,
        command: &'a [String],
        output_directory: &'a Path,
    ) -> BackendFuture<'a>;
}

struct CompilerBackend {
    method: RooflineMethodInfo,
}

pub async fn record(
    options: &Options,
    dispatcher: Arc<EventDispatcher>,
    command: &[String],
    output_directory: &Path,
) -> Result<ScenarioInfo> {
    if command.is_empty() {
        anyhow::bail!("record roofline requires a command");
    }

    let mut selected = select_method(options, command)?;
    if crate::source::roofline_uncore_bandwidth() {
        selected.info.traffic = "dram-measured".to_string();
        selected.info.warnings.push(
            "DRAM traffic is measured on system-wide memory-controller counters and therefore includes traffic from other processes"
                .to_string(),
        );
    }
    println!("Roofline method: {}", selected.info.reason);
    for warning in &selected.info.warnings {
        eprintln!("Warning: {warning}");
    }
    let mut resolved_command = command.to_vec();
    resolved_command[0] = selected.executable.to_string_lossy().into_owned();
    let fallback = qemu_fallback_for(options, &selected, &resolved_command);

    let backend: Box<dyn RooflineBackend> = match selected.backend {
        BackendKind::Auto => unreachable!("automatic Roofline method must resolve to a backend"),
        BackendKind::Compiler => Box::new(CompilerBackend {
            method: selected.info,
        }),
        BackendKind::Qemu => Box::new(qemu::QemuBackend::new(
            options,
            selected.performance == PerformanceSource::Native,
            selected.info,
            false,
        )?),
        BackendKind::DynamoRio => Box::new(qemu::QemuBackend::new_dynamorio(
            options,
            selected.info,
            false,
        )?),
    };
    let result = backend
        .record(dispatcher.clone(), &resolved_command, output_directory)
        .await;
    match (result, fallback) {
        (Err(error), Some(fallback)) => {
            eprintln!(
                "Warning: DynamoRIO Roofline accounting failed ({error:#}); retrying with the QEMU backend"
            );
            let backend = qemu::QemuBackend::new(options, true, fallback.info, false)?;
            backend
                .record(dispatcher, &resolved_command, output_directory)
                .await
        }
        (result, _) => result,
    }
}

/// When DynamoRIO was chosen automatically, a working QEMU setup serves as
/// the runtime fallback: the DynamoRIO port is experimental on some
/// architectures and a failed accounting run should degrade to the slower
/// path, not abort. Explicit --roofline-backend requests never fall back.
fn qemu_fallback_for(
    options: &Options,
    selected: &SelectedMethod,
    resolved_command: &[String],
) -> Option<SelectedMethod> {
    if selected.backend != BackendKind::DynamoRio || selected.info.selection != "auto" {
        return None;
    }
    qemu::probe(options, Path::new(&resolved_command[0])).ok()?;
    Some(qemu_method(true, true, PathBuf::from(&resolved_command[0])))
}

pub async fn record_memory(
    options: &Options,
    dispatcher: Arc<EventDispatcher>,
    command: &[String],
    output_directory: &Path,
) -> Result<ScenarioInfo> {
    if command.is_empty() {
        anyhow::bail!("record mem requires a command");
    }
    let selected = select_method(options, command)?;
    if !matches!(selected.backend, BackendKind::Qemu | BackendKind::DynamoRio) {
        anyhow::bail!(
            "the mem scenario requires address accounting; install DynamoRIO with the miniperf client, or a plugin-enabled QEMU and the miniperf QEMU plugin"
        );
    }
    if selected.performance != PerformanceSource::Native {
        anyhow::bail!("the mem scenario requires a native executable for trustworthy timing");
    }
    println!("Memory method: {}", selected.info.reason);
    let mut resolved_command = command.to_vec();
    resolved_command[0] = selected.executable.to_string_lossy().into_owned();
    let fallback = qemu_fallback_for(options, &selected, &resolved_command);
    let mut dynamorio = selected.backend == BackendKind::DynamoRio;
    let backend = if dynamorio {
        qemu::QemuBackend::new_dynamorio(options, selected.info, true)?
    } else {
        qemu::QemuBackend::new(options, true, selected.info, true)?
    };
    let mut roofline = backend
        .record(dispatcher.clone(), &resolved_command, output_directory)
        .await;
    if let (Err(error), Some(fallback)) = (&roofline, fallback) {
        eprintln!(
            "Warning: DynamoRIO memory accounting failed ({error:#}); retrying with the QEMU backend"
        );
        let backend = qemu::QemuBackend::new(options, true, fallback.info, true)?;
        roofline = backend
            .record(dispatcher, &resolved_command, output_directory)
            .await;
        dynamorio = false;
    }
    let roofline = roofline?;
    let ScenarioInfo::Roofline(info) = roofline else {
        unreachable!("QEMU backend returned a non-Roofline result")
    };
    let method = info.method.as_deref();
    let hardware_bandwidth = output_directory
        .join("memory-bandwidth.txt")
        .metadata()
        .is_ok_and(|metadata| metadata.len() != 0);
    Ok(ScenarioInfo::Mem(mperf_data::MemoryInfo {
        perf_pid: info.perf_pid,
        accounting_pid: info.inst_pid,
        counters: info.counters,
        method: mperf_data::MemoryMethodInfo {
            selection: method
                .map_or("explicit", |value| value.selection.as_str())
                .to_string(),
            accounting: if dynamorio {
                "dynamorio-addresses"
            } else {
                "qemu-addresses"
            }
            .to_string(),
            performance: "native".to_string(),
            bandwidth: if hardware_bandwidth {
                "hardware_memory_controller"
            } else {
                "process_modeled"
            }
            .to_string(),
            quality: method
                .map_or("hybrid", |value| value.quality.as_str())
                .to_string(),
            warnings: method.map_or_else(Vec::new, |value| value.warnings.clone()),
        },
    }))
}

fn select_method(options: &Options, command: &[String]) -> Result<SelectedMethod> {
    let guest = inspect_guest(Path::new(&command[0]))?;
    let native = guest.architecture == host_architecture();
    let qemu_probe = qemu::probe(options, &guest.path);
    let dynamorio_probe = qemu::probe_dynamorio(options);

    match options.backend {
        BackendKind::Qemu => {
            qemu_probe?;
            Ok(qemu_method(false, native, guest.path))
        }
        BackendKind::DynamoRio => {
            dynamorio_probe?;
            if !native {
                anyhow::bail!(
                    "DynamoRIO instruments natively and cannot run cross-architecture executable '{}'; use --roofline-backend qemu instead",
                    guest.path.display()
                );
            }
            Ok(dynamorio_method(false, guest.path))
        }
        BackendKind::Compiler => {
            if !guest.compiler_instrumented {
                anyhow::bail!(
                    "compiler Roofline was requested, but '{}' does not contain complete miniperf loop instrumentation",
                    guest.path.display()
                );
            }
            Ok(compiler_method(false, guest.path))
        }
        BackendKind::Auto => {
            if dynamorio_probe.is_ok() && native {
                return Ok(dynamorio_method(true, guest.path));
            }
            if qemu_probe.is_ok() && native {
                return Ok(qemu_method(true, native, guest.path));
            }
            if qemu_probe.is_ok() {
                anyhow::bail!(
                    "accurate Roofline performance is unavailable for cross-architecture executable '{}': QEMU can provide operation accounting but not native RISC-V timing; run the same mperf command on a compatible RISC-V host (explicit --roofline-backend qemu remains available for emulation diagnostics)",
                    guest.path.display()
                );
            }
            if guest.compiler_instrumented {
                return Ok(compiler_method(true, guest.path));
            }

            let qemu_error = qemu_probe.unwrap_err();
            let dynamorio_error = dynamorio_probe.unwrap_err();
            anyhow::bail!(
                "no accurate Roofline method is available for '{}': DynamoRIO accounting is unavailable ({dynamorio_error:#}), QEMU accounting is unavailable ({qemu_error:#}), and the executable has no miniperf loop instrumentation",
                guest.path.display()
            )
        }
    }
}

fn dynamorio_method(automatic: bool, executable: PathBuf) -> SelectedMethod {
    SelectedMethod {
        backend: BackendKind::DynamoRio,
        performance: PerformanceSource::Native,
        executable,
        info: RooflineMethodInfo {
            selection: if automatic { "auto" } else { "explicit" }.to_string(),
            accounting: "dynamorio".to_string(),
            performance: "native".to_string(),
            traffic: "architectural".to_string(),
            quality: "hybrid-binary".to_string(),
            reason: "native timing with DynamoRIO binary accounting".to_string(),
            warnings: vec![
                "per-loop throughput is published only when native timing has at most 10% estimated 95% sampling error; lower-confidence loops retain accounting but are not plotted".to_string(),
                "native timing and DynamoRIO accounting come from separate executions".to_string(),
            ],
        },
    }
}

fn qemu_method(automatic: bool, native: bool, executable: PathBuf) -> SelectedMethod {
    let performance = if native {
        PerformanceSource::Native
    } else {
        PerformanceSource::Qemu
    };
    let (quality, reason, warnings) = if native {
        (
            "hybrid-binary-sampled-cache-model".to_string(),
            "native timing with QEMU operation accounting, shared-LLC traffic modeling, and dynamic binary loop discovery"
                .to_string(),
            vec![
                "per-loop throughput is published only when native timing has at most 10% estimated 95% sampling error; lower-confidence loops retain accounting but are not plotted".to_string(),
                "native timing and QEMU accounting come from separate executions".to_string(),
                "memory traffic is a deterministic host-LLC model, not a hardware memory-controller measurement".to_string(),
                "the cache model uses write allocation/RFO and dirty writeback; non-temporal stores are conservative".to_string(),
            ],
        )
    } else {
        (
            "emulation-analysis".to_string(),
            "QEMU timing, operation accounting, and shared-LLC traffic modeling because the guest cannot execute natively"
                .to_string(),
            vec![
                "throughput is based on emulator time and must not be interpreted as guest-hardware performance".to_string(),
                "the Roofline UI point is currently whole-process; per-loop candidates and accounting are saved in qemu-roofline.loops.json".to_string(),
                "memory traffic is a deterministic host-LLC model, not a hardware memory-controller measurement".to_string(),
            ],
        )
    };
    SelectedMethod {
        backend: BackendKind::Qemu,
        performance,
        executable,
        info: RooflineMethodInfo {
            selection: if automatic { "auto" } else { "explicit" }.to_string(),
            accounting: "qemu".to_string(),
            performance: if native { "native" } else { "qemu" }.to_string(),
            traffic: "dram-model".to_string(),
            quality,
            reason,
            warnings,
        },
    }
}

fn compiler_method(automatic: bool, executable: PathBuf) -> SelectedMethod {
    SelectedMethod {
        backend: BackendKind::Compiler,
        performance: PerformanceSource::Native,
        executable,
        info: RooflineMethodInfo {
            selection: if automatic { "auto" } else { "explicit" }.to_string(),
            accounting: "compiler".to_string(),
            performance: "native".to_string(),
            traffic: "architectural".to_string(),
            quality: "compiler-instrumented".to_string(),
            reason: "native timing and detected miniperf compiler instrumentation".to_string(),
            warnings: vec![
                "the legacy compiler backend only reports loops transformed by the miniperf LLVM pass"
                    .to_string(),
            ],
        },
    }
}

struct GuestInfo {
    path: PathBuf,
    architecture: object::Architecture,
    compiler_instrumented: bool,
}

fn inspect_guest(path: &Path) -> Result<GuestInfo> {
    let path = if path.components().count() == 1 {
        which::which(path)
            .with_context(|| format!("could not find executable '{}'", path.display()))?
    } else {
        path.to_owned()
    };
    let data = std::fs::read(&path)
        .with_context(|| format!("read Roofline executable '{}'", path.display()))?;
    let object = object::File::parse(data.as_slice())
        .with_context(|| format!("parse Roofline executable '{}'", path.display()))?;
    let instrumentation_symbols = object
        .symbols()
        .chain(object.dynamic_symbols())
        .filter_map(|symbol| symbol.name().ok())
        .filter(|name| {
            matches!(
                *name,
                "mperf_roofline_internal_notify_loop_begin"
                    | "mperf_roofline_internal_notify_loop_end"
                    | "mperf_roofline_internal_notify_loop_stats"
            )
        })
        .collect::<std::collections::HashSet<_>>();
    let compiler_instrumented = instrumentation_symbols.len() == 3;
    Ok(GuestInfo {
        path,
        architecture: object.architecture(),
        compiler_instrumented,
    })
}

fn host_architecture() -> object::Architecture {
    if cfg!(target_arch = "x86_64") {
        object::Architecture::X86_64
    } else if cfg!(target_arch = "aarch64") {
        object::Architecture::Aarch64
    } else if cfg!(target_arch = "riscv64") {
        object::Architecture::Riscv64
    } else if cfg!(target_arch = "riscv32") {
        object::Architecture::Riscv32
    } else {
        object::Architecture::Unknown
    }
}

pub(crate) fn calibrate_host() -> Result<mperf_data::RooflineCalibration> {
    calibrate::measure()
}

pub(crate) fn calibrate_memory_host() -> Result<mperf_data::MemoryBandwidthCalibration> {
    calibrate::measure_memory_hierarchy()
}

pub(crate) fn uses_native_performance(options: &Options, command: &[String]) -> Result<bool> {
    if command.is_empty() {
        anyhow::bail!("record roofline requires a command");
    }
    Ok(select_method(options, command)?.performance == PerformanceSource::Native)
}

impl RooflineBackend for CompilerBackend {
    fn record<'a>(
        &'a self,
        dispatcher: Arc<EventDispatcher>,
        command: &'a [String],
        output_directory: &'a Path,
    ) -> BackendFuture<'a> {
        Box::pin(async move {
            let exe_path = executable_directory()?.to_string_lossy().to_string();
            let ld_path = match std::env::var("LD_LIBRARY_PATH") {
                Ok(path) => format!("{path}:{exe_path}:{exe_path}/../lib"),
                Err(_) => format!("{exe_path}:{exe_path}/../lib"),
            };
            let collector_env = libprof::Source::child_environment(
                &libprof::InternalEventsSource {
                    roofline_instrumented: false,
                },
                output_directory,
            );

            println!(
                "Run 1: collecting performance data for '{}'",
                command.join(" ")
            );
            let mut baseline_env = collector_env.clone();
            baseline_env.push(("LD_LIBRARY_PATH".to_string(), ld_path.clone()));
            let baseline = profile_command(
                dispatcher.clone(),
                command,
                baseline_env,
                None,
                bandwidth_timeline_path(output_directory),
            )
            .await?;

            println!(
                "Run 2: collecting compiler-instrumented loop statistics for '{}'",
                command.join(" ")
            );
            let mut instrumented_env = libprof::Source::child_environment(
                &libprof::InternalEventsSource {
                    roofline_instrumented: true,
                },
                output_directory,
            );
            instrumented_env.push(("LD_LIBRARY_PATH".to_string(), ld_path));
            let process = Process::new(command, &instrumented_env)?;
            process.cont();
            process.wait()?;
            ensure_process_success(&process, "compiler Roofline accounting run")?;

            let mut method = self.method.clone();
            method.warnings.extend(baseline.warnings);

            Ok(ScenarioInfo::Roofline(RooflineInfo {
                backend: "compiler".to_string(),
                perf_pid: baseline.pid,
                counters: baseline.counters,
                inst_pid: process.pid(),
                method: Some(Box::new(method)),
            }))
        })
    }
}

struct ProfiledRun {
    warnings: Vec<String>,
    pid: i32,
    start_ns: u64,
    end_ns: u64,
    counters: Vec<(mperf_data::EventType, String)>,
    instructions: u64,
}

async fn profile_command(
    dispatcher: Arc<EventDispatcher>,
    command: &[String],
    mut env: Vec<(String, String)>,
    rss_output: Option<PathBuf>,
    bandwidth_output: Option<PathBuf>,
) -> Result<ProfiledRun> {
    if let Some(rss_path) = rss_output.as_ref() {
        let allocation_path = rss_path.with_file_name("memory-allocations.txt");
        env.push((
            "MPERF_MEMORY_ALLOCATIONS".to_string(),
            allocation_path.to_string_lossy().into_owned(),
        ));
        if let Some(preload) = memory_preload_path() {
            let preload = std::env::var("LD_PRELOAD")
                .map(|current| format!("{}:{current}", preload.display()))
                .unwrap_or_else(|_| preload.to_string_lossy().into_owned());
            env.push(("LD_PRELOAD".to_string(), preload));
        }
    }
    let precise_memory_source = rss_output
        .as_ref()
        .and_then(|path| path.parent())
        .and_then(precise_memory_source);
    if let Some((source, directory)) = &precise_memory_source {
        env.extend(source.child_environment(directory));
    }
    let process = Process::new(command, &env)?;
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let bandwidth_thread =
        bandwidth_output.and_then(|path| start_bandwidth_timeline(path, sampler_stop.clone()));
    let rss_thread = rss_output.map(|path| {
        let stop = sampler_stop.clone();
        let pid = process.pid();
        std::thread::spawn(move || sample_memory_timeline(pid, &path, stop))
    });
    let counters = crate::source::scenario_counters(Scenario::Roofline);
    // Opened before the sampling groups so they can size themselves around the
    // hardware counter it takes.
    let mut instruction_counter = libprof::CountingDriverBuilder::new()
        .counters(&[Counter::Instructions])
        .process(Some(&process))
        .build()
        .ok();
    let mut driver = libprof::SamplingDriverBuilder::new()
        .counters(&counters)
        .process(&process)
        .build()?;
    let mut warnings = Vec::new();
    let sampled = driver.counters();
    let shed = counters
        .iter()
        .filter(|counter| !sampled.contains(counter))
        .map(|counter| counter.name())
        .collect::<Vec<_>>();
    if !shed.is_empty() {
        let warning = format!(
            "this host's PMU cannot run the full sampling group, so these counters were not sampled: {}",
            shed.join(", ")
        );
        eprintln!("Warning: {warning}");
        warnings.push(warning);
    }
    let mut precise_memory = precise_memory_source.map(|(source, directory)| {
        start_precise_memory(source, dispatcher.clone(), &directory, &process)
    });
    driver.start(dispatcher)?;
    if let Some(counter) = instruction_counter.as_mut()
        && counter.start().is_err()
    {
        instruction_counter = None;
    }

    let start_ns = monotonic_timestamp()?;
    process.cont();
    process.wait()?;
    let instructions = if let Some(counter) = instruction_counter.as_mut() {
        counter.stop()?;
        counter
            .counters()?
            .get(Counter::Instructions)
            .map_or(0, |value| value.value)
    } else {
        0
    };
    sampler_stop.store(true, Ordering::Relaxed);
    if let Some(thread) = rss_thread {
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("RSS sampler panicked"))??;
    }
    if let Some(thread) = bandwidth_thread {
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("bandwidth sampler panicked"))??;
    }
    let end_ns = monotonic_timestamp()?;
    // A sampler that lost records still measured the run, and a second pass
    // depends on this one; a sampler that never scheduled measured nothing, so
    // there is no timing for the second pass to pair with.
    if let Err(error) = driver.stop() {
        if matches!(error, libprof::Error::SamplingGroupNeverScheduled { .. }) {
            anyhow::bail!("{error}");
        }
        eprintln!("Warning: pmu_sampling: {error}");
        warnings.push(format!("pmu_sampling: {error}"));
    }
    if libprof::inherited_sampling_supported() == Some(false) {
        let warning = "this kernel rejects inherited sampling groups (Linux < 6.12), so threads created after exec were not sampled and loop timing covers the main thread only".to_string();
        eprintln!("Warning: {warning}");
        warnings.push(warning);
    }

    if let Some((source, context)) = precise_memory.as_mut() {
        for status in libprof::Source::stop(source.as_mut(), context)
            .iter()
            .filter(|status| !status.is_available())
        {
            eprintln!("Warning: {}: {}", status.name, status.message);
        }
    }
    ensure_process_success(&process, "Roofline performance run")?;

    Ok(ProfiledRun {
        warnings,
        pid: process.pid(),
        start_ns,
        end_ns,
        counters: counters
            .iter()
            .map(|counter| (counter_to_event_ty(counter), counter.name().to_string()))
            .collect(),
        instructions,
    })
}

/// Precise memory sampling for the timed run, where the host provides it.
/// Otherwise nothing is created and the recording — including the profiled
/// child's environment — is exactly what it was before.
fn precise_memory_source(directory: &Path) -> Option<(Box<dyn libprof::Source>, PathBuf)> {
    let source: Box<dyn libprof::Source> = Box::new(libprof::PreciseMemorySource::default());
    match source.probe(directory) {
        libprof::Availability::Available => Some((source, directory.to_owned())),
        libprof::Availability::Unavailable { reason } => {
            println!("Precise memory sampling unavailable: {reason}");
            None
        }
    }
}

fn start_precise_memory(
    mut source: Box<dyn libprof::Source>,
    dispatcher: Arc<EventDispatcher>,
    directory: &Path,
    process: &Process,
) -> (Box<dyn libprof::Source>, libprof::SessionContext) {
    let context = libprof::SessionContext {
        directory: directory.to_owned(),
        sink: dispatcher,
        process: None,
        attached_pid: Some(process.pid() as u32),
    };
    if let Err(error) = source.start(&context) {
        eprintln!("Warning: precise memory sampling could not start: {error:#}");
    }
    (source, context)
}

fn sample_memory_timeline(pid: i32, output: &Path, stop: Arc<AtomicBool>) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(output)
        .with_context(|| format!("create RSS timeline '{}'", output.display()))?;
    while !stop.load(Ordering::Relaxed) {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status"))
            && let Some(kbytes) = status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        {
            writeln!(
                file,
                "{} {}",
                monotonic_timestamp()?,
                kbytes.saturating_mul(1024)
            )?;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

/// The measured-DRAM timeline for the timed run: cumulative system-wide
/// memory-controller bytes, sampled often enough that postprocess can
/// integrate it over each kernel's execution window.
fn start_bandwidth_timeline(
    output: PathBuf,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<Result<()>>> {
    let monitor = match libprof::MemoryControllerMonitor::start() {
        Ok(Some(monitor)) => monitor,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("Warning: measured DRAM bandwidth is unavailable: {error}");
            return None;
        }
    };
    Some(std::thread::spawn(move || {
        use std::io::Write;
        let mut file = std::fs::File::create(&output)
            .with_context(|| format!("create bandwidth timeline '{}'", output.display()))?;
        loop {
            let finished = stop.load(Ordering::Relaxed);
            match monitor.sample() {
                Ok(sample) => writeln!(
                    file,
                    "{} {} {}",
                    monotonic_timestamp()?,
                    sample.read_bytes,
                    sample.write_bytes
                )?,
                Err(error) => {
                    eprintln!("Warning: memory-controller sampling stopped: {error}");
                    return Ok(());
                }
            }
            if finished {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }))
}

/// Where to write measured DRAM bytes, when the Roofline ladder resolves to
/// the uncore rung on this host.
fn bandwidth_timeline_path(directory: &Path) -> Option<PathBuf> {
    crate::source::roofline_uncore_bandwidth().then(|| directory.join("memory-bandwidth.txt"))
}

fn ensure_process_success(process: &Process, description: &str) -> Result<()> {
    match process.exit_code() {
        Some(0) => Ok(()),
        Some(code) => anyhow::bail!("{description} exited with status {code}"),
        None => anyhow::bail!("{description} finished without an observable exit status"),
    }
}

fn executable_directory() -> std::io::Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.pop();
    Ok(path)
}

/// Resolves `relative` inside the miniperf package that owns this executable.
///
/// Release packages keep the collector libraries and the embedded DynamoRIO
/// and qemu-user bundles under `lib/miniperf`; a development build leaves them
/// next to the binary.
pub(super) fn package_path(relative: &str) -> Option<PathBuf> {
    let executable_directory = executable_directory().ok()?;
    [
        executable_directory.join(relative),
        executable_directory.join("../lib/miniperf").join(relative),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// The `LD_PRELOAD` shim that reports allocations. It is an ELF shared object
/// loaded through a Linux-only mechanism; elsewhere there is nothing to
/// preload.
fn memory_preload_path() -> Option<PathBuf> {
    cfg!(target_os = "linux")
        .then(|| package_path("libmperf_libc.so"))
        .flatten()
}

fn monotonic_timestamp() -> Result<u64> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read monotonic clock");
    }
    Ok((timestamp.tv_sec as u64) * 1_000_000_000 + timestamp.tv_nsec as u64)
}
