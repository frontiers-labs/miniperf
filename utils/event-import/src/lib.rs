use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsStr,
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
};

use pmu_data::{
    Alias, EventDesc, Metric, MetricExpression, PlatformDesc, TmaConstant, TmaGroup, TmaMetric,
    TmaScenario,
};
use serde_json::Value;

pub type ImportResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Converts Intel perfmon or Linux perf core event and metric JSON files.
///
/// `source` may be a direct Intel/perf-compatible JSON file or a Linux perf
/// family directory containing multiple JSON files.
pub fn import_intel(source: &Path, family_id: &str, name: &str) -> ImportResult<PlatformDesc> {
    let files = json_sources(source)?;

    let mut events = BTreeMap::<String, EventDesc>::new();
    let mut metrics = BTreeMap::<String, Metric>::new();
    for path in files {
        let document: Value = serde_json::from_reader(BufReader::new(File::open(&path)?))?;
        for value in intel_records(document, &path)? {
            if let Some(event) = convert_event(&value)? {
                events.insert(event.name.clone(), event);
            }
            if let Some(metric) = convert_metric(&value) {
                metrics.insert(metric.name.clone(), metric);
            }
        }
    }

    let aliases = intel_aliases(&events);
    Ok(PlatformDesc {
        family_id: family_id.to_owned(),
        name: name.to_owned(),
        vendor: "Intel".to_owned(),
        arch: "x86_64".to_owned(),
        max_counters: Some(8),
        leader_event: None,
        events: events.into_values().collect(),
        aliases: Some(aliases),
        metrics: metrics.into_values().collect(),
        scenarios: None,
    })
}

/// Backwards-compatible name for importing a Linux perf family directory.
pub fn import_intel_linux(
    source: &Path,
    family_id: &str,
    name: &str,
) -> ImportResult<PlatformDesc> {
    import_intel(source, family_id, name)
}

/// Converts a Linux perf AMD family directory (`arch/x86/amdzen*`).
///
/// Uncore events (those with a `Unit`) are skipped: they belong to the
/// `amd_l3`, `amd_df` and `amd_umc` PMUs, and a `PlatformDesc` describes the
/// core PMU only. The top-down scenario is derived from the events the family
/// has, with the dispatch width taken from perf's own `pipeline.json`.
pub fn import_amd(source: &Path, family_id: &str, name: &str) -> ImportResult<PlatformDesc> {
    let mut events = BTreeMap::<String, EventDesc>::new();
    let mut metric_exprs = BTreeMap::<String, String>::new();
    for path in json_sources(source)? {
        let document: Value = serde_json::from_reader(BufReader::new(File::open(&path)?))?;
        for value in intel_records(document, &path)? {
            if let (Some(name), Some(expr)) = (
                string_field(&value, "MetricName"),
                string_field(&value, "MetricExpr"),
            ) {
                metric_exprs.insert(name.to_owned(), expr.to_owned());
            }
            if let Some(event) = convert_amd_event(&value)? {
                events.insert(event.name.clone(), event);
            }
        }
    }

    // perf metrics apply a counter mask inline (`cpu@event\,cmask\=0x8@`). An
    // event table has no modifiers, so each masked use becomes its own event.
    for expr in metric_exprs.values() {
        for (base, cmask) in masked_events(expr) {
            let Some(event) = events.get(&base) else {
                continue;
            };
            let masked = EventDesc {
                name: masked_event_name(&base, cmask),
                desc: format!(
                    "Cycles in which this event counts at least {cmask}: {}",
                    event.desc
                ),
                code: event.code | (cmask << 24),
            };
            events.insert(masked.name.clone(), masked);
        }
    }

    let scenario = amd_slots_scenario(&events, &metric_exprs)
        .or_else(|| amd_stall_scenario(&events))
        .ok_or_else(|| format!("{} has no top-down events", source.display()))?;
    Ok(PlatformDesc {
        family_id: family_id.to_owned(),
        name: name.to_owned(),
        vendor: "AMD".to_owned(),
        arch: "x86_64".to_owned(),
        // Zen has six counters and none of them is fixed: `cycles` and
        // `instructions` take two in every sampling group.
        max_counters: Some(4),
        leader_event: None,
        aliases: Some(amd_aliases(&events)),
        events: events.into_values().collect(),
        metrics: vec![Metric {
            name: "IPC".to_owned(),
            desc: "Instructions retired per CPU cycle.".to_owned(),
            expression: MetricExpression("instructions / cycles".to_owned()),
            unit: Some("insn/cycle".to_owned()),
        }],
        scenarios: Some(vec![scenario]),
    })
}

/// Encode one AMD core event as a PERF_EVTSEL value. The event select is 12
/// bits wide and its top four live at bits 32-35, above the unit mask.
fn convert_amd_event(value: &Value) -> ImportResult<Option<EventDesc>> {
    let (Some(name), Some(event_code)) = (
        string_field(value, "EventName"),
        string_field(value, "EventCode"),
    ) else {
        return Ok(None);
    };
    if value.get("Unit").is_some() || value.get("Deprecated").is_some() {
        return Ok(None);
    }
    let event = parse_number(event_code)?;
    let umask = parse_optional(value, "UMask")?;
    if event > 0xfff || umask > 0xff {
        return Err(format!("{name}: event {event:#x} umask {umask:#x} does not fit").into());
    }
    let code = (event & 0xff)
        | (umask << 8)
        | (parse_optional(value, "EdgeDetect")? << 18)
        | (parse_optional(value, "Invert")? << 23)
        | (parse_optional(value, "CounterMask")? << 24)
        | ((event >> 8) << 32);
    let desc = string_field(value, "PublicDescription")
        .or_else(|| string_field(value, "BriefDescription"))
        .unwrap_or("");
    Ok(Some(EventDesc {
        name: name.to_owned(),
        desc: desc.to_owned(),
        code,
    }))
}

/// Every `cpu@<event>\,cmask\=<n>@` use in a perf metric expression.
fn masked_events(expr: &str) -> Vec<(String, u64)> {
    expr.split("cpu@")
        .skip(1)
        .filter_map(|rest| {
            let (term, _) = rest.split_once('@')?;
            let (event, cmask) = term.split_once("\\,cmask\\=")?;
            Some((event.to_owned(), parse_number(cmask).ok()?))
        })
        .collect()
}

fn masked_event_name(event: &str, cmask: u64) -> String {
    format!("{event}_cmask{cmask}")
}

/// Zen kernels map the portable events to these encodings
/// (`amd_zen1_perfmon_event_map` in `arch/x86/events/amd/core.c`).
fn amd_aliases(events: &BTreeMap<String, EventDesc>) -> Vec<Alias> {
    [
        ("cycles", 0x76),
        ("instructions", 0xc0),
        ("branches", 0xc2),
        ("branch_misses", 0xc3),
        ("cache_references", 0xff60),
        ("cache_misses", 0x0964),
    ]
    .into_iter()
    .filter_map(|(target, code)| {
        let origin = events.values().find(|event| event.code == code)?;
        Some(Alias {
            target: target.to_owned(),
            origin: origin.name.clone(),
        })
    })
    .collect()
}

fn tma_metric(name: &str, desc: &str, formula: String, group: &str) -> TmaMetric {
    TmaMetric {
        name: name.to_owned(),
        desc: desc.to_owned(),
        formula,
        group: Some(group.to_owned()),
        cpus: None,
    }
}

fn tma_group(name: &str, events: &[&str]) -> TmaGroup {
    TmaGroup {
        name: name.to_owned(),
        events: events.iter().map(|event| (*event).to_owned()).collect(),
        cpus: None,
    }
}

/// The dispatch-slot top-down of Zen 4 and newer: perf's `PipelineL1` and
/// `PipelineL2` metrics, under the bucket names the other tables use.
///
/// The recorder samples `events` in order, four to a counter group (three
/// when the NMI watchdog holds a counter), and a metric only sees the samples
/// of the group its events were counted in. Every pair a formula combines is
/// therefore adjacent here, at an offset that both group sizes keep together.
fn amd_slots_scenario(
    events: &BTreeMap<String, EventDesc>,
    metric_exprs: &BTreeMap<String, String>,
) -> Option<TmaScenario> {
    let slots = metric_exprs.get("total_dispatch_slots")?;
    let width: u32 = slots.split_once('*')?.0.trim().parse().ok()?;
    let resync = [
        "bp_redirects.resync",
        "bp_fe_redir.resync",
        "resyncs_or_nc_redirects",
    ]
    .into_iter()
    .find(|event| events.contains_key(*event))?;
    let starved = masked_event_name(
        "de_no_dispatch_per_slot.no_ops_from_frontend",
        u64::from(width),
    );
    let ordered = [
        "de_src_op_disp.all",
        "ex_ret_ops",
        "ex_ret_ucode_ops",
        "de_no_dispatch_per_slot.backend_stalls",
        "ex_ret_brn_misp",
        resync,
        "ex_no_retire.load_not_complete",
        "ex_no_retire.not_complete",
        "de_no_dispatch_per_slot.smt_contention",
        "de_no_dispatch_per_slot.no_ops_from_frontend",
        &starved,
    ];
    if !ordered.iter().all(|event| events.contains_key(*event)) {
        return None;
    }

    let slots = "($dispatch_width * cycles)";
    let per_slot = |event: &str| format!("{event} / {slots}");
    Some(TmaScenario {
        name: "tma".to_owned(),
        events: ["cycles", "instructions"]
            .into_iter()
            .chain(ordered)
            .map(str::to_owned)
            .collect(),
        groups: vec![
            tma_group("retiring", &["cycles", "ex_ret_ops"]),
            tma_group(
                "bad_speculation",
                &["cycles", "de_src_op_disp.all", "ex_ret_ops"],
            ),
            tma_group(
                "fe_bound",
                &["cycles", "de_no_dispatch_per_slot.no_ops_from_frontend"],
            ),
            tma_group(
                "be_bound",
                &["cycles", "de_no_dispatch_per_slot.backend_stalls"],
            ),
            tma_group(
                "smt_contention",
                &["cycles", "de_no_dispatch_per_slot.smt_contention"],
            ),
            tma_group("microcode", &["ex_ret_ucode_ops", "ex_ret_ops"]),
            tma_group("flushes", &["ex_ret_brn_misp", resync]),
            tma_group("fetch_latency", &["cycles", &starved]),
            tma_group(
                "no_retire",
                &["ex_no_retire.load_not_complete", "ex_no_retire.not_complete"],
            ),
        ],
        precise_attribution: false,
        constants: vec![TmaConstant {
            name: "dispatch_width".to_owned(),
            value: width,
        }],
        metrics: vec![
            tma_metric(
                "retiring",
                "Dispatch slots used by ops that retired",
                per_slot("ex_ret_ops"),
                "retiring",
            ),
            tma_metric(
                "bad_speculation",
                "Dispatch slots used by ops that did not retire",
                format!("(de_src_op_disp.all - ex_ret_ops) / {slots}"),
                "bad_speculation",
            ),
            tma_metric(
                "fe_bound",
                "Dispatch slots left empty because the frontend supplied no ops",
                per_slot("de_no_dispatch_per_slot.no_ops_from_frontend"),
                "fe_bound",
            ),
            tma_metric(
                "be_bound",
                "Dispatch slots left empty because the backend stalled",
                per_slot("de_no_dispatch_per_slot.backend_stalls"),
                "be_bound",
            ),
            tma_metric(
                "smt_contention",
                "Dispatch slots given to the sibling hardware thread",
                per_slot("de_no_dispatch_per_slot.smt_contention"),
                "smt_contention",
            ),
            tma_metric(
                "retiring.microcode",
                "Retiring slots used by microcoded ops",
                "retiring * ex_ret_ucode_ops / ex_ret_ops".to_owned(),
                "microcode",
            ),
            tma_metric(
                "retiring.fastpath",
                "Retiring slots used by fastpath ops",
                "retiring * (1 - ex_ret_ucode_ops / ex_ret_ops)".to_owned(),
                "microcode",
            ),
            tma_metric(
                "bad_speculation.branch_mispredict",
                "Bad speculation flushed by mispredicted branches",
                format!("bad_speculation * ex_ret_brn_misp / (ex_ret_brn_misp + {resync})"),
                "flushes",
            ),
            tma_metric(
                "bad_speculation.pipeline_restarts",
                "Bad speculation flushed by pipeline restarts (resyncs)",
                format!("bad_speculation * {resync} / (ex_ret_brn_misp + {resync})"),
                "flushes",
            ),
            tma_metric(
                "fe_bound.fetch_latency",
                "Frontend slots lost in cycles that delivered no ops at all (cache and TLB misses, resteers)",
                format!("{starved} / cycles"),
                "fetch_latency",
            ),
            tma_metric(
                "fe_bound.fetch_bandwidth",
                "Frontend slots lost in cycles that delivered some ops, but fewer than the dispatch width",
                "fe_bound - fe_bound.fetch_latency".to_owned(),
                "fetch_latency",
            ),
            tma_metric(
                "be_bound.memory_bound",
                "Backend stalls while the oldest op waited on a load",
                "be_bound * ex_no_retire.load_not_complete / ex_no_retire.not_complete"
                    .to_owned(),
                "no_retire",
            ),
            tma_metric(
                "be_bound.core_bound",
                "Backend stalls while the oldest op waited on something other than a load",
                "be_bound * (1 - ex_no_retire.load_not_complete / ex_no_retire.not_complete)"
                    .to_owned(),
                "no_retire",
            ),
        ],
        ui: None,
    })
}

/// Zen 1 to 3 have no dispatch-slot events, so their level one is fractions
/// of cycles from the fetch-stall counters. Those saturate rather than
/// partition, and the buckets can overlap. Event order matters for the same
/// reason as in [`amd_slots_scenario`].
fn amd_stall_scenario(events: &BTreeMap<String, EventDesc>) -> Option<TmaScenario> {
    let retired = ["ex_ret_ops", "ex_ret_cops"]
        .into_iter()
        .find(|event| events.contains_key(*event))?;
    let ordered = [
        "ic_fetch_stall.ic_stall_back_pressure",
        "l2_fill_pending.l2_fill_busy",
        retired,
        "ic_fetch_stall.ic_stall_dq_empty",
        "ex_ret_brn_misp",
    ];
    if !ordered.iter().all(|event| events.contains_key(*event)) {
        return None;
    }
    Some(TmaScenario {
        name: "tma".to_owned(),
        events: ["cycles", "instructions"]
            .into_iter()
            .chain(ordered)
            .map(str::to_owned)
            .collect(),
        groups: vec![
            tma_group("retiring", &["cycles", retired]),
            tma_group("fe_bound", &["cycles", "ic_fetch_stall.ic_stall_dq_empty"]),
            tma_group("bad_speculation", &["cycles", "ex_ret_brn_misp"]),
            tma_group(
                "be_bound",
                &[
                    "cycles",
                    "ic_fetch_stall.ic_stall_back_pressure",
                    "l2_fill_pending.l2_fill_busy",
                ],
            ),
        ],
        precise_attribution: false,
        constants: vec![
            TmaConstant {
                name: "max_retired_width".to_owned(),
                value: 8,
            },
            TmaConstant {
                name: "branch_mispredict_penalty".to_owned(),
                value: 13,
            },
        ],
        metrics: vec![
            tma_metric(
                "retiring",
                "Fraction of cycles useful work completed",
                format!("{retired} / ($max_retired_width * cycles)"),
                "retiring",
            ),
            tma_metric(
                "fe_bound",
                "Fraction of cycles Fetch/Decode not supplied",
                "ic_fetch_stall.ic_stall_dq_empty / cycles".to_owned(),
                "fe_bound",
            ),
            tma_metric(
                "bad_speculation",
                "Fraction of cycles lost to branch misprediction",
                "(ex_ret_brn_misp * $branch_mispredict_penalty) / cycles".to_owned(),
                "bad_speculation",
            ),
            tma_metric(
                "be_bound",
                "Fraction of cycles backend was out of resources",
                "ic_fetch_stall.ic_stall_back_pressure / cycles".to_owned(),
                "be_bound",
            ),
            tma_metric(
                "be_bound.memory_bound",
                "Backend pressure coincident with outstanding L2 fills",
                "l2_fill_pending.l2_fill_busy / cycles".to_owned(),
                "be_bound",
            ),
            tma_metric(
                "be_bound.core_bound",
                "Backend pressure not coincident with outstanding L2 fills",
                "(ic_fetch_stall.ic_stall_back_pressure - l2_fill_pending.l2_fill_busy) / cycles"
                    .to_owned(),
                "be_bound",
            ),
        ],
        ui: None,
    })
}

/// Converts an Arm Telemetry Solution PMU JSON file.
pub fn import_arm_telemetry(
    source: &Path,
    family_id: &str,
    name: &str,
) -> ImportResult<PlatformDesc> {
    let document: Value = serde_json::from_reader(BufReader::new(File::open(source)?))?;
    let event_values = document
        .get("events")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{} does not contain an events object", source.display()))?;

    let mut events = BTreeMap::<String, EventDesc>::new();
    for (mnemonic, value) in event_values {
        if !arm_event_is_pmu_accessible(value)? {
            continue;
        }
        let code = value
            .get("code")
            .ok_or_else(|| format!("Arm event {mnemonic} is missing its code"))?;
        let code = parse_json_number(code, "code")?;
        let desc = string_field(value, "description")
            .or_else(|| string_field(value, "title"))
            .unwrap_or("");
        events.insert(
            mnemonic.clone(),
            EventDesc {
                name: mnemonic.clone(),
                desc: desc.to_owned(),
                code,
            },
        );
    }

    let aliases = arm_aliases(&events);
    Ok(PlatformDesc {
        family_id: family_id.to_owned(),
        name: name.to_owned(),
        vendor: "ARM".to_owned(),
        arch: "aarch64".to_owned(),
        max_counters: None,
        leader_event: None,
        events: events.into_values().collect(),
        aliases: Some(aliases),
        metrics: Vec::new(),
        scenarios: None,
    })
}

fn json_sources(source: &Path) -> ImportResult<Vec<PathBuf>> {
    if source.is_file() {
        if source.extension() != Some(OsStr::new("json")) {
            return Err(format!("{} is not a JSON file", source.display()).into());
        }
        return Ok(vec![source.to_owned()]);
    }

    let mut files: Vec<PathBuf> = fs::read_dir(source)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .filter(|path| {
            !path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .starts_with("uncore-")
        })
        .collect();
    files.sort();
    Ok(files)
}

fn intel_records(document: Value, path: &Path) -> ImportResult<Vec<Value>> {
    match document {
        Value::Array(records) => Ok(records),
        Value::Object(mut object) => object
            .remove("Events")
            .or_else(|| object.remove("events"))
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                format!(
                    "{} must be an event array or contain an Events array",
                    path.display()
                )
                .into()
            }),
        _ => Err(format!("{} must contain a JSON event array", path.display()).into()),
    }
}

fn arm_event_is_pmu_accessible(value: &Value) -> ImportResult<bool> {
    let Some(accesses) = value.get("accesses") else {
        return Ok(true);
    };
    let accesses = accesses
        .as_array()
        .ok_or("Arm event accesses must be an array")?;
    Ok(accesses.iter().any(|access| access.as_str() == Some("PMU")))
}

pub fn convert_event(value: &Value) -> ImportResult<Option<EventDesc>> {
    let Some(name) = string_field(value, "EventName") else {
        return Ok(None);
    };
    let Some(event_code) = string_field(value, "EventCode") else {
        return Ok(None);
    };
    if value.get("MSRIndex").is_some() || value.get("MSRValue").is_some() {
        return Ok(None);
    }

    let mut code = parse_number(event_code)?;
    code |= parse_optional(value, "UMask")? << 8;
    code |= parse_optional(value, "EdgeDetect")? << 18;
    code |= parse_optional(value, "AnyThread")? << 21;
    code |= parse_optional(value, "Invert")? << 23;
    code |= parse_optional(value, "CounterMask")? << 24;

    let desc = string_field(value, "PublicDescription")
        .or_else(|| string_field(value, "BriefDescription"))
        .unwrap_or("")
        .to_owned();
    Ok(Some(EventDesc {
        name: name.to_owned(),
        desc,
        code,
    }))
}

pub fn convert_metric(value: &Value) -> Option<Metric> {
    let name = string_field(value, "MetricName")?;
    let expression = string_field(value, "MetricExpr")?;
    let desc = string_field(value, "PublicDescription")
        .or_else(|| string_field(value, "BriefDescription"))
        .unwrap_or("");
    Some(Metric {
        name: name.to_owned(),
        desc: desc.to_owned(),
        expression: MetricExpression(expression.to_owned()),
        unit: string_field(value, "ScaleUnit").map(str::to_owned),
    })
}

fn intel_aliases(events: &BTreeMap<String, EventDesc>) -> Vec<Alias> {
    const ALIASES: &[(&str, &[&str])] = &[
        (
            "cycles",
            &["CPU_CLK_UNHALTED.THREAD_P", "CPU_CLK_UNHALTED.THREAD"],
        ),
        ("instructions", &["INST_RETIRED.ANY_P", "INST_RETIRED.ANY"]),
        ("branches", &["BR_INST_RETIRED.ALL_BRANCHES"]),
        ("branch_misses", &["BR_MISP_RETIRED.ALL_BRANCHES"]),
        ("cache_references", &["LONGEST_LAT_CACHE.REFERENCE"]),
        ("cache_misses", &["LONGEST_LAT_CACHE.MISS"]),
    ];

    ALIASES
        .iter()
        .filter_map(|(target, origins)| {
            origins
                .iter()
                .find(|origin| events.contains_key(**origin))
                .map(|origin| Alias {
                    target: (*target).to_owned(),
                    origin: (*origin).to_owned(),
                })
        })
        .collect()
}

fn arm_aliases(events: &BTreeMap<String, EventDesc>) -> Vec<Alias> {
    [
        ("cycles", "CPU_CYCLES"),
        ("instructions", "INST_RETIRED"),
        ("branches", "BR_RETIRED"),
        ("branch_misses", "BR_MIS_PRED_RETIRED"),
        ("cache_references", "LL_CACHE_RD"),
        ("cache_misses", "LL_CACHE_MISS_RD"),
        ("stalled_cycles_frontend", "STALL_FRONTEND"),
        ("stalled_cycles_backend", "STALL_BACKEND"),
    ]
    .into_iter()
    .filter(|(_, origin)| events.contains_key(*origin))
    .map(|(target, origin)| Alias {
        target: target.to_owned(),
        origin: origin.to_owned(),
    })
    .collect()
}

fn string_field<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn parse_optional(value: &Value, name: &str) -> ImportResult<u64> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(raw)) if raw.is_empty() => Ok(0),
        Some(Value::String(raw)) => parse_number(raw),
        Some(Value::Number(raw)) => raw
            .as_u64()
            .ok_or_else(|| format!("{name} is not an unsigned integer").into()),
        Some(other) => Err(format!("unsupported {name} value: {other}").into()),
    }
}

fn parse_json_number(value: &Value, name: &str) -> ImportResult<u64> {
    match value {
        Value::String(raw) => parse_number(raw),
        Value::Number(raw) => raw
            .as_u64()
            .ok_or_else(|| format!("{name} is not an unsigned integer").into()),
        other => Err(format!("unsupported {name} value: {other}").into()),
    }
}

fn parse_number(raw: &str) -> ImportResult<u64> {
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(raw.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_edge_invert_and_counter_mask_fields() {
        let value = serde_json::json!({
            "EventName": "TEST.EVENT",
            "EventCode": "0x48",
            "UMask": "0x2",
            "CounterMask": "1",
            "EdgeDetect": "1",
            "Invert": "1"
        });
        assert_eq!(convert_event(&value).unwrap().unwrap().code, 0x0184_0248);
    }

    #[test]
    fn imports_perf_metric_definition() {
        let value = serde_json::json!({
            "MetricName": "IPC",
            "MetricExpr": "instructions / cycles",
            "BriefDescription": "Instructions per cycle",
            "ScaleUnit": "1insn/cycle"
        });
        let metric = convert_metric(&value).unwrap();
        assert_eq!(metric.name, "IPC");
        assert_eq!(metric.expression.0, "instructions / cycles");
        assert_eq!(metric.unit.as_deref(), Some("1insn/cycle"));
    }

    #[test]
    fn ignores_fixed_only_and_extra_register_events() {
        let fixed = serde_json::json!({"EventName": "INST_RETIRED.ANY", "UMask": "0x1"});
        let offcore = serde_json::json!({
            "EventName": "OFFCORE_RESPONSE.TEST", "EventCode": "0xb7",
            "MSRIndex": "0x1a6", "MSRValue": "0x1"
        });
        assert!(convert_event(&fixed).unwrap().is_none());
        assert!(convert_event(&offcore).unwrap().is_none());
    }

    #[test]
    fn imports_direct_intel_perfmon_fixture() {
        let source = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/intel-perfmon.json"
        ));
        let platform = import_intel(source, "fixture", "Intel fixture").unwrap();
        assert_eq!(platform.events.len(), 2);
        assert_eq!(platform.events[0].name, "CPU_CLK_UNHALTED.THREAD_P");
        assert_eq!(platform.events[0].code, 0x3c);
        assert_eq!(platform.events[1].code, 0x1c0);
        let aliases = platform.aliases.unwrap();
        assert_eq!(aliases[0].target, "cycles");
        assert_eq!(aliases[1].target, "instructions");
    }

    #[test]
    fn amd_extended_event_select_goes_above_the_unit_mask() {
        // PPR: PMCx1A0 with unit mask 0x1e is PERF_CTL 0x1_0000_1EA0.
        let value = serde_json::json!({
            "EventName": "de_no_dispatch_per_slot.backend_stalls",
            "EventCode": "0x1a0",
            "UMask": "0x1e"
        });
        assert_eq!(
            convert_amd_event(&value).unwrap().unwrap().code,
            0x1_0000_1ea0
        );
    }

    #[test]
    fn imports_amd_fixture_with_slot_topdown() {
        let source = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/amd"));
        let platform = import_amd(source, "fixture", "AMD fixture").unwrap();
        let code = |name: &str| {
            platform
                .events
                .iter()
                .find(|event| event.name == name)
                .map(|event| event.code)
        };
        // The L3 event belongs to another PMU and must not reach the core table.
        assert_eq!(code("l3_lookup_state.all_coherent_accesses_to_l3"), None);
        // The masked use in the fixture's metric becomes its own event.
        assert_eq!(
            code("de_no_dispatch_per_slot.no_ops_from_frontend_cmask8"),
            Some(0x1_0800_01a0)
        );
        let aliases = platform.aliases.unwrap();
        assert!(aliases
            .iter()
            .any(|alias| alias.target == "cycles" && alias.origin == "ls_not_halted_cyc"));

        let scenario = &platform.scenarios.unwrap()[0];
        assert_eq!(scenario.constants[0].value, 8);
        let formula = |name: &str| {
            &scenario
                .metrics
                .iter()
                .find(|metric| metric.name == name)
                .unwrap()
                .formula
        };
        assert_eq!(
            formula("fe_bound.fetch_latency"),
            "de_no_dispatch_per_slot.no_ops_from_frontend_cmask8 / cycles"
        );
        assert!(formula("bad_speculation.pipeline_restarts").contains("bp_redirects.resync"));
    }

    #[test]
    fn imports_arm_telemetry_fixture() {
        let source = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/arm-telemetry.json"
        ));
        let platform = import_arm_telemetry(source, "fixture", "Arm fixture").unwrap();
        assert_eq!(platform.vendor, "ARM");
        assert_eq!(platform.arch, "aarch64");
        assert_eq!(platform.events.len(), 3);
        assert_eq!(platform.events[0].name, "BR_RETIRED");
        assert_eq!(platform.events[0].code, 0x21);
        assert_eq!(platform.events[1].desc, "Counts processor cycles.");
        assert_eq!(platform.aliases.unwrap().len(), 3);
    }
}
