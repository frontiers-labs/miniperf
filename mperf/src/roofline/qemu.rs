use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use kdam::BarExt;
use libprof::Process;
use mperf_data::{
    CallFrame, Event, EventType, Location, RooflineInfo, RooflineMethodInfo, ScenarioInfo,
};
use object::{Object, ObjectKind, ObjectSegment, ObjectSymbol, SegmentFlags};
use serde::Serialize;
use smallvec::smallvec;

use super::{
    BackendFuture, Options, ProfiledRun, RooflineBackend, loops::ControlFlowGraph,
    monotonic_timestamp, profile_command,
};
use crate::event_dispatcher::EventDispatcher;

pub(super) struct QemuBackend {
    tool: AccountingTool,
    native_performance: bool,
    method: RooflineMethodInfo,
    cache: CacheInfo,
    memory_profile: bool,
}

/// Which same-artifact accounting tool drives run 2. Both emit the
/// `qemu-roofline.*` artifact set; DynamoRIO instruments natively and only
/// works when the guest matches the host architecture.
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "QEMU and DynamoRIO accounting tools are constructed only on Linux"
    )
)]
pub(super) enum AccountingTool {
    Qemu {
        qemu: Option<PathBuf>,
        plugin: PathBuf,
        extra_args: Vec<String>,
    },
    DynamoRio {
        drrun: PathBuf,
        client: PathBuf,
    },
}

/// Run-2 command line plus the extra environment it needs.
type AccountingCommand = (Vec<String>, Vec<(String, String)>);

impl AccountingTool {
    fn name(&self) -> &'static str {
        match self {
            AccountingTool::Qemu { .. } => "QEMU",
            AccountingTool::DynamoRio { .. } => "DynamoRIO",
        }
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
struct Counts {
    instructions: u64,
    scalar_int_ops: u64,
    scalar_float_ops: u64,
    scalar_double_ops: u64,
    vector_int_ops: u64,
    vector_float_ops: u64,
    vector_double_ops: u64,
    bytes_load: u64,
    bytes_store: u64,
    dram_bytes_load: u64,
    dram_bytes_store: u64,
    rvv_state_errors: u64,
    unclassified_instructions: u64,
    child_process_seen: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct BlockCounts {
    #[serde(skip_serializing)]
    end: u64,
    executions: u64,
    scalar_int_ops: u64,
    scalar_float_ops: u64,
    scalar_double_ops: u64,
    vector_int_ops: u64,
    vector_float_ops: u64,
    vector_double_ops: u64,
    bytes_load: u64,
    bytes_store: u64,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
    unclassified_instructions: u64,
    /// Executions per accounting-engine thread; empty when the engine counted
    /// the block without a thread identity.
    #[serde(skip_serializing)]
    threads: Vec<(u32, u64)>,
}

/// One thread's share of a loop's work in the accounting run. Operation and
/// byte counts are apportioned by that thread's share of each block's
/// executions.
#[derive(Clone, Debug, Default, Serialize)]
struct LoopThreadArtifact {
    thread: u32,
    executions: u64,
    scalar_int_ops: u64,
    scalar_float_ops: u64,
    scalar_double_ops: u64,
    vector_int_ops: u64,
    vector_float_ops: u64,
    vector_double_ops: u64,
    arch_bytes_load: u64,
    arch_bytes_store: u64,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct DynamicCfgCapture {
    image: Option<ImageInfo>,
    cache: Option<CacheInfo>,
    entries: BTreeSet<u64>,
    edges: BTreeMap<(u64, u64), u64>,
    blocks: BTreeMap<u64, BlockCounts>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheInfo {
    line_size: u64,
    capacity: u64,
    associativity: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageInfo {
    start: u64,
    end: u64,
    entry: u64,
}

#[derive(Serialize)]
struct LoopArtifact {
    header: String,
    module_offset: String,
    function: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    latches: Vec<String>,
    blocks: Vec<String>,
    block_ranges: Vec<BlockRangeArtifact>,
    parent_header: Option<String>,
    trip_count: u64,
    inclusive: BlockCounts,
    self_counts: BlockCounts,
    threads: Vec<LoopThreadArtifact>,
}

#[derive(Serialize)]
struct BlockRangeArtifact {
    runtime_start: String,
    runtime_end: String,
    module_start: String,
    module_end: String,
}

#[derive(Serialize)]
struct DynamicCfgArtifact {
    format_version: u32,
    model: &'static str,
    executable: String,
    warnings: Vec<&'static str>,
    cache_line_size: u64,
    llc_capacity: u64,
    llc_associativity: u64,
    traffic_model: &'static str,
    image_start: String,
    image_end: String,
    image_entry: String,
    load_bias: String,
    entries: Vec<String>,
    loops: Vec<LoopArtifact>,
    irreducible_cycles: Vec<Vec<String>>,
}

#[derive(Serialize)]
struct NativeTimingArtifact {
    pid: i32,
    start_ns: u64,
    end_ns: u64,
}

impl QemuBackend {
    #[cfg(not(target_os = "linux"))]
    pub(super) fn new(
        _options: &Options,
        _native_performance: bool,
        _method: RooflineMethodInfo,
        _memory_profile: bool,
    ) -> Result<Self> {
        anyhow::bail!("the QEMU roofline backend supports Linux hosts only");
    }

    #[cfg(target_os = "linux")]
    pub(super) fn new(
        options: &Options,
        native_performance: bool,
        method: RooflineMethodInfo,
        memory_profile: bool,
    ) -> Result<Self> {
        let plugin = plugin_path(options);
        if !plugin.is_file() {
            anyhow::bail!(
                "QEMU roofline plugin '{}' was not found; build it with `cargo build -p miniperf-qemu-roofline` or pass --qemu-plugin",
                plugin.display()
            );
        }

        Ok(Self {
            tool: AccountingTool::Qemu {
                qemu: options.qemu.clone(),
                plugin,
                extra_args: options.qemu_args.clone(),
            },
            native_performance,
            method,
            cache: detect_host_cache(),
            memory_profile,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn new_dynamorio(
        _options: &Options,
        _method: RooflineMethodInfo,
        _memory_profile: bool,
    ) -> Result<Self> {
        anyhow::bail!("the DynamoRIO roofline backend supports Linux hosts only");
    }

    #[cfg(target_os = "linux")]
    pub(super) fn new_dynamorio(
        options: &Options,
        mut method: RooflineMethodInfo,
        memory_profile: bool,
    ) -> Result<Self> {
        let (drrun, client) = dynamorio_paths(options)?;
        if memory_profile {
            method.traffic = "dram-model".to_string();
            method.quality = "hybrid-binary-exact-addresses".to_string();
            method.reason =
                "native timing with DynamoRIO exact-address and shared-LLC analysis".to_string();
            method.warnings.push(
                "memory traffic is a deterministic host-LLC model, not a hardware memory-controller measurement"
                    .to_string(),
            );
            method.warnings.push(
                "the cache model uses write allocation/RFO and dirty writeback; non-temporal stores are conservative"
                    .to_string(),
            );
        } else {
            method.traffic = "architectural".to_string();
            method.quality = "hybrid-binary-aggregate-blocks".to_string();
            method.reason = "native timing with low-overhead DynamoRIO operation and architectural-traffic accounting"
                .to_string();
            method.warnings.retain(|warning| {
                !warning.contains("host-LLC model") && !warning.contains("write allocation/RFO")
            });
            method.warnings.push(
                "the low-overhead Roofline pass does not collect dynamic addresses or modeled DRAM traffic; use the mem scenario for exact address and shared-LLC analysis"
                    .to_string(),
            );
            method.warnings.push(
                "basic-block counts are exact; CFG successor topology is static and conditional edge weights are approximate"
                    .to_string(),
            );
        }
        Ok(Self {
            tool: AccountingTool::DynamoRio { drrun, client },
            // DynamoRIO instruments the native binary; performance always
            // comes from a separate native run.
            native_performance: true,
            method,
            cache: detect_host_cache(),
            memory_profile,
        })
    }

    fn qemu_for(&self, guest: &Path) -> Result<PathBuf> {
        let AccountingTool::Qemu { qemu, .. } = &self.tool else {
            anyhow::bail!("qemu_for is only valid for the QEMU accounting tool");
        };
        if let Some(qemu) = qemu {
            return resolve_executable(qemu);
        }
        if let Some(qemu) = std::env::var_os("MPERF_QEMU") {
            return resolve_executable(&PathBuf::from(qemu));
        }

        let guest = resolve_executable(guest)?;
        let data = std::fs::read(&guest)
            .with_context(|| format!("read guest executable '{}'", guest.display()))?;
        let object = object::File::parse(data.as_slice())
            .with_context(|| format!("parse guest executable '{}'", guest.display()))?;
        if !matches!(object.kind(), ObjectKind::Executable | ObjectKind::Dynamic) {
            anyhow::bail!("QEMU roofline guest must be an executable ELF");
        }
        let binary = qemu_binary_for_architecture(object.architecture())
            .context("unsupported guest architecture for QEMU roofline")?;
        if let Some(bundled) = super::package_path(&format!("qemu/bin/{binary}")) {
            return Ok(bundled);
        }
        which::which(binary).with_context(|| {
            format!("could not find '{binary}' in this miniperf package or on PATH; pass --qemu explicitly")
        })
    }

    fn command(&self, qemu: &Path, guest: &[String], output: Option<&Path>) -> Result<Vec<String>> {
        let AccountingTool::Qemu {
            plugin, extra_args, ..
        } = &self.tool
        else {
            anyhow::bail!("command is only valid for the QEMU accounting tool");
        };
        let mut command = vec![qemu.to_string_lossy().to_string()];
        command.extend(extra_args.iter().cloned());
        if let Some(output) = output {
            let output = output.to_string_lossy();
            if output.contains(',') {
                anyhow::bail!("QEMU plugin output path cannot contain a comma");
            }
            command.push("-plugin".to_string());
            command.push(format!(
                "{},output={output},cache-line={},llc-size={},llc-assoc={},memory-profile={}",
                plugin.to_string_lossy(),
                self.cache.line_size,
                self.cache.capacity,
                self.cache.associativity,
                if self.memory_profile { "on" } else { "off" }
            ));
            if self.memory_profile
                && let Some(preload) = super::memory_preload_path()
            {
                command.push("-E".to_string());
                command.push(format!("LD_PRELOAD={}", preload.display()));
                command.push("-E".to_string());
                command.push("MPERF_MEMORY_ALLOCATIONS=/dev/null".to_string());
            }
        }
        command.extend(guest.iter().cloned());
        Ok(command)
    }

    /// Builds the accounting (run 2) command and its extra environment.
    fn accounting_command(
        &self,
        guest: &[String],
        output: &Path,
        progress: Option<&Path>,
    ) -> Result<AccountingCommand> {
        match &self.tool {
            AccountingTool::Qemu { .. } => {
                let qemu = self.qemu_for(Path::new(&guest[0]))?;
                Ok((self.command(&qemu, guest, Some(output))?, Vec::new()))
            }
            AccountingTool::DynamoRio { drrun, client } => {
                let mut command = vec![
                    drrun.to_string_lossy().to_string(),
                    // The embedded bundle is 64-bit release only, so DR's
                    // frontend warns about the lib32 and debug builds it
                    // cannot find and then runs anyway.
                    "-quiet".to_string(),
                    // Clean-call-heavy instrumentation exceeds DynamoRIO's
                    // block emit limits at the default 256-instruction blocks
                    // (observed on riscv64); traces add no value here.
                    "-disable_traces".to_string(),
                    "-max_bb_instrs".to_string(),
                    "32".to_string(),
                    "-c".to_string(),
                    client.to_string_lossy().to_string(),
                    format!("output={}", output.to_string_lossy()),
                    format!("cache-line={}", self.cache.line_size),
                    format!("llc-size={}", self.cache.capacity),
                    format!("llc-assoc={}", self.cache.associativity),
                    format!(
                        "memory-profile={}",
                        if self.memory_profile { "on" } else { "off" }
                    ),
                    "--".to_string(),
                ];
                if let Some(progress) = progress {
                    command.insert(
                        command.len() - 1,
                        format!("progress={}", progress.to_string_lossy()),
                    );
                }
                command.extend(guest.iter().cloned());
                let mut env = Vec::new();
                if self.memory_profile
                    && let Some(preload) = super::memory_preload_path()
                {
                    env.push((
                        "LD_PRELOAD".to_string(),
                        preload.to_string_lossy().into_owned(),
                    ));
                    env.push((
                        "MPERF_MEMORY_ALLOCATIONS".to_string(),
                        "/dev/null".to_string(),
                    ));
                }
                Ok((command, env))
            }
        }
    }
}

struct DynamoRioProgress {
    stop: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}

impl DynamoRioProgress {
    fn start(path: PathBuf, estimated_instructions: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_completed = completed.clone();
        let thread_path = path.clone();
        let thread = thread::spawn(move || {
            let total = usize::try_from(estimated_instructions).unwrap_or(usize::MAX);
            let mut progress = kdam::tqdm!(
                total = total,
                desc = "DynamoRIO accounting",
                unit = "inst",
                unit_scale = true,
                leave = true
            );
            let mut instruction_stream = None;
            while !thread_stop.load(Ordering::Acquire) {
                if instruction_stream.is_none() {
                    instruction_stream = File::open(&thread_path).ok();
                }
                if let Some(instructions) = instruction_stream
                    .as_mut()
                    .and_then(read_instruction_progress)
                {
                    let position = usize::try_from(instructions)
                        .unwrap_or(usize::MAX)
                        .min(total);
                    let _ = progress.update_to(position);
                }
                thread::sleep(Duration::from_millis(100));
            }
            if let Some(instructions) = instruction_stream
                .as_mut()
                .and_then(read_instruction_progress)
            {
                let position = usize::try_from(instructions)
                    .unwrap_or(usize::MAX)
                    .min(total);
                let _ = progress.update_to(position);
            }
            if thread_completed.load(Ordering::Relaxed) {
                // Replay totals can differ slightly from Run 1. Process exit,
                // rather than an exact counter match, is authoritative.
                let _ = progress.update_to(total);
            }
        });
        Self {
            stop,
            completed,
            thread: Some(thread),
            path,
        }
    }

    fn finish(&mut self, completed: bool) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.completed.store(completed, Ordering::Relaxed);
        self.stop.store(true, Ordering::Release);
        let _ = thread.join();
        let _ = std::fs::remove_file(&self.path);
        eprintln!();
    }
}

impl Drop for DynamoRioProgress {
    fn drop(&mut self) {
        self.finish(false);
    }
}

fn read_instruction_progress(stream: &mut File) -> Option<u64> {
    let mut update = String::new();
    stream
        .read_to_string(&mut update)
        .ok()
        .filter(|bytes| *bytes != 0)?;
    update
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .next_back()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn probe(_options: &Options, _guest: &Path) -> Result<()> {
    anyhow::bail!("the QEMU roofline backend supports Linux hosts only")
}

#[cfg(target_os = "linux")]
pub(super) fn probe(options: &Options, guest: &Path) -> Result<()> {
    let plugin = plugin_path(options);
    if !plugin.is_file() {
        anyhow::bail!("QEMU roofline plugin '{}' was not found", plugin.display());
    }
    let backend = QemuBackend {
        tool: AccountingTool::Qemu {
            qemu: options.qemu.clone(),
            plugin,
            extra_args: options.qemu_args.clone(),
        },
        native_performance: false,
        method: RooflineMethodInfo {
            selection: String::new(),
            accounting: String::new(),
            performance: String::new(),
            traffic: String::new(),
            quality: String::new(),
            reason: String::new(),
            warnings: Vec::new(),
        },
        cache: detect_host_cache(),
        memory_profile: false,
    };
    let qemu = backend.qemu_for(guest)?;
    ensure_plugin_support(&qemu)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn probe_dynamorio(_options: &Options) -> Result<()> {
    anyhow::bail!("the DynamoRIO roofline backend supports Linux hosts only")
}

/// Checks that drrun and the miniperf DynamoRIO client are available.
#[cfg(target_os = "linux")]
pub(super) fn probe_dynamorio(options: &Options) -> Result<()> {
    dynamorio_paths(options).map(|_| ())
}

#[cfg(target_os = "linux")]
fn dynamorio_paths(options: &Options) -> Result<(PathBuf, PathBuf)> {
    let configured_drrun = options
        .dynamorio
        .clone()
        .or_else(|| std::env::var_os("MPERF_DYNAMORIO").map(PathBuf::from));
    let drrun = if let Some(configured) = configured_drrun {
        resolve_dynamorio_launcher(&configured)?
    } else {
        if let Some(bundled) = super::package_path("dynamorio/bin64/drrun") {
            bundled
        } else {
            which::which("drrun").map_err(|_| {
                anyhow::anyhow!(
                    "DynamoRIO 'drrun' was not found in this miniperf package or on PATH; pass --dynamorio with a drrun executable or DynamoRIO build directory"
                )
            })?
        }
    };

    let configured_client = options
        .dynamorio_client
        .clone()
        .or_else(|| std::env::var_os("MPERF_DR_CLIENT").map(PathBuf::from));
    let client = if let Some(configured) = configured_client {
        if !configured.is_file() {
            anyhow::bail!(
                "miniperf DynamoRIO client '{}' was not found",
                configured.display()
            );
        }
        configured
    } else {
        discover_dynamorio_client(&drrun)?.ok_or_else(|| {
            anyhow::anyhow!(
                "miniperf DynamoRIO client 'libdr_roofline.so' was not found next to mperf, in the DynamoRIO bundle, or in this miniperf source checkout; build utils/dr-roofline or pass --dynamorio-client"
            )
        })?
    };
    Ok((drrun, client))
}

#[cfg(target_os = "linux")]
fn resolve_dynamorio_launcher(configured: &Path) -> Result<PathBuf> {
    if configured.is_file() {
        return Ok(configured.to_path_buf());
    }
    if configured.is_dir() {
        for relative in ["bin64/drrun", "dynamorio/bin64/drrun"] {
            let candidate = configured.join(relative);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        anyhow::bail!(
            "DynamoRIO directory '{}' contains neither bin64/drrun nor dynamorio/bin64/drrun",
            configured.display()
        );
    }
    if configured.components().count() == 1
        && let Ok(found) = which::which(configured)
    {
        return Ok(found);
    }
    anyhow::bail!(
        "DynamoRIO launcher or build directory '{}' was not found",
        configured.display()
    )
}

#[cfg(target_os = "linux")]
fn discover_dynamorio_client(drrun: &Path) -> Result<Option<PathBuf>> {
    let executable_directory = super::executable_directory().ok();
    if let Some(client) = executable_directory
        .as_deref()
        .map(|directory| directory.join("libdr_roofline.so"))
        .filter(|client| client.is_file())
    {
        return Ok(Some(client));
    }

    // A release bundle stores drrun under dynamorio/bin64 and the client at
    // the bundle root. Also accept a client copied to a normal DR build root.
    let mut directory = drrun.parent();
    for _ in 0..3 {
        let Some(current) = directory else { break };
        let candidate = current.join("libdr_roofline.so");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        directory = current.parent();
    }

    let mut clients = Vec::new();
    if let Ok(current_directory) = std::env::current_dir() {
        clients.extend(discover_checkout_clients(&current_directory)?);
    }
    if let Some(executable_directory) = executable_directory {
        clients.extend(discover_checkout_clients(&executable_directory)?);
    }
    clients.sort();
    clients.dedup();
    match clients.as_slice() {
        [] => Ok(None),
        [client] => Ok(Some(client.clone())),
        _ => anyhow::bail!(
            "multiple built miniperf DynamoRIO clients were found ({}); select one with --dynamorio-client",
            clients
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(target_os = "linux")]
fn discover_checkout_clients(start: &Path) -> Result<Vec<PathBuf>> {
    let Some(root) = start.ancestors().find(|candidate| {
        candidate.join("Cargo.toml").is_file()
            && candidate.join("utils/dr-roofline/CMakeLists.txt").is_file()
    }) else {
        return Ok(Vec::new());
    };
    let client_source = root.join("utils/dr-roofline");
    let mut clients = Vec::new();
    for entry in std::fs::read_dir(&client_source)
        .with_context(|| format!("failed to inspect '{}'", client_source.display()))?
    {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        for candidate in [
            path.join("libdr_roofline.so"),
            path.join("Release/libdr_roofline.so"),
        ] {
            if candidate.is_file() {
                clients.push(candidate);
            }
        }
    }
    Ok(clients)
}

#[cfg(target_os = "linux")]
fn detect_host_cache() -> CacheInfo {
    let fallback = CacheInfo {
        line_size: 64,
        capacity: 8 * 1024 * 1024,
        associativity: 16,
    };
    let Ok(indices) = std::fs::read_dir("/sys/devices/system/cpu/cpu0/cache") else {
        return fallback;
    };
    let mut candidates = Vec::new();
    for index in indices.flatten() {
        let path = index.path();
        let read = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .ok()
                .map(|value| value.trim().to_owned())
        };
        let Some(cache_type) = read("type") else {
            continue;
        };
        if cache_type != "Unified" && cache_type != "Data" {
            continue;
        }
        let Some(level) = read("level").and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(capacity) = read("size").and_then(|value| parse_cache_size(&value)) else {
            continue;
        };
        let Some(line_size) = read("coherency_line_size").and_then(|value| value.parse().ok())
        else {
            continue;
        };
        let Some(associativity) =
            read("ways_of_associativity").and_then(|value| value.parse().ok())
        else {
            continue;
        };
        if line_size == 0 || associativity == 0 || capacity < line_size * associativity {
            continue;
        }
        candidates.push((
            level,
            CacheInfo {
                line_size,
                capacity,
                associativity,
            },
        ));
    }
    candidates
        .into_iter()
        .max_by_key(|(level, cache)| (*level, cache.capacity))
        .map(|(_, cache)| cache)
        .unwrap_or(fallback)
}

#[cfg(target_os = "linux")]
fn parse_cache_size(value: &str) -> Option<u64> {
    let (number, multiplier) = match value.as_bytes().last().copied()? {
        b'K' | b'k' => (&value[..value.len() - 1], 1024_u64),
        b'M' | b'm' => (&value[..value.len() - 1], 1024_u64 * 1024),
        b'G' | b'g' => (&value[..value.len() - 1], 1024_u64 * 1024 * 1024),
        _ => (value, 1),
    };
    number.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

#[cfg(target_os = "linux")]
fn plugin_path(options: &Options) -> PathBuf {
    options
        .qemu_plugin
        .clone()
        .or_else(|| std::env::var_os("MPERF_QEMU_PLUGIN").map(PathBuf::from))
        .unwrap_or_else(default_plugin_path)
}

impl RooflineBackend for QemuBackend {
    fn record<'a>(
        &'a self,
        dispatcher: Arc<EventDispatcher>,
        guest: &'a [String],
        output_directory: &'a Path,
    ) -> BackendFuture<'a> {
        Box::pin(async move {
            if let AccountingTool::Qemu { .. } = &self.tool {
                let qemu = self.qemu_for(Path::new(&guest[0]))?;
                ensure_plugin_support(&qemu)?;
            }
            let baseline_command = if self.native_performance {
                guest.to_vec()
            } else {
                let qemu = self.qemu_for(Path::new(&guest[0]))?;
                self.command(&qemu, guest, None)?
            };
            println!(
                "Run 1: collecting {} performance data for '{}'",
                if self.native_performance {
                    "native"
                } else {
                    "QEMU-hosted"
                },
                guest.join(" ")
            );
            let baseline = profile_command(
                dispatcher.clone(),
                &baseline_command,
                Vec::new(),
                self.memory_profile
                    .then(|| output_directory.join("memory-rss.txt")),
                super::bandwidth_timeline_path(output_directory),
            )
            .await?;
            let timing_path = output_directory.join("memory-native.json");
            let timing_file = File::create(&timing_path)
                .with_context(|| format!("create '{}'", timing_path.display()))?;
            serde_json::to_writer(
                timing_file,
                &NativeTimingArtifact {
                    pid: baseline.pid,
                    start_ns: baseline.start_ns,
                    end_ns: baseline.end_ns,
                },
            )?;
            publish_baseline_region(
                dispatcher.clone(),
                baseline.pid,
                baseline.start_ns,
                baseline.end_ns,
                &guest[0],
            )
            .await;

            let counts_path = output_directory.join("qemu-roofline.counts");
            let progress_path = matches!(&self.tool, AccountingTool::DynamoRio { .. })
                .then(|| {
                    output_directory.join(format!(".dynamorio-progress-{}", uuid::Uuid::now_v7()))
                })
                .filter(|_| baseline.instructions != 0);
            let (accounting_command, accounting_env) =
                self.accounting_command(guest, &counts_path, progress_path.as_deref())?;
            println!(
                "Run 2: collecting {} instruction and memory accounting for '{}'",
                self.tool.name(),
                guest.join(" ")
            );
            let process = Process::new(&accounting_command, &accounting_env)?;
            let mut progress =
                progress_path.map(|path| DynamoRioProgress::start(path, baseline.instructions));
            process.cont();
            let wait = process.wait();
            if let Some(progress) = progress.as_mut() {
                progress.finish(wait.is_ok() && process.exit_code() == Some(0));
            }
            wait?;
            super::ensure_process_success(
                &process,
                &format!("{} Roofline accounting run", self.tool.name()),
            )?;
            let counts =
                parse_counts(&std::fs::read_to_string(&counts_path).with_context(|| {
                    format!(
                        "{} roofline accounting did not produce '{}'",
                        self.tool.name(),
                        counts_path.display()
                    )
                })?)?;
            let cfg_path = counts_path.with_extension("cfg");
            let cfg_capture =
                parse_dynamic_cfg(&std::fs::read_to_string(&cfg_path).with_context(|| {
                    format!(
                        "{} roofline accounting did not produce dynamic CFG '{}'",
                        self.tool.name(),
                        cfg_path.display()
                    )
                })?)?;
            validate_cfg_totals(&cfg_capture, &counts)?;
            let loops_path = output_directory.join("qemu-roofline.loops.json");
            let aggregate_blocks =
                matches!(&self.tool, AccountingTool::DynamoRio { .. }) && !self.memory_profile;
            let (natural_loops, irreducible_cycles) = write_loop_artifact(
                &loops_path,
                &cfg_capture,
                Path::new(&guest[0]),
                aggregate_blocks,
            )?;
            let cfg_kind = if aggregate_blocks {
                "aggregate CFG"
            } else {
                "dynamic CFG"
            };
            println!(
                "{} {cfg_kind}: {natural_loops} natural loops, {irreducible_cycles} irreducible cycles; candidates saved to '{}'",
                self.tool.name(),
                loops_path.display()
            );
            if counts.rvv_state_errors != 0 {
                anyhow::bail!(
                    "{} roofline could not read RVV state for {} executed vector instructions",
                    self.tool.name(),
                    counts.rvv_state_errors
                );
            }
            if counts.unclassified_instructions != 0 {
                eprintln!(
                    "Warning: TMDL could not classify {} executed RISC-V instructions; Roofline operation totals are conservative",
                    counts.unclassified_instructions
                );
            }
            if counts.child_process_seen != 0 {
                eprintln!(
                    "Warning: child processes were detected and excluded from memory accounting"
                );
            }
            let mut method = self.method.clone();
            method.warnings.extend(baseline.warnings.iter().cloned());
            // The replay reproduces the program's work; the timed run executes
            // that work plus whatever its threads retire while waiting for each
            // other. How much waiting that is depends on how fast the run went,
            // so the two totals are not comparable in that direction: an OpenMP
            // barrier at the default active wait policy retires three orders of
            // magnitude more than the loops it waits for. Only a native total
            // below the replay's means the runs executed different programs.
            if baseline.instructions != 0 && counts.instructions != 0 {
                let native = baseline.instructions as f64;
                let replayed = counts.instructions as f64;
                if native < replayed * 0.95 {
                    let warning = format!(
                        "the timed run retired fewer instructions than {} replayed ({} against {}); the two runs did not execute the same work and replay-dependent memory rates are degraded",
                        self.tool.name(),
                        baseline.instructions,
                        counts.instructions
                    );
                    eprintln!("Warning: {warning}");
                    method.quality = "replay-diverged".to_string();
                    method.warnings.push(warning);
                } else if native > replayed * 2.0 {
                    let warning = format!(
                        "the timed run retired {:.0}x the instructions {} accounted for; two things look like this and the totals cannot tell them apart — threads retiring instructions while waiting rather than computing (an OpenMP barrier at the default active wait policy is the usual source, and its wait loop then dominates the hotspots), or a replay that did not execute all of the work the timed run did",
                        native / replayed,
                        self.tool.name()
                    );
                    eprintln!("Warning: {warning}");
                    method.warnings.push(warning);
                }
            }
            if counts.child_process_seen != 0 {
                method.warnings.push(
                    "child processes were detected and excluded; v1 reports only the launched process and its threads"
                        .to_string(),
                );
            }
            publish_accounting_region(
                dispatcher,
                process.pid(),
                monotonic_timestamp()?,
                &guest[0],
                &counts,
            )
            .await;

            Ok(qemu_info(
                baseline,
                process.pid(),
                self.native_performance,
                method,
            ))
        })
    }
}

fn resolve_executable(executable: &Path) -> Result<PathBuf> {
    if executable.components().count() == 1 {
        which::which(executable)
            .with_context(|| format!("could not find QEMU executable '{}'", executable.display()))
    } else {
        Ok(executable.to_owned())
    }
}

fn ensure_plugin_support(qemu: &Path) -> Result<()> {
    let output = std::process::Command::new(qemu)
        .arg("--help")
        .output()
        .with_context(|| format!("run '{} --help'", qemu.display()))?;
    if !qemu_help_has_plugin(&output.stdout) && !qemu_help_has_plugin(&output.stderr) {
        anyhow::bail!(
            "QEMU executable '{}' has no TCG plugin support; use a QEMU user-mode build whose --help lists -plugin",
            qemu.display()
        );
    }
    let qemu_data = std::fs::read(qemu)
        .with_context(|| format!("read QEMU executable '{}'", qemu.display()))?;
    let qemu_object = object::File::parse(qemu_data.as_slice())
        .with_context(|| format!("parse QEMU executable '{}'", qemu.display()))?;
    let exported = qemu_object
        .dynamic_symbols()
        .filter_map(|symbol| symbol.name().ok())
        .collect::<BTreeSet<_>>();
    let missing = [
        "qemu_plugin_tb_vaddr",
        "qemu_plugin_insn_vaddr",
        "qemu_plugin_insn_size",
        "qemu_plugin_start_code",
        "qemu_plugin_end_code",
        "qemu_plugin_entry_code",
        "qemu_plugin_get_registers",
        "qemu_plugin_read_register",
    ]
    .into_iter()
    .filter(|symbol| !exported.contains(symbol))
    .collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "QEMU executable '{}' is missing plugin APIs required for accurate Roofline capture: {}",
            qemu.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn qemu_help_has_plugin(output: &[u8]) -> bool {
    String::from_utf8_lossy(output)
        .lines()
        .any(|line| line.trim_start().starts_with("-plugin"))
}

fn qemu_info(
    baseline: ProfiledRun,
    accounting_pid: i32,
    native_performance: bool,
    method: RooflineMethodInfo,
) -> ScenarioInfo {
    ScenarioInfo::Roofline(RooflineInfo {
        backend: if native_performance {
            "hybrid-qemu"
        } else {
            "qemu"
        }
        .to_string(),
        perf_pid: baseline.pid,
        counters: baseline.counters,
        inst_pid: accounting_pid,
        method: Some(Box::new(method)),
    })
}

async fn publish_baseline_region(
    dispatcher: Arc<EventDispatcher>,
    pid: i32,
    start: u64,
    end: u64,
    guest: &str,
) {
    let id = publish_start(&dispatcher, pid, start, guest).await;
    publish_end(&dispatcher, pid, end, id).await;
}

async fn publish_accounting_region(
    dispatcher: Arc<EventDispatcher>,
    pid: i32,
    timestamp: u64,
    guest: &str,
    counts: &Counts,
) {
    let id = publish_start(&dispatcher, pid, timestamp, guest).await;
    for (ty, value) in [
        (EventType::RooflineBytesLoad, counts.dram_bytes_load),
        (EventType::RooflineBytesStore, counts.dram_bytes_store),
        (EventType::RooflineScalarIntOps, counts.scalar_int_ops),
        (EventType::RooflineScalarFloatOps, counts.scalar_float_ops),
        (EventType::RooflineScalarDoubleOps, counts.scalar_double_ops),
        (EventType::RooflineVectorIntOps, counts.vector_int_ops),
        (EventType::RooflineVectorFloatOps, counts.vector_float_ops),
        (EventType::RooflineVectorDoubleOps, counts.vector_double_ops),
    ] {
        dispatcher
            .publish_event(synthetic_event(
                &dispatcher,
                pid,
                timestamp,
                ty,
                0,
                id,
                value,
            ))
            .await;
    }
    publish_end(&dispatcher, pid, timestamp, id).await;
}

async fn publish_start(
    dispatcher: &Arc<EventDispatcher>,
    pid: i32,
    timestamp: u64,
    guest: &str,
) -> u64 {
    let id = dispatcher.unique_id();
    let function_name = dispatcher.string_id_async("[QEMU whole process]").await;
    let file_name = dispatcher.string_id_async(guest).await;
    let mut event = synthetic_event(
        dispatcher,
        pid,
        timestamp,
        EventType::RooflineLoopStart,
        id,
        0,
        0,
    );
    event.callstack = smallvec![CallFrame::Location(Location {
        function_name,
        file_name,
        line: 0,
    })];
    dispatcher.publish_event(event).await;
    id
}

async fn publish_end(
    dispatcher: &Arc<EventDispatcher>,
    pid: i32,
    timestamp: u64,
    correlation_id: u64,
) {
    dispatcher
        .publish_event(synthetic_event(
            dispatcher,
            pid,
            timestamp,
            EventType::RooflineLoopEnd,
            0,
            correlation_id,
            0,
        ))
        .await;
}

fn synthetic_event(
    dispatcher: &EventDispatcher,
    pid: i32,
    timestamp: u64,
    ty: EventType,
    unique_id: u64,
    parent_or_correlation_id: u64,
    value: u64,
) -> Event {
    let is_counter = matches!(
        ty,
        EventType::RooflineBytesLoad
            | EventType::RooflineBytesStore
            | EventType::RooflineScalarIntOps
            | EventType::RooflineScalarFloatOps
            | EventType::RooflineScalarDoubleOps
            | EventType::RooflineVectorIntOps
            | EventType::RooflineVectorFloatOps
            | EventType::RooflineVectorDoubleOps
    );
    Event {
        unique_id: if unique_id == 0 {
            dispatcher.unique_id()
        } else {
            unique_id
        },
        correlation_id: if is_counter {
            0
        } else {
            parent_or_correlation_id
        },
        parent_id: if is_counter {
            parent_or_correlation_id
        } else {
            0
        },
        ty,
        thread_id: 0,
        process_id: pid as u32,
        cpu: u32::MAX,
        time_enabled: 0,
        time_running: 0,
        value,
        timestamp,
        name: 0,
        callstack: smallvec![],
        lbr_callstack: Vec::new(),
        user_regs: None,
        user_stack: Vec::new(),
    }
}

fn parse_counts(input: &str) -> Result<Counts> {
    let mut values = HashMap::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let (name, value) = line
            .split_once('=')
            .with_context(|| format!("invalid QEMU roofline count '{line}'"))?;
        let value = value
            .parse::<u64>()
            .with_context(|| format!("invalid QEMU roofline count '{line}'"))?;
        if values.insert(name, value).is_some() {
            anyhow::bail!("duplicate QEMU roofline count '{name}'");
        }
    }
    let get = |name| {
        values
            .get(name)
            .copied()
            .with_context(|| format!("QEMU roofline output is missing required count '{name}'"))
    };
    Ok(Counts {
        scalar_int_ops: get("scalar_int_ops")?,
        scalar_float_ops: get("scalar_float_ops")?,
        scalar_double_ops: get("scalar_double_ops")?,
        vector_int_ops: get("vector_int_ops")?,
        vector_float_ops: get("vector_float_ops")?,
        vector_double_ops: get("vector_double_ops")?,
        bytes_load: get("bytes_load")?,
        bytes_store: get("bytes_store")?,
        dram_bytes_load: get("dram_bytes_load")?,
        dram_bytes_store: get("dram_bytes_store")?,
        rvv_state_errors: get("rvv_state_errors")?,
        unclassified_instructions: get("unclassified_instructions")?,
        instructions: get("instructions")?,
        child_process_seen: get("child_process_seen")?,
    })
}

fn parse_dynamic_cfg(input: &str) -> Result<DynamicCfgCapture> {
    let mut capture = DynamicCfgCapture::default();
    let mut version_seen = false;
    let mut thread_blocks = Vec::new();
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(version) = line.strip_prefix("miniperf-qemu-cfg=") {
            if version_seen {
                anyhow::bail!("duplicate QEMU dynamic CFG version");
            }
            if version != "4" && version != "5" {
                anyhow::bail!("unsupported QEMU dynamic CFG version '{version}'");
            }
            version_seen = true;
            continue;
        }

        let fields = line.split_whitespace().collect::<Vec<_>>();
        match fields.as_slice() {
            ["cache", line_size, capacity, associativity, model]
                if matches!(*model, "write-back-no-rfo" | "write-back-write-allocate") =>
            {
                if capture.cache.is_some() {
                    anyhow::bail!("duplicate QEMU dynamic CFG cache metadata");
                }
                let cache = CacheInfo {
                    line_size: parse_decimal(line_size, line)?,
                    capacity: parse_decimal(capacity, line)?,
                    associativity: parse_decimal(associativity, line)?,
                };
                if !cache.line_size.is_power_of_two()
                    || cache.associativity == 0
                    || cache.capacity < cache.line_size * cache.associativity
                {
                    anyhow::bail!("invalid QEMU dynamic CFG cache metadata '{line}'");
                }
                capture.cache = Some(cache);
            }
            ["image", start, end, entry] => {
                if capture.image.is_some() {
                    anyhow::bail!("duplicate QEMU dynamic CFG image metadata");
                }
                let image = ImageInfo {
                    start: parse_address(start)?,
                    end: parse_address(end)?,
                    entry: parse_address(entry)?,
                };
                // For dynamically linked Linux executables QEMU reports the
                // interpreter's process entry, which legitimately lies outside
                // the main executable's text range. The main-image load bias is
                // derived from its executable ELF segment instead.
                if image.start >= image.end {
                    anyhow::bail!("invalid QEMU dynamic CFG image range '{line}'");
                }
                capture.image = Some(image);
            }
            ["entry", address] => {
                let address = parse_address(address)?;
                if !capture.entries.insert(address) {
                    anyhow::bail!("duplicate QEMU dynamic CFG entry '{address:#x}'");
                }
            }
            ["edge", from, to, executions] => {
                let edge = (parse_address(from)?, parse_address(to)?);
                let executions = parse_decimal(executions, line)?;
                if executions == 0 {
                    anyhow::bail!("QEMU dynamic CFG edge has zero executions: '{line}'");
                }
                if capture.edges.insert(edge, executions).is_some() {
                    anyhow::bail!(
                        "duplicate QEMU dynamic CFG edge '{:#x}' -> '{:#x}'",
                        edge.0,
                        edge.1
                    );
                }
            }
            [
                "block",
                address,
                end,
                executions,
                scalar_int,
                scalar_float,
                scalar_double,
                vector_int,
                vector_float,
                vector_double,
                bytes_load,
                bytes_store,
                arch_bytes_load,
                arch_bytes_store,
                unclassified,
            ] => {
                let address = parse_address(address)?;
                let counts = BlockCounts {
                    end: parse_address(end)?,
                    executions: parse_decimal(executions, line)?,
                    scalar_int_ops: parse_decimal(scalar_int, line)?,
                    scalar_float_ops: parse_decimal(scalar_float, line)?,
                    scalar_double_ops: parse_decimal(scalar_double, line)?,
                    vector_int_ops: parse_decimal(vector_int, line)?,
                    vector_float_ops: parse_decimal(vector_float, line)?,
                    vector_double_ops: parse_decimal(vector_double, line)?,
                    bytes_load: parse_decimal(bytes_load, line)?,
                    bytes_store: parse_decimal(bytes_store, line)?,
                    arch_bytes_load: parse_decimal(arch_bytes_load, line)?,
                    arch_bytes_store: parse_decimal(arch_bytes_store, line)?,
                    unclassified_instructions: parse_decimal(unclassified, line)?,
                    threads: Vec::new(),
                };
                if counts.executions == 0 {
                    anyhow::bail!("QEMU dynamic CFG block has zero executions: '{line}'");
                }
                if counts.end <= address {
                    anyhow::bail!("QEMU dynamic CFG block has an invalid range: '{line}'");
                }
                if capture.blocks.insert(address, counts).is_some() {
                    anyhow::bail!("duplicate QEMU dynamic CFG block '{address:#x}'");
                }
            }
            ["tblock", address, thread, executions] => {
                let executions = parse_decimal(executions, line)?;
                if executions == 0 {
                    anyhow::bail!("QEMU dynamic CFG thread block has zero executions: '{line}'");
                }
                thread_blocks.push((
                    parse_address(address)?,
                    u32::try_from(parse_decimal(thread, line)?)
                        .with_context(|| format!("invalid QEMU dynamic CFG thread in '{line}'"))?,
                    executions,
                ));
            }
            _ => anyhow::bail!("invalid QEMU dynamic CFG line '{line}'"),
        }
    }
    if !version_seen {
        anyhow::bail!("QEMU dynamic CFG has no format version");
    }
    if capture.blocks.is_empty() {
        anyhow::bail!("QEMU dynamic CFG contains no executed blocks");
    }
    if capture.image.is_none() {
        anyhow::bail!("QEMU dynamic CFG contains no main-image metadata");
    }
    if capture.cache.is_none() {
        anyhow::bail!("QEMU dynamic CFG contains no cache-model metadata");
    }
    for (address, thread, executions) in thread_blocks {
        let block = capture.blocks.get_mut(&address).with_context(|| {
            format!("QEMU dynamic CFG thread block references unknown block '{address:#x}'")
        })?;
        if block
            .threads
            .iter()
            .any(|(candidate, _)| *candidate == thread)
        {
            anyhow::bail!("duplicate QEMU dynamic CFG thread block '{address:#x}' thread {thread}");
        }
        block.threads.push((thread, executions));
        let counted = block.threads.iter().map(|(_, count)| *count).sum::<u64>();
        if counted > block.executions {
            anyhow::bail!(
                "QEMU dynamic CFG block '{address:#x}' has {counted} per-thread executions but {} in total",
                block.executions
            );
        }
    }
    for address in capture
        .entries
        .iter()
        .copied()
        .chain(capture.edges.keys().flat_map(|(from, to)| [*from, *to]))
    {
        if !capture.blocks.contains_key(&address) {
            anyhow::bail!("QEMU dynamic CFG references unknown block '{address:#x}'");
        }
    }
    Ok(capture)
}

fn parse_address(value: &str) -> Result<u64> {
    let value = value
        .strip_prefix("0x")
        .with_context(|| format!("QEMU dynamic CFG address is not hexadecimal: '{value}'"))?;
    u64::from_str_radix(value, 16)
        .with_context(|| format!("invalid QEMU dynamic CFG address '0x{value}'"))
}

fn parse_decimal(value: &str, line: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid QEMU dynamic CFG count in '{line}'"))
}

fn validate_cfg_totals(capture: &DynamicCfgCapture, counts: &Counts) -> Result<()> {
    let totals = sum_blocks(capture.blocks.values());
    let expected = BlockCounts {
        scalar_int_ops: counts.scalar_int_ops,
        scalar_float_ops: counts.scalar_float_ops,
        scalar_double_ops: counts.scalar_double_ops,
        vector_int_ops: counts.vector_int_ops,
        vector_float_ops: counts.vector_float_ops,
        vector_double_ops: counts.vector_double_ops,
        bytes_load: counts.dram_bytes_load,
        bytes_store: counts.dram_bytes_store,
        // Every architectural access is attributed to the block that issued
        // it, so unlike modeled DRAM traffic these must agree exactly.
        arch_bytes_load: counts.bytes_load,
        arch_bytes_store: counts.bytes_store,
        unclassified_instructions: counts.unclassified_instructions,
        ..BlockCounts::default()
    };
    let mut comparable = totals;
    comparable.executions = 0;
    // Dirty lines still resident at process exit are flushed into the
    // whole-process traffic total. They have no honest final basic-block
    // owner, so per-block stores may be smaller but can never be larger.
    let stores_are_valid = comparable.bytes_store <= expected.bytes_store;
    comparable.bytes_store = expected.bytes_store;
    if !stores_are_valid || comparable != expected {
        anyhow::bail!(
            "QEMU per-block accounting does not match whole-process totals: blocks={comparable:?}, process={expected:?}"
        );
    }
    Ok(())
}

fn write_loop_artifact(
    path: &Path,
    capture: &DynamicCfgCapture,
    executable: &Path,
    aggregate_blocks: bool,
) -> Result<(usize, usize)> {
    let image = capture
        .image
        .context("QEMU dynamic CFG has no image metadata")?;
    let object_data = std::fs::read(executable)
        .with_context(|| format!("read Roofline executable '{}'", executable.display()))?;
    let object = object::File::parse(object_data.as_slice())
        .with_context(|| format!("parse Roofline executable '{}'", executable.display()))?;
    let link_text_start = executable_segment_start(&object)
        .context("Roofline executable has no executable ELF load segment")?;
    let load_bias = image.start.checked_sub(link_text_start).with_context(|| {
        format!(
            "QEMU main-image start {:#x} precedes executable segment {link_text_start:#x}",
            image.start
        )
    })?;
    let runtime_entry = object
        .entry()
        .checked_add(load_bias)
        .context("Roofline executable entry address overflow")?;
    if !((image.start..image.end).contains(&runtime_entry)) {
        anyhow::bail!(
            "derived main executable entry {runtime_entry:#x} is outside QEMU image {:#x}..{:#x}",
            image.start,
            image.end
        );
    }
    let loader = addr2line::Loader::new(executable).ok();
    let in_main_image = |address: u64| address >= image.start && address < image.end;
    let main_blocks = capture
        .blocks
        .keys()
        .copied()
        .filter(|address| in_main_image(*address))
        .collect::<BTreeSet<_>>();
    if main_blocks.is_empty() {
        anyhow::bail!("QEMU dynamic CFG has no blocks in the main executable image");
    }
    let mut graph = ControlFlowGraph::default();
    let mut main_entries = capture
        .entries
        .iter()
        .copied()
        .filter(|address| in_main_image(*address))
        .collect::<BTreeSet<_>>();
    for &block in &main_blocks {
        let has_main_predecessor = capture
            .edges
            .keys()
            .any(|(from, to)| *to == block && main_blocks.contains(from));
        if !has_main_predecessor {
            main_entries.insert(block);
        }
    }
    let artifact_entries = main_entries.iter().copied().map(hex).collect::<Vec<_>>();
    for entry in main_entries {
        graph.add_entry(entry);
    }
    for &(from, to) in capture.edges.keys() {
        if main_blocks.contains(&from) && main_blocks.contains(&to) {
            graph.add_edge(from, to);
        }
    }
    let analysis = graph.analyze();
    let loops = analysis
        .natural_loops
        .iter()
        .map(|loop_info| {
            let module_offset = loop_info.header.saturating_sub(load_bias);
            let (function, file, line) = resolve_source(loader.as_ref(), module_offset);
            let self_blocks = loop_info
                .blocks
                .iter()
                .filter(|block| {
                    !analysis.natural_loops.iter().any(|candidate| {
                        candidate.header != loop_info.header
                            && candidate.blocks.len() < loop_info.blocks.len()
                            && loop_info.blocks.is_superset(&candidate.blocks)
                            && candidate.blocks.contains(block)
                    })
                })
                .filter_map(|address| capture.blocks.get(address));
            LoopArtifact {
                header: hex(loop_info.header),
                module_offset: hex(module_offset),
                function,
                file,
                line,
                latches: loop_info.latches.iter().copied().map(hex).collect(),
                blocks: loop_info.blocks.iter().copied().map(hex).collect(),
                block_ranges: loop_info
                    .blocks
                    .iter()
                    .filter_map(|start| {
                        capture.blocks.get(start).map(|counts| BlockRangeArtifact {
                            runtime_start: hex(*start),
                            runtime_end: hex(counts.end),
                            module_start: hex(start.saturating_sub(load_bias)),
                            module_end: hex(counts.end.saturating_sub(load_bias)),
                        })
                    })
                    .collect(),
                parent_header: loop_info
                    .parent
                    .map(|parent| hex(analysis.natural_loops[parent].header)),
                trip_count: loop_info
                    .latches
                    .iter()
                    .filter_map(|latch| capture.edges.get(&(*latch, loop_info.header)))
                    .copied()
                    .sum(),
                inclusive: sum_blocks(
                    loop_info
                        .blocks
                        .iter()
                        .filter_map(|address| capture.blocks.get(address)),
                ),
                self_counts: sum_blocks(self_blocks),
                threads: loop_threads(
                    loop_info
                        .blocks
                        .iter()
                        .filter_map(|address| capture.blocks.get(address)),
                ),
            }
        })
        .collect::<Vec<_>>();
    let irreducible_cycles = analysis
        .irreducible_cycles
        .iter()
        .map(|cycle| cycle.iter().copied().map(hex).collect())
        .collect::<Vec<_>>();
    let cache = capture
        .cache
        .context("QEMU dynamic CFG has no cache-model metadata")?;
    let (model, warnings, traffic_model) = if aggregate_blocks {
        (
            "static-successor CFG with exact aggregate block counts",
            vec![
                "Direct successor topology is static; conditional edge weights are approximate.",
                "Loop bytes are exact architectural operand traffic; modeled DRAM traffic is not collected.",
                "Source locations require debug information; optimized code may map multiple loops to one line.",
            ],
            "architectural-operands",
        )
    } else {
        (
            "observed translation-block CFG with summarized calls",
            vec![
                "Only control-flow edges executed during the accounting run are present.",
                "Call/return pairs are summarized; non-local control transfers can split a region.",
                "Loop bytes are deterministic shared-LLC model traffic, not hardware memory-controller measurements.",
                "The cache model uses write allocation/RFO and dirty writeback; non-temporal stores are conservative.",
                "Source locations require debug information; optimized code may map multiple loops to one line.",
            ],
            "shared-llc-write-back-write-allocate",
        )
    };
    let artifact = DynamicCfgArtifact {
        format_version: 4,
        model,
        executable: executable.to_string_lossy().into_owned(),
        warnings,
        cache_line_size: cache.line_size,
        llc_capacity: cache.capacity,
        llc_associativity: cache.associativity,
        traffic_model,
        image_start: hex(image.start),
        image_end: hex(image.end),
        image_entry: hex(runtime_entry),
        load_bias: hex(load_bias),
        entries: artifact_entries,
        loops,
        irreducible_cycles,
    };
    let natural_count = artifact.loops.len();
    let irreducible_count = artifact.irreducible_cycles.len();
    serde_json::to_writer_pretty(
        File::create(path)
            .with_context(|| format!("create QEMU loop artifact '{}'", path.display()))?,
        &artifact,
    )
    .with_context(|| format!("write QEMU loop artifact '{}'", path.display()))?;
    Ok((natural_count, irreducible_count))
}

fn executable_segment_start(object: &object::File<'_>) -> Option<u64> {
    object
        .segments()
        .filter(|segment| {
            matches!(
                segment.flags(),
                SegmentFlags::Elf { p_flags } if p_flags & object::elf::PF_X != 0
            )
        })
        .map(|segment| segment.address())
        .min()
}

fn resolve_source(
    loader: Option<&addr2line::Loader>,
    address: u64,
) -> (Option<String>, Option<String>, Option<u32>) {
    let Some(loader) = loader else {
        return (None, None, None);
    };
    let mut function = loader
        .find_symbol(address)
        .map(|symbol| addr2line::demangle_auto(Cow::Borrowed(symbol), None).into_owned());
    let mut file = None;
    let mut line = None;
    if let Ok(mut frames) = loader.find_frames(address) {
        while let Ok(Some(frame)) = frames.next() {
            if function.is_none() {
                function = frame
                    .function
                    .and_then(|name| name.demangle().ok().map(Cow::into_owned));
            }
            if file.is_none() {
                file = frame
                    .location
                    .as_ref()
                    .and_then(|location| location.file.map(str::to_owned));
            }
            if line.is_none() {
                line = frame.location.and_then(|location| location.line);
            }
            if function.is_some() && file.is_some() && line.is_some() {
                break;
            }
        }
    }
    (function, file, line)
}

fn loop_threads<'a>(blocks: impl IntoIterator<Item = &'a BlockCounts>) -> Vec<LoopThreadArtifact> {
    let mut by_thread: BTreeMap<u32, LoopThreadArtifact> = BTreeMap::new();
    for block in blocks {
        for &(thread, executions) in &block.threads {
            let share = executions as f64 / block.executions.max(1) as f64;
            let apportion = |count: u64| (count as f64 * share).round() as u64;
            let entry = by_thread.entry(thread).or_default();
            entry.thread = thread;
            entry.executions = entry.executions.saturating_add(executions);
            entry.scalar_int_ops += apportion(block.scalar_int_ops);
            entry.scalar_float_ops += apportion(block.scalar_float_ops);
            entry.scalar_double_ops += apportion(block.scalar_double_ops);
            entry.vector_int_ops += apportion(block.vector_int_ops);
            entry.vector_float_ops += apportion(block.vector_float_ops);
            entry.vector_double_ops += apportion(block.vector_double_ops);
            entry.arch_bytes_load += apportion(block.arch_bytes_load);
            entry.arch_bytes_store += apportion(block.arch_bytes_store);
        }
    }
    by_thread.into_values().collect()
}

fn sum_blocks<'a>(blocks: impl IntoIterator<Item = &'a BlockCounts>) -> BlockCounts {
    let mut total = BlockCounts::default();
    for block in blocks {
        total.executions = total.executions.saturating_add(block.executions);
        total.scalar_int_ops = total.scalar_int_ops.saturating_add(block.scalar_int_ops);
        total.scalar_float_ops = total
            .scalar_float_ops
            .saturating_add(block.scalar_float_ops);
        total.scalar_double_ops = total
            .scalar_double_ops
            .saturating_add(block.scalar_double_ops);
        total.vector_int_ops = total.vector_int_ops.saturating_add(block.vector_int_ops);
        total.vector_float_ops = total
            .vector_float_ops
            .saturating_add(block.vector_float_ops);
        total.vector_double_ops = total
            .vector_double_ops
            .saturating_add(block.vector_double_ops);
        total.bytes_load = total.bytes_load.saturating_add(block.bytes_load);
        total.bytes_store = total.bytes_store.saturating_add(block.bytes_store);
        total.arch_bytes_load = total.arch_bytes_load.saturating_add(block.arch_bytes_load);
        total.arch_bytes_store = total
            .arch_bytes_store
            .saturating_add(block.arch_bytes_store);
        total.unclassified_instructions = total
            .unclassified_instructions
            .saturating_add(block.unclassified_instructions);
    }
    total
}

fn hex(address: u64) -> String {
    format!("{address:#x}")
}

#[cfg(target_os = "linux")]
fn default_plugin_path() -> PathBuf {
    super::package_path("libminiperf_qemu_roofline.so").unwrap_or_else(|| {
        super::executable_directory()
            .unwrap_or_default()
            .join("libminiperf_qemu_roofline.so")
    })
}

fn qemu_binary_for_architecture(architecture: object::Architecture) -> Option<&'static str> {
    match architecture {
        object::Architecture::Riscv64 => Some("qemu-riscv64"),
        object::Architecture::Riscv32 => Some("qemu-riscv32"),
        object::Architecture::X86_64 => Some("qemu-x86_64"),
        _ => None,
    }
}
