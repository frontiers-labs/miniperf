use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use mperf_data::{RecordInfo, ScenarioInfo};
use num_format::Locale;
use num_format::ToFormattedString;
use parking_lot::{Mutex, RwLock};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Style, Stylize},
    widgets::{Block, Paragraph, Row, Table, Widget, Wrap},
};
use store::Session;

#[derive(Clone)]
pub struct SummaryTab {
    record_info: RecordInfo,
    session: Arc<Mutex<Session>>,
    stat: Arc<RwLock<Stat>>,
    load_started: Arc<AtomicBool>,
    load_error: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stat {
    cycles: u64,
    instructions: u64,
    branch_instructions: Option<u64>,
    branch_misses: Option<u64>,
    cache_references: Option<u64>,
    cache_misses: Option<u64>,
    stalled_cycles_frontend: Option<u64>,
    stalled_cycles_backend: Option<u64>,
    initialized: bool,
}

impl SummaryTab {
    pub fn new(record_info: RecordInfo, session: Arc<Mutex<Session>>) -> Self {
        SummaryTab {
            record_info,
            session,
            stat: Arc::new(RwLock::new(Stat::default())),
            load_started: Arc::new(AtomicBool::new(false)),
            load_error: Arc::new(RwLock::new(None)),
        }
    }

    pub fn run(&self) {
        if self
            .load_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(this.fetch_data());
    }

    async fn fetch_data(self) {
        let session = self.session.lock();
        let result: Result<Stat, String> = (|| {
            if !session.has_table("pmu_counters") {
                return Err("this recording has no pmu_counters table".to_string());
            }
            let conn = session.connection();
            let mut columns_statement = conn
                .prepare("PRAGMA table_info('pmu_counters')")
                .map_err(|error| error.to_string())?;
            let available_columns: HashSet<String> = columns_statement
                .query_map([], |row| row.get::<_, String>("name"))
                .map_err(|error| error.to_string())?
                .collect::<store::duckdb::Result<_>>()
                .map_err(|error| error.to_string())?;

            let has_branch = available_columns.contains("pmu_branch_instructions")
                && available_columns.contains("pmu_branch_misses");
            let has_cache = available_columns.contains("pmu_llc_references")
                && available_columns.contains("pmu_llc_misses");
            let has_stalled = available_columns.contains("pmu_stalled_cycles_frontend")
                && available_columns.contains("pmu_stalled_cycles_backend");

            let scaled = |counter: &str| {
                format!("CAST(SUM({counter} * 1.0 / confidence) AS BIGINT) AS {counter}")
            };
            let missing = |counter: &str| format!("CAST(0 AS BIGINT) AS {counter}");

            let mut select_parts = vec![
                "CAST(SUM(pmu_cycles) AS BIGINT) AS pmu_cycles".to_string(),
                "CAST(SUM(pmu_instructions) AS BIGINT) AS pmu_instructions".to_string(),
            ];

            for (present, counters) in [
                (has_branch, ["pmu_branch_instructions", "pmu_branch_misses"]),
                (has_cache, ["pmu_llc_references", "pmu_llc_misses"]),
                (
                    has_stalled,
                    ["pmu_stalled_cycles_frontend", "pmu_stalled_cycles_backend"],
                ),
            ] {
                for counter in counters {
                    select_parts.push(if present {
                        scaled(counter)
                    } else {
                        missing(counter)
                    });
                }
            }

            let query = format!("SELECT {} FROM pmu_counters", select_parts.join(",\n"));
            let mut statement = conn.prepare(&query).map_err(|error| error.to_string())?;
            let mut rows = statement.query([]).map_err(|error| error.to_string())?;
            let row = rows
                .next()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "summary query returned no rows".to_string())?;

            let read = |name| {
                row.get::<_, Option<i64>>(name)
                    .map(|value| value.unwrap_or_default() as u64)
                    .map_err(|error| error.to_string())
            };
            Ok(Stat {
                cycles: read("pmu_cycles")?,
                instructions: read("pmu_instructions")?,
                branch_instructions: has_branch
                    .then(|| read("pmu_branch_instructions"))
                    .transpose()?,
                branch_misses: has_branch.then(|| read("pmu_branch_misses")).transpose()?,
                cache_references: has_cache.then(|| read("pmu_llc_references")).transpose()?,
                cache_misses: has_cache.then(|| read("pmu_llc_misses")).transpose()?,
                stalled_cycles_frontend: has_stalled
                    .then(|| read("pmu_stalled_cycles_frontend"))
                    .transpose()?,
                stalled_cycles_backend: has_stalled
                    .then(|| read("pmu_stalled_cycles_backend"))
                    .transpose()?,
                initialized: true,
            })
        })();
        drop(session);

        match result {
            Ok(stat) => *self.stat.write() = stat,
            Err(error) => {
                *self.load_error.write() = Some(format!("Could not load summary data:\n\n{error}"));
            }
        }
    }
}

impl Widget for SummaryTab {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        let horizontal = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]);
        let [summary_area, _right_area] = horizontal.areas(area);

        let vertical = Layout::vertical_margin(
            Layout::vertical([Constraint::Fill(3), Constraint::Fill(1)]),
            1,
        );
        let [stat_area, info_area] = vertical.areas(summary_area);

        let block = Block::bordered().title("Counters stats");
        block.render(stat_area, buf);

        let vertical = Layout::horizontal_margin(
            Layout::vertical_margin(Layout::vertical([Constraint::Fill(1)]), 1),
            2,
        );
        let [stat_table_area] = vertical.areas(stat_area);

        {
            let stat = self.stat.read();

            if let Some(error) = self.load_error.read().clone() {
                Paragraph::new(error)
                    .wrap(Wrap { trim: true })
                    .render(stat_table_area, buf);
            } else if !stat.initialized {
                let counter = 0;
                let pb = ratatui::widgets::Gauge::default()
                    .block(Block::bordered().title("Loading data..."))
                    .gauge_style(Style::new().white().on_black().italic())
                    .percent(counter);
                pb.render(stat_table_area, buf);
            } else {
                let ipc = if stat.cycles > 0 {
                    format!("{:.2}", stat.instructions as f64 / stat.cycles as f64)
                } else {
                    "N/A".to_string()
                };

                let branch_instruction_count = format_optional_count(stat.branch_instructions);
                let branch_per_cycle = match (stat.branch_instructions, stat.cycles) {
                    (Some(branch_instr), cycles) if cycles > 0 => {
                        format!("{:.2} per cycle", branch_instr as f64 / cycles as f64)
                    }
                    _ => "N/A".to_string(),
                };

                let branch_miss_count = format_optional_count(stat.branch_misses);
                let branch_miss_pct = match (stat.branch_misses, stat.branch_instructions) {
                    (Some(misses), Some(instructions)) if instructions > 0 => {
                        format!("{:.2}%", misses as f64 / instructions as f64 * 100_f64)
                    }
                    _ => "N/A".to_string(),
                };

                let branch_mpki = match (stat.branch_misses, stat.instructions) {
                    (Some(misses), instructions) if instructions > 0 => {
                        format!("{:.2}", misses as f64 / instructions as f64 * 1000.0)
                    }
                    _ => "N/A".to_string(),
                };

                let cache_ref_count = format_optional_count(stat.cache_references);
                let cache_miss_count = format_optional_count(stat.cache_misses);
                let cache_miss_pct = match (stat.cache_misses, stat.cache_references) {
                    (Some(misses), Some(references)) if misses + references > 0 => {
                        format!(
                            "{:.2}%",
                            misses as f64 / (misses + references) as f64 * 100_f64
                        )
                    }
                    _ => "N/A".to_string(),
                };
                let cache_mpki = match (stat.cache_misses, stat.instructions) {
                    (Some(misses), instructions) if instructions > 0 => {
                        format!("{:.2}", misses as f64 / instructions as f64 * 1000.0)
                    }
                    _ => "N/A".to_string(),
                };
                let stalled_backend_count = format_optional_count(stat.stalled_cycles_backend);
                let stalled_backend_pct =
                    format_optional_ratio(stat.stalled_cycles_backend, stat.cycles);
                let stalled_frontend_count = format_optional_count(stat.stalled_cycles_frontend);
                let stalled_frontend_pct =
                    format_optional_ratio(stat.stalled_cycles_frontend, stat.cycles);

                let rows = [
                    Row::new([
                        "Cycles".to_string(),
                        stat.cycles.to_formatted_string(&Locale::en),
                        "".to_string(),
                    ]),
                    Row::new([
                        "Instructions".to_string(),
                        stat.instructions.to_formatted_string(&Locale::en),
                        "".to_string(),
                    ]),
                    Row::new(["IPC".to_string(), ipc, "".to_string()]),
                    Row::new([
                        "Branch instructions".to_string(),
                        branch_instruction_count,
                        branch_per_cycle,
                    ]),
                    Row::new([
                        "Branch misses".to_string(),
                        branch_miss_count,
                        branch_miss_pct,
                    ]),
                    Row::new(["Branch MPKI".to_string(), branch_mpki, "".to_string()]),
                    Row::new([
                        "Last level cache references".to_string(),
                        cache_ref_count,
                        "".to_string(),
                    ]),
                    Row::new([
                        "Last level cache misses".to_string(),
                        cache_miss_count,
                        cache_miss_pct,
                    ]),
                    Row::new(["Cache MPKI".to_string(), cache_mpki, "".to_string()]),
                    Row::new([
                        "Stalled cycles backend".to_string(),
                        stalled_backend_count,
                        stalled_backend_pct,
                    ]),
                    Row::new([
                        "Stalled cycles frontend".to_string(),
                        stalled_frontend_count,
                        stalled_frontend_pct,
                    ]),
                ];
                let widths = [
                    Constraint::Percentage(60),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                ];
                let stat_table = Table::new(rows, widths).column_spacing(1);
                stat_table.render(stat_table_area, buf);
            }
        }

        let block = Block::bordered().title("Result info");
        block.render(info_area, buf);

        let roofline_method = match &self.record_info.scenario_info {
            ScenarioInfo::Roofline(info) => info.method.as_deref().cloned(),
            _ => None,
        };
        let command = self
            .record_info
            .command
            .unwrap_or(vec!["".to_string()])
            .join(" ");

        let mut rows = vec![
            Row::new([
                "Scenario".to_string(),
                self.record_info.scenario.name().to_string(),
            ]),
            Row::new(["Command".to_string(), command]),
            Row::new(["CPU family".to_string(), self.record_info.cpu_model.clone()]),
            Row::new([
                "CPU vendor".to_string(),
                self.record_info.cpu_vendor.clone(),
            ]),
        ];
        if let Some(method) = roofline_method {
            rows.push(Row::new([
                "Roofline method".to_string(),
                format!(
                    "{} accounting · {} performance · {}",
                    method.accounting, method.performance, method.quality
                ),
            ]));
            rows.push(Row::new(["Method reason".to_string(), method.reason]));
            if !method.warnings.is_empty() {
                rows.push(Row::new([
                    "Method warnings".to_string(),
                    method.warnings.join("; "),
                ]));
            }
        }
        if let Some(calibration) = &self.record_info.cpu_info.roofline_calibration {
            rows.push(Row::new([
                "Roof ceilings".to_string(),
                format!(
                    "{:.2} GFLOP/s · {:.2} GB/s · {} Rayon threads",
                    calibration.fp64_gflops,
                    calibration.memory_gbytes_per_second,
                    calibration.threads
                ),
            ]));
            if let Some(single) = &calibration.single_thread {
                rows.push(Row::new([
                    "Single-thread ceilings".to_string(),
                    format!(
                        "{:.2} GFLOP/s · {:.2} GB/s",
                        single.fp64_gflops, single.memory_gbytes_per_second
                    ),
                ]));
            }
        }
        let widths = [Constraint::Percentage(20), Constraint::Percentage(80)];

        let vertical = Layout::horizontal_margin(
            Layout::vertical_margin(Layout::vertical([Constraint::Fill(1)]), 1),
            2,
        );
        let [info_table_area] = vertical.areas(info_area);

        let info_table = Table::new(rows, widths).column_spacing(1);
        info_table.render(info_table_area, buf);
    }
}

fn format_optional_count(value: Option<u64>) -> String {
    value
        .map(|v| v.to_formatted_string(&Locale::en))
        .unwrap_or_else(|| "N/A".to_string())
}

fn format_optional_ratio(value: Option<u64>, total: u64) -> String {
    match (value, total) {
        (Some(value), total) if total > 0 => format!("{:.2}%", value as f64 / total as f64 * 100.0),
        _ => "N/A".to_string(),
    }
}
