mod assembly;
mod mem_samples;
mod memory;
mod roofline;
mod samples;
mod snapshot;
mod tables;
mod tma;
mod trace;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use mperf_data::{EventType, RecordInfo, Scenario};
use tokio::fs;

use tables::{Columns, Tables, quote_identifier};

/// Turn a recording into the derived tables every consumer reads: one Parquet
/// file per table in the session directory.
pub async fn perform_postprocessing(res_dir: &Path, pb: kdam::Bar) -> Result<()> {
    let mut pb = pb;

    let data = fs::read_to_string(res_dir.join("info.json"))
        .await
        .expect("failed to read info.json");
    let info: RecordInfo = serde_json::from_str(&data).expect("failed to parse info.json");

    let tables = Tables::open(res_dir)?;
    samples::process(&tables, &info, res_dir, &mut pb).await?;
    snapshot::write_host_telemetry(&tables, res_dir)?;
    snapshot::write_collectors(&tables, &info)?;

    match info.scenario {
        Scenario::Snapshot => {
            snapshot::process(&tables, &info, res_dir)?;
            assembly::process(&tables, &mut pb)?;
            write_hotspots(&tables)?;
        }
        Scenario::Mem => {
            memory::process(&tables, &info, res_dir)?;
            mem_samples::process(&tables, res_dir)?;
            assembly::process(&tables, &mut pb)?;
            write_hotspots(&tables)?;
        }
        Scenario::Roofline => {
            roofline::process(&tables, &info, res_dir)?;
            if res_dir.join("qemu-roofline.memory.json").exists() {
                memory::process(&tables, &info, res_dir)?;
            }
            assembly::process(&tables, &mut pb)?;
            write_hotspots(&tables)?;
            roofline::write_chart(&tables)?;
        }
        Scenario::TMA => {
            mem_samples::process(&tables, res_dir)?;
            assembly::process(&tables, &mut pb)?;
            tma::process(&tables, &info.scenario_info)?;
        }
    }

    write_capture_fidelity(&tables, &info)?;
    trace::write_custom_events(&tables)?;
    write_derived_metrics(&tables)?;
    samples::remove_raw_segments(&tables, res_dir)?;

    Ok(())
}

/// The `pmu_counters` column a scenario counter is stored in.
pub(crate) fn event_column_name(event: &(EventType, String)) -> String {
    match event.0 {
        EventType::PmuCustom => format!("pmu_{}", event.1.replace('.', "_")),
        _ => event.0.to_string(),
    }
}

/// The `pmu_counters` column a recorded sample's interned event name maps to.
pub(crate) fn event_column_name_for(name: &str) -> String {
    match crate::utils::event_type_from_name(name) {
        Some(ty) => ty.to_string(),
        None => format!("pmu_{}", name.replace('.', "_")),
    }
}

/// One row per rung considered for this recording: the chosen capture strategy
/// and every better one, with the reason it was unavailable.
fn write_capture_fidelity(tables: &Tables, info: &RecordInfo) -> Result<()> {
    let mut scenario = Vec::new();
    let mut rung = Vec::new();
    let mut status = Vec::new();
    let mut reason = Vec::new();
    for fidelity in &info.capture_fidelity {
        for rejected in &fidelity.rejected {
            scenario.push(fidelity.scenario.clone());
            rung.push(rejected.rung.clone());
            status.push("rejected".to_string());
            reason.push(rejected.reason.clone());
        }
        scenario.push(fidelity.scenario.clone());
        rung.push(fidelity.rung.clone());
        status.push("chosen".to_string());
        reason.push(String::new());
    }

    let mut columns = Columns::default();
    columns.text("scenario", scenario);
    columns.text("rung", rung);
    columns.text("status", status);
    columns.text("reason", reason);
    tables.write("capture_fidelity", columns.finish()?)
}

fn write_hotspots(tables: &Tables) -> Result<()> {
    let available = tables.columns("pmu_counters");
    let counter = |name: &str| {
        if available.iter().any(|column| column == name) {
            format!("pmu_counters.{}", quote_identifier(name))
        } else {
            "CAST(NULL AS BIGINT)".to_owned()
        }
    };
    let total = |name: &str| {
        if available.iter().any(|column| column == name) {
            format!("(SELECT SUM({}) FROM pmu_counters)", quote_identifier(name))
        } else {
            "CAST(NULL AS BIGINT)".to_owned()
        }
    };
    let cycles = counter("pmu_cycles");
    let instructions = counter("pmu_instructions");
    let branch_misses = counter("pmu_branch_misses");
    let branch_instructions = counter("pmu_branch_instructions");
    let llc_misses = counter("pmu_llc_misses");
    let llc_references = counter("pmu_llc_references");

    tables.write_query(
        "hotspots",
        &format!(
            "SELECT
                proc_map.func_name AS func_name,
                SUM({cycles}) * 1.0 / {} AS total,
                CAST(SUM({cycles}) AS BIGINT) AS cycles,
                CAST(SUM({instructions}) AS BIGINT) AS instructions,
                SUM({instructions}) * 1.0 / SUM({cycles}) AS ipc,
                SUM({branch_misses} * 1.0 / pmu_counters.confidence) * 1.0
                    / SUM({branch_instructions} * 1.0 / pmu_counters.confidence) AS branch_miss_rate,
                SUM({branch_misses} * 1.0 / pmu_counters.confidence) * 1.0
                    / SUM({instructions}) * 1000 AS branch_mpki,
                SUM({llc_misses} * 1.0 / pmu_counters.confidence) * 1.0
                    / (SUM({llc_misses} * 1.0 / pmu_counters.confidence)
                       + SUM({llc_references} * 1.0 / pmu_counters.confidence)) AS cache_miss_rate,
                SUM({llc_misses} * 1.0 / pmu_counters.confidence) * 1.0
                    / SUM({instructions}) * 1000 AS cache_mpki
             FROM pmu_counters
             INNER JOIN proc_map ON pmu_counters.ip = proc_map.ip
             GROUP BY proc_map.func_name",
            total("pmu_cycles")
        ),
    )
}

fn write_derived_metrics(tables: &Tables) -> Result<()> {
    write_metric_definitions(tables, &libprof::host_metrics())
}

fn write_metric_definitions(tables: &Tables, metrics: &[libprof::Metric]) -> Result<()> {
    let available = tables.columns("pmu_counters");
    let mut rows = Vec::<(String, f64, Option<String>, String)>::new();
    for metric in metrics {
        let Ok(event_names) = metric.expression.event_names() else {
            continue;
        };
        let mut values = HashMap::new();
        let mut applicable = true;
        for event_name in event_names {
            let Some(column) = metric_event_column(&event_name) else {
                applicable = false;
                break;
            };
            if !available.iter().any(|name| name == column) {
                applicable = false;
                break;
            }
            let value = tables
                .scalar_f64(&format!(
                    "SELECT CAST(SUM({}) AS DOUBLE) FROM pmu_counters",
                    quote_identifier(column)
                ))
                .unwrap_or(0.0);
            values.insert(event_name, value);
        }
        if !applicable {
            continue;
        }
        let Ok(value) = metric.expression.evaluate(&values) else {
            continue;
        };
        rows.retain(|row| row.0 != metric.name);
        rows.push((
            metric.name.clone(),
            value,
            metric.unit.clone(),
            metric.expression.0.clone(),
        ));
    }

    let mut columns = Columns::default();
    columns.text("name", rows.iter().map(|row| row.0.clone()).collect());
    columns.f64("value", rows.iter().map(|row| row.1).collect());
    columns.text_opt("unit", rows.iter().map(|row| row.2.clone()).collect());
    columns.text("expression", rows.iter().map(|row| row.3.clone()).collect());
    tables.write("derived_metrics", columns.finish()?)
}

fn metric_event_column(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("cycles") {
        Some("pmu_cycles")
    } else if name.eq_ignore_ascii_case("instructions") {
        Some("pmu_instructions")
    } else if name.eq_ignore_ascii_case("branches") {
        Some("pmu_branch_instructions")
    } else if name.eq_ignore_ascii_case("branch_misses") {
        Some("pmu_branch_misses")
    } else if name.eq_ignore_ascii_case("llc_references")
        || name.eq_ignore_ascii_case("cache_references")
    {
        Some("pmu_llc_references")
    } else if name.eq_ignore_ascii_case("llc_misses") || name.eq_ignore_ascii_case("cache_misses") {
        Some("pmu_llc_misses")
    } else if name.eq_ignore_ascii_case("stalled_cycles_frontend") {
        Some("pmu_stalled_cycles_frontend")
    } else if name.eq_ignore_ascii_case("stalled_cycles_backend") {
        Some("pmu_stalled_cycles_backend")
    } else {
        None
    }
}
