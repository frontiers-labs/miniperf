use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crossterm::event::KeyCode;
use num_format::{Locale, ToFormattedString};
use parking_lot::{Mutex, RwLock};
use pmu_data::{MetricColumnSpec, MetricsTableSpec, SortDirection, ValueFormat};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Widget},
};
use store::{
    Session,
    duckdb::{Row as SqlRow, params},
};

#[derive(Clone)]
pub struct MetricsTableTab {
    rows: Arc<RwLock<Vec<MetricsRow>>>,
    is_running: Arc<RwLock<bool>>,
    session: Arc<Mutex<Session>>,
    state: Arc<Mutex<MetricsState>>,
    config: Arc<MetricsTableConfig>,
    layout: Arc<RwLock<Option<RuntimeLayout>>>,
}

#[derive(Clone)]
struct MetricsTableConfig {
    title: String,
    view: String,
    columns: Vec<ColumnConfig>,
    sticky_override: Option<usize>,
    order_by: Option<OrderClause>,
    limit: Option<usize>,
    function_column: Option<String>,
    enable_assembly: bool,
}

#[derive(Clone)]
struct RuntimeLayout {
    columns: Vec<ColumnConfig>,
    sticky_columns: usize,
    function_column_index: Option<usize>,
}

#[derive(Clone)]
struct ColumnConfig {
    key: String,
    label: String,
    format: ValueFormat,
    width: Option<u16>,
    sticky: bool,
    optional: bool,
    alignment: Alignment,
}

#[derive(Clone)]
struct OrderClause {
    column: String,
    direction: SortDirection,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum MetricsFocus {
    #[default]
    List,
    Assembly,
}

#[derive(Default)]
struct MetricsState {
    selected: Option<usize>,
    offset: usize,
    column_offset: usize,
    focus: MetricsFocus,
    assembly_loading: bool,
    assembly_error: Option<String>,
    assembly: Option<AssemblyViewState>,
    assembly_request_id: u64,
    assembly_summary: Option<Vec<(String, String)>>,
    table_error: Option<String>,
}

#[derive(Clone)]
struct MetricsRow {
    values: Vec<MetricValue>,
}

#[derive(Clone)]
enum MetricValue {
    Text(String),
    Integer(i64),
    Float(f64),
    Null,
}

#[derive(Clone)]
struct AssemblyRow {
    address: u64,
    instruction: String,
    samples: u64,
    share: f64,
    cycles: u64,
    instructions: u64,
    branch_misses: u64,
    branch_instructions: u64,
    llc_misses: u64,
    llc_references: u64,
}

#[derive(Clone, Copy, Default)]
struct AssemblyStats {
    samples: u64,
    cycles: u64,
    instructions: u64,
    branch_misses: u64,
    branch_instructions: u64,
    llc_misses: u64,
    llc_references: u64,
}

impl AssemblyStats {
    fn merge(&mut self, other: Self) {
        self.samples = self.samples.saturating_add(other.samples);
        self.cycles = self.cycles.saturating_add(other.cycles);
        self.instructions = self.instructions.saturating_add(other.instructions);
        self.branch_misses = self.branch_misses.saturating_add(other.branch_misses);
        self.branch_instructions = self
            .branch_instructions
            .saturating_add(other.branch_instructions);
        self.llc_misses = self.llc_misses.saturating_add(other.llc_misses);
        self.llc_references = self.llc_references.saturating_add(other.llc_references);
    }
}

fn assembly_row(
    address: u64,
    instruction: String,
    stats: AssemblyStats,
    total_samples: u64,
) -> AssemblyRow {
    AssemblyRow {
        address,
        instruction,
        samples: stats.samples,
        share: if total_samples > 0 {
            stats.samples as f64 / total_samples as f64
        } else {
            0.0
        },
        cycles: stats.cycles,
        instructions: stats.instructions,
        branch_misses: stats.branch_misses,
        branch_instructions: stats.branch_instructions,
        llc_misses: stats.llc_misses,
        llc_references: stats.llc_references,
    }
}

#[derive(Clone)]
struct AssemblyViewState {
    func_name: String,
    module_path: String,
    symbol: String,
    rows: Vec<AssemblyRow>,
    selected: Option<usize>,
    offset: usize,
    max_samples: u64,
}

impl Widget for MetricsTableTab {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        let rows = self.rows.read();
        let layout_opt = self.layout.read().clone();

        let vertical = Layout::vertical([Constraint::Fill(1)]).vertical_margin(0);
        let horizontal = Layout::horizontal([Constraint::Fill(1)]).horizontal_margin(0);

        let [table_area] = vertical.areas(area);
        let [table_area] = horizontal.areas(table_area);

        let mut state = self.state.lock();

        if let Some(message) = state.table_error.clone() {
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .render(table_area, buf);
            return;
        }

        let Some(layout) = layout_opt else {
            let pb = ratatui::widgets::Gauge::default()
                .block(Block::bordered().title("Loading data..."))
                .gauge_style(Style::new().white().on_black().italic())
                .percent(0);
            pb.render(table_area, buf);
            return;
        };

        let total_rows = rows.len();
        if total_rows == 0 {
            Paragraph::new("No data available")
                .alignment(Alignment::Center)
                .render(table_area, buf);
            return;
        }

        if state.selected.is_none() {
            state.selected = Some(0);
        }
        if let Some(selected) = state.selected
            && selected >= total_rows
        {
            state.selected = Some(total_rows - 1);
        }

        let sticky_columns = layout.sticky_columns.max(1).min(layout.columns.len());
        let metric_columns = layout.columns.len().saturating_sub(sticky_columns);
        let max_metric_offset = metric_columns.saturating_sub(1);
        if state.column_offset > max_metric_offset {
            state.column_offset = max_metric_offset;
        }

        let header = build_header(&layout, sticky_columns, state.column_offset);
        let widths = build_constraints(&layout, sticky_columns, state.column_offset);

        let table_rows = rows
            .iter()
            .map(|row| build_row(row, &layout, sticky_columns, state.column_offset));

        let mut table_state = TableState::default()
            .with_selected(state.selected)
            .with_offset(state.offset);

        let table = Table::new(table_rows, widths)
            .header(header)
            .row_highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("▶ ")
            .block(Block::new().borders(Borders::TOP | Borders::BOTTOM));

        let table_height = table_area.height as usize;
        let header_height = 2usize;
        let border_and_separator_height = 3usize;
        let visible_rows = table_height
            .saturating_sub(header_height + border_and_separator_height)
            .max(1);

        if let Some(selected) = table_state.selected() {
            if selected < table_state.offset() {
                table_state = table_state.with_offset(selected);
            } else if selected >= table_state.offset() + visible_rows {
                let new_offset = selected + 1 - visible_rows;
                table_state = table_state.with_offset(new_offset);
            }
        }

        ratatui::widgets::StatefulWidget::render(table, table_area, buf, &mut table_state);

        state.selected = table_state.selected();
        state.offset = table_state.offset();

        if sticky_columns + state.column_offset < layout.columns.len() {
            let header_separator_y = table_area.y + 2;
            if header_separator_y < table_area.bottom() - 1 {
                let line_area = Rect::new(table_area.x, header_separator_y, table_area.width, 1);
                let line = "─".repeat(table_area.width as usize);
                Paragraph::new(line.as_str())
                    .style(Style::default())
                    .render(line_area, buf);
            }
        }

        if state.focus == MetricsFocus::Assembly
            || state.assembly_loading
            || state.assembly_error.is_some()
        {
            render_assembly_overlay(table_area, buf, &mut state);
        }
    }
}

impl MetricsTableTab {
    pub fn new(spec: MetricsTableSpec, session: Arc<Mutex<Session>>) -> Self {
        MetricsTableTab {
            rows: Arc::new(RwLock::new(Vec::new())),
            is_running: Arc::new(RwLock::new(false)),
            session,
            state: Arc::new(Mutex::new(MetricsState::default())),
            config: Arc::new(MetricsTableConfig::from_spec(spec)),
            layout: Arc::new(RwLock::new(None)),
        }
    }

    pub fn title(&self) -> &str {
        &self.config.title
    }

    pub fn run(&self) {
        {
            let rows = self.rows.read();
            if !rows.is_empty() || *self.is_running.read() {
                return;
            }
        }
        *self.is_running.write() = true;
        let this = self.clone();
        tokio::spawn(this.fetch_data());
    }

    async fn fetch_data(self) {
        let result: Result<Vec<MetricsRow>, String> = (|| {
            let session = self.session.lock();
            if !session.has_table(&self.config.view) {
                return Err(format!("No '{}' data in this recording", self.config.view));
            }
            let query = self.config.build_query();
            let mut stmt = session
                .connection()
                .prepare(&query)
                .map_err(|err| err.to_string())?;
            let mut result_rows = stmt.query([]).map_err(|err| err.to_string())?;

            let column_names = result_rows
                .as_ref()
                .map(|stmt| stmt.column_names())
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>();

            let layout = self.config.build_runtime_layout(&column_names)?;
            *self.layout.write() = Some(layout.clone());

            let mut rows = Vec::new();
            while let Some(row) = result_rows.next().map_err(|err| err.to_string())? {
                rows.push(read_row(&layout, row));
            }
            Ok(rows)
        })();

        let mut state = self.state.lock();
        match result {
            Ok(rows) => {
                *self.rows.write() = rows;
                state.selected = None;
                state.offset = 0;
                state.column_offset = 0;
                state.table_error = None;
            }
            Err(err) => {
                *self.rows.write() = Vec::new();
                *self.layout.write() = None;
                state.table_error = Some(err);
            }
        }

        *self.is_running.write() = false;
    }

    pub fn handle_event(&mut self, code: KeyCode) {
        let metrics_len = self.rows.read().len();
        let layout_opt = self.layout.read().clone();
        let Some(layout) = layout_opt else {
            return;
        };

        let mut state = self.state.lock();

        if state.focus == MetricsFocus::Assembly {
            match code {
                KeyCode::Esc | KeyCode::Enter => {
                    state.assembly_request_id = state.assembly_request_id.wrapping_add(1);
                    state.focus = MetricsFocus::List;
                    state.assembly = None;
                    state.assembly_loading = false;
                    state.assembly_error = None;
                    state.assembly_summary = None;
                }
                _ => {
                    if state.assembly_loading {
                        return;
                    }
                    if let Some(assembly) = state.assembly.as_mut() {
                        let len = assembly.rows.len();
                        if len == 0 {
                            return;
                        }
                        match code {
                            KeyCode::Down => {
                                let current = assembly.selected.unwrap_or(0);
                                let next = (current + 1).min(len - 1);
                                assembly.selected = Some(next);
                                if next >= assembly.offset + ASSEMBLY_VIEW_WINDOW_HINT {
                                    assembly.offset = assembly.offset.saturating_add(1);
                                }
                            }
                            KeyCode::Up => {
                                let current = assembly.selected.unwrap_or(0);
                                let next = current.saturating_sub(1);
                                assembly.selected = Some(next);
                                if next < assembly.offset {
                                    assembly.offset = assembly.offset.saturating_sub(1);
                                }
                            }
                            KeyCode::PageDown => {
                                let current = assembly.selected.unwrap_or(0);
                                let next = (current + ASSEMBLY_SCROLL_STEP).min(len - 1);
                                assembly.selected = Some(next);
                                assembly.offset = (assembly.offset + ASSEMBLY_SCROLL_STEP)
                                    .min(len.saturating_sub(1));
                            }
                            KeyCode::PageUp => {
                                let current = assembly.selected.unwrap_or(0);
                                let next = current.saturating_sub(ASSEMBLY_SCROLL_STEP);
                                assembly.selected = Some(next);
                                assembly.offset =
                                    assembly.offset.saturating_sub(ASSEMBLY_SCROLL_STEP);
                            }
                            KeyCode::Home => {
                                assembly.selected = Some(0);
                                assembly.offset = 0;
                            }
                            KeyCode::End => {
                                assembly.selected = Some(len - 1);
                            }
                            _ => {}
                        }
                    }
                }
            }
            return;
        }

        if metrics_len == 0 {
            return;
        }

        let max_metric_offset = layout
            .columns
            .len()
            .saturating_sub(layout.sticky_columns)
            .saturating_sub(1);
        match code {
            KeyCode::Down => {
                let current = state.selected.unwrap_or(0);
                let next = (current + 1).min(metrics_len - 1);
                state.selected = Some(next);
            }
            KeyCode::Up => {
                let current = state.selected.unwrap_or(0);
                let next = current.saturating_sub(1);
                state.selected = Some(next);
            }
            KeyCode::PageDown => {
                let step = 5;
                let current = state.selected.unwrap_or(0);
                let next = (current + step).min(metrics_len - 1);
                state.selected = Some(next);
                let new_offset = state.offset.saturating_add(step);
                state.offset = new_offset.min(metrics_len.saturating_sub(1));
            }
            KeyCode::PageUp => {
                let step = 5;
                let current = state.selected.unwrap_or(0);
                let next = current.saturating_sub(step);
                state.selected = Some(next);
                state.offset = state.offset.saturating_sub(step);
            }
            KeyCode::Home => {
                state.selected = Some(0);
                state.offset = 0;
            }
            KeyCode::End => {
                state.selected = Some(metrics_len - 1);
            }
            KeyCode::Right => {
                if state.column_offset < max_metric_offset {
                    state.column_offset += 1;
                }
            }
            KeyCode::Left => {
                state.column_offset = state.column_offset.saturating_sub(1);
            }
            KeyCode::Enter => {
                if !self.config.enable_assembly {
                    return;
                }
                if let Some(idx) = state.selected {
                    let summary = collect_summary(&layout, &self.rows.read(), idx);
                    let func_name = layout.function_column_index.and_then(|column_idx| {
                        self.rows
                            .read()
                            .get(idx)
                            .and_then(|row| row.values.get(column_idx))
                            .and_then(|value| value.as_text().map(|s| s.to_string()))
                    });

                    if let Some(func_name) = func_name {
                        state.focus = MetricsFocus::Assembly;
                        state.assembly_loading = true;
                        state.assembly_error = None;
                        state.assembly = None;
                        state.assembly_summary = summary;
                        let this = self.clone();
                        let request_id = state.next_assembly_request_id();
                        drop(state);
                        this.request_assembly(func_name, request_id);
                    } else {
                        state.assembly_error =
                            Some("Unable to open assembly view for the selected row".to_string());
                        state.assembly_summary = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn request_assembly(&self, func_name: String, request_id: u64) {
        let this = self.clone();
        tokio::spawn(this.fetch_assembly(func_name, request_id));
    }

    async fn fetch_assembly(self, func_name: String, request_id: u64) {
        let result: Result<AssemblyViewState, String> = (|| {
            let session = self.session.lock();
            for table in [
                "pmu_counters",
                "proc_map",
                "assembly_address_stats",
                "assembly_lines",
            ] {
                if !session.has_table(table) {
                    return Err("Assembly data is not available for the selected row".to_string());
                }
            }
            let conn = session.connection();

            let mut module_stmt = conn
                .prepare(
                    "SELECT proc_map.module_path AS module_path, SUM(pmu_counters.pmu_cycles) AS total_cycles
                     FROM pmu_counters
                     INNER JOIN proc_map ON proc_map.ip = pmu_counters.ip
                     WHERE proc_map.func_name = ?
                     GROUP BY proc_map.module_path
                     ORDER BY total_cycles DESC
                     LIMIT 1",
                )
                .map_err(|err| err.to_string())?;
            let module_path = {
                let mut rows = module_stmt
                    .query(params![func_name])
                    .map_err(|err| err.to_string())?;
                match rows.next().map_err(|err| err.to_string())? {
                    Some(row) => row.get::<_, String>(0).map_err(|err| err.to_string())?,
                    None => {
                        return Err(
                            "Assembly data is not available for the selected row".to_string()
                        );
                    }
                }
            };

            let mut stats_stmt = conn
                .prepare(
                    "SELECT address, samples, cycles, instructions, branch_misses, branch_instructions, llc_misses, llc_references
                     FROM assembly_address_stats
                     WHERE module_path = ? AND func_name = ?
                     ORDER BY address",
                )
                .map_err(|err| err.to_string())?;

            let mut stats_map = HashMap::new();
            let mut ordered_addresses = Vec::new();
            let mut total_samples = 0u64;

            {
                let mut rows = stats_stmt
                    .query(params![module_path, func_name])
                    .map_err(|err| err.to_string())?;
                while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                    let count = |column| {
                        row.get::<_, i64>(column)
                            .map(|value| value.max(0) as u64)
                            .map_err(|err: store::duckdb::Error| err.to_string())
                    };
                    let address = row
                        .get::<_, u64>("address")
                        .map_err(|err| err.to_string())?;
                    let samples = count("samples")?;

                    stats_map.insert(
                        address,
                        AssemblyStats {
                            samples,
                            cycles: count("cycles")?,
                            instructions: count("instructions")?,
                            branch_misses: count("branch_misses")?,
                            branch_instructions: count("branch_instructions")?,
                            llc_misses: count("llc_misses")?,
                            llc_references: count("llc_references")?,
                        },
                    );
                    ordered_addresses.push(address);

                    total_samples = total_samples.saturating_add(samples);
                }
            }

            if ordered_addresses.is_empty() {
                return Err("No assembly information found for the selected function".to_string());
            }

            // PMU instruction pointers are not guaranteed to equal the first byte of an
            // instruction. Attribute each one to the closest preceding persisted instruction,
            // bounded by the maximum x86 instruction length so gaps between selected symbols do
            // not absorb unrelated samples.
            const MAX_INSTRUCTION_BYTES: u64 = 15;
            let mut instruction_stmt = conn
                .prepare(
                    "SELECT runtime_address, symbol FROM assembly_lines
                     WHERE module_path = ? AND runtime_address BETWEEN ? AND ?
                       AND symbol IS NOT NULL
                     ORDER BY runtime_address DESC LIMIT 1",
                )
                .map_err(|err| err.to_string())?;
            let mut attributed_stats = HashMap::<u64, AssemblyStats>::new();
            let mut owner_symbols = Vec::new();
            let mut unattributed = Vec::new();
            for address in ordered_addresses {
                let stats = stats_map[&address];
                let lower = address.saturating_sub(MAX_INSTRUCTION_BYTES);
                let mut rows = instruction_stmt
                    .query(params![module_path, lower, address])
                    .map_err(|err| err.to_string())?;
                match rows.next().map_err(|err| err.to_string())? {
                    Some(row) => {
                        let instruction_address = row
                            .get::<_, u64>("runtime_address")
                            .map_err(|err| err.to_string())?;
                        let owner = row
                            .get::<_, String>("symbol")
                            .map_err(|err| err.to_string())?;
                        attributed_stats
                            .entry(instruction_address)
                            .or_default()
                            .merge(stats);
                        owner_symbols.push(owner);
                    }
                    None => unattributed.push((address, stats)),
                }
            }
            owner_symbols.sort_unstable();
            owner_symbols.dedup();

            let mut lines_stmt = conn
                .prepare(
                    "SELECT runtime_address, instruction FROM assembly_lines
                     WHERE module_path = ? AND symbol = ? ORDER BY runtime_address",
                )
                .map_err(|err| err.to_string())?;

            let mut rows = Vec::new();
            for owner in &owner_symbols {
                let mut lines = lines_stmt
                    .query(params![module_path, owner])
                    .map_err(|err| err.to_string())?;
                while let Some(row) = lines.next().map_err(|err| err.to_string())? {
                    let address = row
                        .get::<_, u64>("runtime_address")
                        .map_err(|err| err.to_string())?;
                    let instruction = row
                        .get::<_, String>("instruction")
                        .map_err(|err| err.to_string())?;
                    let stats = attributed_stats.get(&address).copied().unwrap_or_default();
                    rows.push(assembly_row(address, instruction, stats, total_samples));
                }
            }

            // Old or partially postprocessed recordings may not contain the selected machine
            // symbol. Keep their metrics visible and make the missing instruction explicit.
            rows.extend(unattributed.into_iter().map(|(address, stats)| {
                assembly_row(
                    address,
                    "<persisted instruction unavailable>".to_string(),
                    stats,
                    total_samples,
                )
            }));
            rows.sort_unstable_by_key(|row| row.address);

            if rows.is_empty() {
                return Err(
                    "Persisted assembly is not available for the selected function".to_string(),
                );
            }

            let max_samples = rows.iter().map(|row| row.samples).max().unwrap_or(0);
            let symbol = match owner_symbols.as_slice() {
                [] => "[instructions unavailable]".to_string(),
                [owner] => owner.clone(),
                owners => format!("{} machine symbols", owners.len()),
            };

            Ok(AssemblyViewState {
                func_name: func_name.clone(),
                module_path,
                symbol,
                rows,
                selected: None,
                offset: 0,
                max_samples,
            })
        })();

        let mut state = self.state.lock();
        if request_id != state.assembly_request_id {
            return;
        }

        match result {
            Ok(view) => {
                state.assembly_loading = false;
                state.assembly_error = None;
                state.assembly = Some(view);
            }
            Err(err) => {
                state.assembly_loading = false;
                state.assembly_error = Some(err);
                state.assembly = None;
            }
        }
    }
}

impl MetricsTableConfig {
    fn from_spec(spec: MetricsTableSpec) -> Self {
        let view = spec.view.clone();
        let title = spec
            .title
            .clone()
            .unwrap_or_else(|| default_tab_title(&view));
        let title = format!(" {} ", title);

        let mut columns = Vec::new();
        if spec.include_default_columns {
            columns.extend(default_columns());
        }
        columns.extend(spec.columns.into_iter().map(ColumnConfig::from_spec));

        MetricsTableConfig {
            title,
            view: spec.view,
            columns,
            sticky_override: spec.sticky_columns,
            order_by: spec.order_by.map(|order| OrderClause {
                column: order.column,
                direction: order.direction,
            }),
            limit: spec.limit,
            function_column: spec
                .function_column
                .or_else(|| Some("func_name".to_string())),
            enable_assembly: spec.enable_assembly,
        }
    }

    fn build_query(&self) -> String {
        let mut query = format!("SELECT * FROM {}", self.view);
        if let Some(order) = &self.order_by {
            query.push_str(" ORDER BY ");
            query.push_str(&order.column);
            query.push(' ');
            query.push_str(match order.direction {
                SortDirection::Asc => "ASC",
                SortDirection::Desc => "DESC",
            });
        }
        if let Some(limit) = self.limit {
            query.push_str(&format!(" LIMIT {}", limit));
        }
        query
    }

    fn build_runtime_layout(&self, available: &HashSet<String>) -> Result<RuntimeLayout, String> {
        let mut resolved = Vec::new();
        let mut missing = Vec::new();

        for column in &self.columns {
            if available.contains(&column.key) {
                resolved.push(column.clone());
            } else if !column.optional {
                missing.push(column.label.clone());
            }
        }

        if !missing.is_empty() {
            return Err(format!("Missing required columns: {}", missing.join(", ")));
        }

        if resolved.is_empty() {
            return Err("No columns available for this metrics table".to_string());
        }

        let sticky_columns = self
            .sticky_override
            .map(|count| count.min(resolved.len()))
            .unwrap_or_else(|| {
                resolved
                    .iter()
                    .take_while(|column| column.sticky)
                    .count()
                    .max(1)
            });

        let function_column_index = self
            .function_column
            .as_ref()
            .and_then(|key| resolved.iter().position(|column| &column.key == key));

        Ok(RuntimeLayout {
            columns: resolved,
            sticky_columns,
            function_column_index,
        })
    }
}

impl MetricsState {
    fn next_assembly_request_id(&mut self) -> u64 {
        self.assembly_request_id = self.assembly_request_id.wrapping_add(1);
        self.assembly_request_id
    }
}

impl ColumnConfig {
    fn default_column(
        key: &str,
        label: &str,
        format: ValueFormat,
        width: Option<u16>,
        sticky: bool,
        optional: bool,
    ) -> Self {
        let alignment = match format {
            ValueFormat::Text => Alignment::Left,
            _ => Alignment::Right,
        };
        ColumnConfig {
            key: key.to_string(),
            label: label.to_string(),
            format,
            width,
            sticky,
            optional,
            alignment,
        }
    }

    fn from_spec(spec: MetricColumnSpec) -> Self {
        let alignment = match spec.format {
            ValueFormat::Text => Alignment::Left,
            _ => Alignment::Right,
        };
        ColumnConfig {
            key: spec.key.clone(),
            label: spec.label.clone().unwrap_or_else(|| spec.key.clone()),
            format: spec.format,
            width: spec.width,
            sticky: spec.sticky,
            optional: spec.optional,
            alignment,
        }
    }
}

impl MetricValue {
    fn as_text(&self) -> Option<&str> {
        match self {
            MetricValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_integer(&self) -> Option<i64> {
        match self {
            MetricValue::Integer(v) => Some(*v),
            MetricValue::Float(v) => Some(*v as i64),
            _ => None,
        }
    }

    fn as_float(&self) -> Option<f64> {
        match self {
            MetricValue::Float(v) => Some(*v),
            MetricValue::Integer(v) => Some(*v as f64),
            _ => None,
        }
    }
}

fn default_columns() -> Vec<ColumnConfig> {
    vec![
        ColumnConfig::default_column(
            "func_name",
            "Function",
            ValueFormat::Text,
            Some(34),
            true,
            false,
        ),
        ColumnConfig::default_column(
            "total",
            "Total %",
            ValueFormat::Percent2,
            Some(12),
            false,
            false,
        ),
        ColumnConfig::default_column(
            "cycles",
            "Cycles",
            ValueFormat::Integer,
            Some(18),
            false,
            false,
        ),
        ColumnConfig::default_column(
            "instructions",
            "Instructions",
            ValueFormat::Integer,
            Some(18),
            false,
            false,
        ),
        ColumnConfig::default_column("ipc", "IPC", ValueFormat::Float2, Some(10), false, false),
    ]
}

fn read_row(layout: &RuntimeLayout, row: &SqlRow<'_>) -> MetricsRow {
    let mut values = Vec::with_capacity(layout.columns.len());

    for column in &layout.columns {
        values.push(read_value(row, column));
    }

    MetricsRow { values }
}

fn read_value(row: &SqlRow<'_>, column: &ColumnConfig) -> MetricValue {
    let key = column.key.as_str();
    match column.format {
        ValueFormat::Text => row
            .get::<_, Option<String>>(key)
            .ok()
            .flatten()
            .map(MetricValue::Text)
            .unwrap_or(MetricValue::Null),
        ValueFormat::Integer => row
            .get::<_, Option<i64>>(key)
            .ok()
            .flatten()
            .map(MetricValue::Integer)
            .unwrap_or(MetricValue::Null),
        _ => row
            .get::<_, Option<f64>>(key)
            .ok()
            .flatten()
            .map(MetricValue::Float)
            .unwrap_or(MetricValue::Null),
    }
}

fn build_header(layout: &RuntimeLayout, sticky_len: usize, column_offset: usize) -> Row<'static> {
    let mut cells = Vec::new();

    for idx in 0..sticky_len.min(layout.columns.len()) {
        let column = &layout.columns[idx];
        let text = Text::from(column.label.clone()).alignment(column.alignment);
        cells.push(Cell::from(text));
    }

    for idx in (sticky_len + column_offset).min(layout.columns.len())..layout.columns.len() {
        let column = &layout.columns[idx];
        let text = Text::from(column.label.clone()).alignment(column.alignment);
        cells.push(Cell::from(text));
    }

    Row::new(cells).height(2).style(Style::new().bold())
}

fn build_constraints(
    layout: &RuntimeLayout,
    sticky_len: usize,
    column_offset: usize,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();

    for idx in 0..sticky_len.min(layout.columns.len()) {
        constraints.push(column_constraint(&layout.columns[idx]));
    }

    for idx in (sticky_len + column_offset).min(layout.columns.len())..layout.columns.len() {
        constraints.push(column_constraint(&layout.columns[idx]));
    }

    constraints
}

fn column_constraint(column: &ColumnConfig) -> Constraint {
    match column.width {
        Some(width) => Constraint::Length(width),
        None => match column.alignment {
            Alignment::Left => Constraint::Max(30),
            Alignment::Right | Alignment::Center => Constraint::Max(18),
        },
    }
}

fn build_row(
    row: &MetricsRow,
    layout: &RuntimeLayout,
    sticky_len: usize,
    column_offset: usize,
) -> Row<'static> {
    let mut cells = Vec::new();

    for idx in 0..sticky_len.min(layout.columns.len()) {
        let column = &layout.columns[idx];
        let value = row.values.get(idx).unwrap_or(&MetricValue::Null);
        let formatted = format_value(value, &column.format);
        let text = Text::from(formatted).alignment(column.alignment);
        cells.push(Cell::from(text));
    }

    for idx in (sticky_len + column_offset).min(layout.columns.len())..layout.columns.len() {
        let column = &layout.columns[idx];
        let value = row.values.get(idx).unwrap_or(&MetricValue::Null);
        let formatted = format_value(value, &column.format);
        let text = Text::from(formatted).alignment(column.alignment);
        cells.push(Cell::from(text));
    }

    Row::new(cells)
}

fn collect_summary(
    layout: &RuntimeLayout,
    rows: &[MetricsRow],
    idx: usize,
) -> Option<Vec<(String, String)>> {
    let row = rows.get(idx)?;
    let mut summary = Vec::new();

    for (col_idx, column) in layout.columns.iter().enumerate() {
        if col_idx < layout.sticky_columns {
            continue;
        }
        let value = row.values.get(col_idx).unwrap_or(&MetricValue::Null);
        summary.push((column.label.clone(), format_value(value, &column.format)));
    }

    Some(summary)
}

fn format_value(value: &MetricValue, format: &ValueFormat) -> String {
    match format {
        ValueFormat::Text => value.as_text().unwrap_or("N/A").to_string(),
        ValueFormat::Integer => value
            .as_integer()
            .map(|v| v.to_formatted_string(&Locale::en))
            .unwrap_or_else(|| "N/A".to_string()),
        ValueFormat::Float
        | ValueFormat::Float1
        | ValueFormat::Float2
        | ValueFormat::Float3
        | ValueFormat::Auto => value
            .as_float()
            .map(|v| format_number(v, float_precision(format)))
            .unwrap_or_else(|| "N/A".to_string()),
        ValueFormat::Percent
        | ValueFormat::Percent1
        | ValueFormat::Percent2
        | ValueFormat::Percent3 => value
            .as_float()
            .map(|v| format!("{}%", format_number(v * 100.0, float_precision(format))))
            .unwrap_or_else(|| "N/A".to_string()),
    }
}

fn float_precision(format: &ValueFormat) -> usize {
    match format {
        ValueFormat::Float1 | ValueFormat::Percent1 => 1,
        ValueFormat::Float3 | ValueFormat::Percent3 => 3,
        _ => 2,
    }
}

fn format_number(value: f64, precision: usize) -> String {
    match precision {
        0 => format!("{:.0}", value),
        1 => format!("{:.1}", value),
        2 => format!("{:.2}", value),
        3 => format!("{:.3}", value),
        _ => format!("{:.2}", value),
    }
}

fn render_assembly_overlay(
    area: Rect,
    buf: &mut ratatui::prelude::Buffer,
    state: &mut MetricsState,
) {
    let layout = Layout::vertical([Constraint::Fill(1)]).vertical_margin(2);
    let [inner_area] = layout.areas(area);

    let layout = Layout::horizontal([Constraint::Fill(1)]).horizontal_margin(2);
    let [inner_area] = layout.areas(inner_area);

    Clear.render(inner_area, buf);

    let block = Block::bordered().title("Assembly view");
    block.render(inner_area, buf);

    if state.assembly_loading {
        Paragraph::new("Loading assembly...")
            .alignment(Alignment::Center)
            .render(inner_area, buf);
        return;
    }

    if let Some(message) = state.assembly_error.as_ref() {
        Paragraph::new(message.to_string())
            .alignment(Alignment::Center)
            .render(inner_area, buf);
        return;
    }

    let Some(view) = state.assembly.as_mut() else {
        Paragraph::new("Assembly data is not available")
            .alignment(Alignment::Center)
            .render(inner_area, buf);
        return;
    };

    if view.rows.is_empty() {
        Paragraph::new("No disassembly found for the selected function")
            .alignment(Alignment::Center)
            .render(inner_area, buf);
        return;
    }

    let layout = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]);
    let [info_area, table_area] = layout.areas(inner_area);

    let mut info_lines = vec![
        Line::from(format!("Function: {}", view.func_name)),
        Line::from(format!("Module: {}", view.module_path)),
        Line::from(format!("Symbol: {}", view.symbol)),
    ];

    if let Some(summary) = state.assembly_summary.as_ref() {
        info_lines.push(Line::from("Metrics:"));
        for (label, value) in summary.iter().take(8) {
            info_lines.push(Line::from(format!("  {}: {}", label, value)));
        }
    }

    Paragraph::new(info_lines).render(info_area, buf);

    let has_branch = view
        .rows
        .iter()
        .any(|row| row.branch_instructions > 0 || row.branch_misses > 0);
    let has_cache = view
        .rows
        .iter()
        .any(|row| row.llc_references > 0 || row.llc_misses > 0);

    let mut header_cells = vec![
        Cell::from(""),
        Cell::from("Address"),
        Cell::from("Assembly"),
        Cell::from(Text::from("Samples").alignment(Alignment::Right)),
        Cell::from(Text::from("Share %").alignment(Alignment::Right)),
        Cell::from(Text::from("Cycles").alignment(Alignment::Right)),
        Cell::from(Text::from("Instructions").alignment(Alignment::Right)),
        Cell::from(Text::from("IPC").alignment(Alignment::Right)),
    ];

    if has_branch {
        header_cells.push(Cell::from(
            Text::from("Branch MPKI").alignment(Alignment::Right),
        ));
        header_cells.push(Cell::from(
            Text::from("Branch mispred %").alignment(Alignment::Right),
        ));
    }
    if has_cache {
        header_cells.push(Cell::from(
            Text::from("Cache MPKI").alignment(Alignment::Right),
        ));
        header_cells.push(Cell::from(
            Text::from("Cache miss %").alignment(Alignment::Right),
        ));
    }

    let header = Row::new(header_cells).style(Style::new().bold());

    let rows_iter = view.rows.iter().map(|row| {
        let heat_cell = Cell::from("  ").style(heat_style(row.samples, view.max_samples));
        let address = format!("0x{:016x}", row.address);
        let asm_text = row.instruction.clone();
        let samples = row.samples.to_formatted_string(&Locale::en);
        let share = format!("{:.2}", row.share * 100.0);
        let cycles = row.cycles.to_formatted_string(&Locale::en);
        let instructions = row.instructions.to_formatted_string(&Locale::en);
        let ipc = if row.cycles > 0 {
            row.instructions as f64 / row.cycles as f64
        } else {
            0.0
        };
        let mut cells = vec![
            heat_cell,
            Cell::from(address),
            Cell::from(asm_text),
            Cell::from(Text::from(samples).alignment(Alignment::Right)),
            Cell::from(Text::from(share).alignment(Alignment::Right)),
            Cell::from(Text::from(cycles).alignment(Alignment::Right)),
            Cell::from(Text::from(instructions).alignment(Alignment::Right)),
            Cell::from(Text::from(format!("{:.2}", ipc)).alignment(Alignment::Right)),
        ];

        if has_branch {
            let branch_mpki = if row.instructions > 0 {
                row.branch_misses as f64 / row.instructions as f64 * 1000.0
            } else {
                0.0
            };
            let branch_miss_pct = if row.branch_instructions > 0 {
                row.branch_misses as f64 / row.branch_instructions as f64 * 100.0
            } else {
                0.0
            };
            cells.push(Cell::from(
                Text::from(format!("{:.2}", branch_mpki)).alignment(Alignment::Right),
            ));
            cells.push(Cell::from(
                Text::from(format!("{:.2}", branch_miss_pct)).alignment(Alignment::Right),
            ));
        }

        if has_cache {
            let cache_mpki = if row.instructions > 0 {
                row.llc_misses as f64 / row.instructions as f64 * 1000.0
            } else {
                0.0
            };
            let cache_miss_pct = if row.llc_misses + row.llc_references > 0 {
                row.llc_misses as f64 / (row.llc_misses + row.llc_references) as f64 * 100.0
            } else {
                0.0
            };
            cells.push(Cell::from(
                Text::from(format!("{:.2}", cache_mpki)).alignment(Alignment::Right),
            ));
            cells.push(Cell::from(
                Text::from(format!("{:.2}", cache_miss_pct)).alignment(Alignment::Right),
            ));
        }

        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Length(50),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(16),
        Constraint::Length(10),
    ];

    if has_branch {
        widths.push(Constraint::Length(14));
        widths.push(Constraint::Length(16));
    }
    if has_cache {
        widths.push(Constraint::Length(14));
        widths.push(Constraint::Length(16));
    }

    let mut table_state = TableState::default()
        .with_selected(view.selected)
        .with_offset(view.offset);

    let table = Table::new(rows_iter, widths)
        .header(header)
        .highlight_symbol("▶ ")
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .block(Block::new().borders(Borders::ALL));

    ratatui::widgets::StatefulWidget::render(table, table_area, buf, &mut table_state);

    view.selected = table_state.selected();
    view.offset = table_state.offset();
}

const ASSEMBLY_VIEW_WINDOW_HINT: usize = 20;
const ASSEMBLY_SCROLL_STEP: usize = 10;
const HEATMAP_GRADIENT: &[(f64, (u8, u8, u8))] = &[
    (0.05, (255, 250, 245)),
    (0.15, (255, 237, 188)),
    (0.3, (255, 213, 128)),
    (0.5, (255, 185, 77)),
    (0.7, (255, 140, 40)),
    (1.0, (236, 65, 25)),
];

fn heat_ratio(samples: u64, max_samples: u64) -> f64 {
    if max_samples == 0 {
        return 0.0;
    }
    samples as f64 / max_samples as f64
}

fn heat_style(samples: u64, max_samples: u64) -> Style {
    match gradient_color(samples, max_samples) {
        Some((r, g, b)) => Style::default()
            .bg(Color::Rgb(r, g, b))
            .fg(contrast_text_color(r, g, b)),
        None => Style::default(),
    }
}

fn gradient_color(samples: u64, max_samples: u64) -> Option<(u8, u8, u8)> {
    let ratio = heat_ratio(samples, max_samples);
    HEATMAP_GRADIENT
        .windows(2)
        .find(|window| ratio >= window[0].0 && ratio <= window[1].0)
        .map(|window| interpolate_color(ratio, window[0], window[1]))
}

fn interpolate_color(
    ratio: f64,
    start: (f64, (u8, u8, u8)),
    end: (f64, (u8, u8, u8)),
) -> (u8, u8, u8) {
    let span = end.0 - start.0;
    let t = if span.abs() < f64::EPSILON {
        0.0
    } else {
        (ratio - start.0) / span
    };
    let r = lerp(start.1.0, end.1.0, t);
    let g = lerp(start.1.1, end.1.1, t);
    let b = lerp(start.1.2, end.1.2, t);
    (r, g, b)
}

fn lerp(a: u8, b: u8, t: f64) -> u8 {
    ((a as f64) + (b as f64 - a as f64) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn contrast_text_color(r: u8, g: u8, b: u8) -> Color {
    let luminance = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
    if luminance > 186.0 {
        Color::Black
    } else {
        Color::White
    }
}

fn default_tab_title(view: &str) -> String {
    if view.is_empty() {
        return "Metrics".to_string();
    }
    let mut chars = view.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Metrics".to_string(),
    }
}
