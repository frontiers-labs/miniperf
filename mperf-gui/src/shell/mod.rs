mod asm;
mod cores;
mod derived;
mod flame;
mod flamescope;
mod memory;
mod resources;
mod roofline;
mod session;
mod timeline;
mod tma;
mod topdown;

use std::{path::PathBuf, sync::Arc};

use gpui::{
    App, Axis, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
    MouseButton, PathPromptOptions, Render, ScrollStrategy, Subscription, UniformListScrollHandle,
    Window, WindowAppearance, actions, div, prelude::*, px, uniform_list,
};

use crate::charts;
use crate::profile::TimeRange;
use crate::profile_analysis::{
    CallTree, CpuUtilizationHeatmap, FlameScopeHeatmap, FunctionAnalysis, FunctionMetrics,
    FunctionStat, IcicleLayout, StackWeight, ThreadBalance,
};
use crate::recent;
use crate::source::{SourceDocument, SourceLocation};
use crate::ui::{
    self, ActiveTheme, ButtonSize, ButtonVariant, Column, DropdownItem, Icon, TabItem, Theme,
    badge, button, chip, dialog, dropdown_menu, empty_state, icon, kbd, stat_tile, tab_bar,
    table_cell, table_header_sortable, table_row,
};
use derived::Derived;
use flame::{FlameHover, FlameView};
use flamescope::{ScopeHover, ScopeView, format_fold_period};
use session::{GlobalFilter, ShellSession, format_count, format_duration_seconds};
use timeline::TimelineView;

actions!(tracks_shell, [FocusSymbolFilter, ClearStage]);

const KEY_CONTEXT: &str = "TracksShell";

/// Upper bound on flame-scope rows per fold; the real count follows density.
const SCOPE_MAX_BINS: usize = 50;

/// Time buckets in the Cores occupancy lanes.
const CORE_LANE_BUCKETS: usize = 360;

/// Maps a Top-Down level-1 metric name onto its color category. Vendors name
/// these differently, so match on the stem rather than the exact label.
fn tma_category(name: &str) -> ui::TmaCategory {
    let name = name.to_lowercase();
    if name.contains("retir") {
        ui::TmaCategory::Retiring
    } else if name.contains("spec") {
        ui::TmaCategory::BadSpeculation
    } else if name.contains("front") {
        ui::TmaCategory::FrontendBound
    } else {
        ui::TmaCategory::BackendBound
    }
}

fn short_tma_label(name: &str) -> &'static str {
    match tma_category(name) {
        ui::TmaCategory::Retiring => "Ret",
        ui::TmaCategory::BadSpeculation => "BadSpec",
        ui::TmaCategory::FrontendBound => "FE",
        ui::TmaCategory::BackendBound => "BE",
    }
}

fn tma_legend(rows: &[(String, f64, ui::TmaCategory)], cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(12.0))
        .gap_y(px(4.0))
        .text_size(px(10.5))
        .text_color(theme.muted_foreground)
        .children(rows.iter().map(|(name, _, category)| {
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .size(px(8.0))
                        .rounded(px(2.0))
                        .bg(theme.tma_color(*category)),
                )
                .child(name.clone())
        }))
}

/// Fixed-width gutter bar whose length and color both track sample share.
fn heat_cell(samples: u64, max: u64, theme: &Theme) -> impl IntoElement + use<> {
    let fraction = if max > 0 {
        samples as f32 / max as f32
    } else {
        0.0
    };
    div()
        .w(px(48.0))
        .h(px(13.0))
        .flex_none()
        .rounded(px(2.0))
        .bg(theme.viz.grid.opacity(0.4))
        .child(
            div()
                .h_full()
                .w(gpui::relative(if samples > 0 {
                    fraction.max(0.04)
                } else {
                    0.0
                }))
                .rounded(px(2.0))
                .bg(charts::heat(fraction)),
        )
}

/// Clock/throttle badge: a TMA or hotspot reading is not interpretable without
/// the clock the run actually saw.
fn clock_badge(session: &ShellSession, theme: &Theme) -> Option<impl IntoElement + use<>> {
    let health = session.snapshot.as_ref()?.clock_health()?;
    Some(badge(health.label()).tint(resources::severity_color(health.severity, theme)))
}

fn summary_stat(
    label: &'static str,
    value: String,
    sub: Option<String>,
    cx: &App,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    div()
        .flex_1()
        .min_w(px(150.0))
        .child(
            ui::viz_card(label)
                .child(
                    div()
                        .truncate()
                        .text_size(px(18.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(value),
                )
                .when_some(sub, |card, sub| {
                    card.child(
                        div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(sub),
                    )
                }),
        )
        .into_any_element()
}

/// Counter-track height in the Timeline tab, taller than the pinned tracks.
const TIMELINE_TRACK_H: f32 = 44.0;

pub fn init(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-f", FocusSymbolFilter, ctx),
        KeyBinding::new("ctrl-f", FocusSymbolFilter, ctx),
        KeyBinding::new("escape", ClearStage, ctx),
    ]);
}

/// Static views in canonical tab order. Availability is data-presence driven,
/// so a recording that never collected stacks simply has fewer tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewId {
    Summary,
    Hotspots,
    Flame,
    FlameScope,
    Timeline,
    Cores,
    TopDown,
    Resources,
    Memory,
    Roofline,
}

impl ViewId {
    const ALL: [Self; 10] = [
        Self::Summary,
        Self::Hotspots,
        Self::Flame,
        Self::FlameScope,
        Self::Timeline,
        Self::Cores,
        Self::TopDown,
        Self::Resources,
        Self::Memory,
        Self::Roofline,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::Hotspots => "Hotspots",
            Self::Flame => "Flame Graph",
            Self::FlameScope => "Flame Scope",
            Self::Timeline => "Timeline",
            Self::Cores => "Cores",
            Self::TopDown => "Top-Down",
            Self::Resources => "Resources",
            Self::Memory => "Memory",
            Self::Roofline => "Roofline",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Summary => Icon::LayoutDashboard,
            Self::Hotspots => Icon::Table2,
            Self::Flame => Icon::Flame,
            Self::FlameScope => Icon::Grid3x3,
            Self::Timeline => Icon::LineChart,
            Self::Cores => Icon::Cpu,
            Self::TopDown => Icon::Layers,
            Self::Resources => Icon::Gauge,
            Self::Memory => Icon::MemoryStick,
            Self::Roofline => Icon::Mountain,
        }
    }

    fn is_available(self, session: &ShellSession) -> bool {
        match self {
            Self::Summary => true,
            Self::Hotspots | Self::Flame => session.has_stacks,
            Self::FlameScope => session.full_range.is_some() && session.total_samples > 0,
            Self::Timeline => session.lanes.is_some() || session.tracks.is_some(),
            Self::Cores => !session.profile.cpu_observations.is_empty(),
            Self::TopDown => session
                .tma
                .as_ref()
                .is_some_and(tma::TmaData::has_hierarchy),
            Self::Resources => session.snapshot.is_some(),
            Self::Memory => session.memory.is_some(),
            Self::Roofline => session.roofline.is_some(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StackMode {
    TopDown,
    BottomUp,
}

/// Flame width semantics. `Instructions` appears only when the recording
/// carries a per-sample instruction counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlameWeight {
    Cycles,
    Instructions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    SelfPct,
    TotalPct,
    CpuTime,
    Ipc,
    LlcMpki,
    BeStall,
    BrMpki,
}

struct SourceTab {
    frame_id: usize,
    title: String,
    document: Option<SourceDocument>,
    scroll: UniformListScrollHandle,
    asm_scroll: UniformListScrollHandle,
    asm: Option<Arc<asm::AsmListing>>,
    asm_error: Option<String>,
    /// Source line under the cursor, overridden by `pinned_line` when set.
    hovered_line: Option<usize>,
    pinned_line: Option<usize>,
}

impl SourceTab {
    fn active_line(&self) -> Option<usize> {
        self.pinned_line.or(self.hovered_line)
    }
}

/// `--shell`: the Tracks UI on a real recording — title bar with a recents
/// switcher, one global filter, master timeline with a live brush, hotspots,
/// selection panel, status bar. Analyses run on the background executor.
pub struct ShellView {
    focus_handle: FocusHandle,
    override_dark: Option<bool>,
    recent_results: Vec<PathBuf>,

    session: Option<Arc<ShellSession>>,
    loading: Option<PathBuf>,
    load_error: Option<String>,
    picking_directory: bool,

    filter: GlobalFilter,
    filter_generation: u64,
    analysis: Derived<FunctionAnalysis>,
    flame: Derived<CallTree>,
    flame_stacks: StackMode,
    flame_weight: FlameWeight,
    flame_zoom: Option<usize>,
    flame_layout: Option<Arc<IcicleLayout>>,
    flame_hover: Option<FlameHover>,
    scope: Derived<FlameScopeHeatmap>,
    scope_hover: Option<ScopeHover>,
    cpu_lanes: Derived<CpuUtilizationHeatmap>,
    balance: Derived<ThreadBalance>,

    recording_menu_open: bool,
    threads_menu_open: bool,
    modules_menu_open: bool,
    new_profile_open: bool,
    timeline_collapsed: bool,
    active_tab: usize,
    source_tabs: Vec<SourceTab>,
    side_panel_open: bool,
    panel_width: f32,
    selected_frame: Option<usize>,
    /// Index into the recording's roofline loops, for the Roofline view.
    roofline_loop: Option<usize>,
    /// Zoom/pan window of the roofline plot; `None` fits every loop.
    roofline_view: Option<roofline::Viewport>,
    roofline_drag: Option<roofline::Drag>,
    sort_key: SortKey,
    sort_desc: bool,
    pub brush: charts::Brush,
    symbol_input: Entity<ui::TextInput>,
    splitter: Entity<ui::Splitter>,
    _subscriptions: Vec<Subscription>,
}

impl ShellView {
    pub fn new(
        initial_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let symbol_input = cx.new(|cx| {
            ui::TextInput::new("symbol-filter", "match symbol…", cx)
                .leading_icon(Icon::Search)
                .clearable()
                .kbd_hint("⌘F")
                .height(24.0)
                .text_size(11.0)
                .width(176.0)
        });
        let input_sub = cx.subscribe_in(
            &symbol_input,
            window,
            |this: &mut Self, input, event: &ui::InputEvent, window, cx| match event {
                ui::InputEvent::Changed => {
                    let text = input.read(cx).text().to_owned();
                    if this.filter.symbol != text {
                        this.filter.symbol = text;
                        this.refresh_analysis(cx);
                        cx.notify();
                    }
                }
                ui::InputEvent::Escaped => {
                    window.focus(&this.focus_handle);
                    cx.notify();
                }
            },
        );
        let entity = cx.entity();
        let splitter = cx.new(|_| {
            ui::Splitter::new(Axis::Horizontal, move |position, window, cx| {
                entity.update(cx, |this, cx| {
                    let total = f32::from(window.viewport_size().width);
                    this.panel_width = (total - f32::from(position)).clamp(240.0, 420.0);
                    cx.notify();
                });
            })
        });
        let appearance_sub = cx.observe_window_appearance(window, |this: &mut Self, window, cx| {
            if this.override_dark.is_none() {
                cx.set_global(Theme::from_appearance(window.appearance()));
            }
            cx.notify();
        });
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let mut view = Self {
            focus_handle,
            override_dark: None,
            recent_results: recent::load(),
            session: None,
            loading: None,
            load_error: None,
            picking_directory: false,
            filter: GlobalFilter::default(),
            filter_generation: 0,
            analysis: Derived::default(),
            flame: Derived::default(),
            flame_stacks: StackMode::TopDown,
            flame_weight: FlameWeight::Cycles,
            flame_zoom: None,
            flame_layout: None,
            flame_hover: None,
            scope: Derived::default(),
            scope_hover: None,
            cpu_lanes: Derived::default(),
            balance: Derived::default(),
            recording_menu_open: false,
            threads_menu_open: false,
            modules_menu_open: false,
            new_profile_open: false,
            timeline_collapsed: false,
            active_tab: 0,
            source_tabs: Vec::new(),
            side_panel_open: true,
            panel_width: 300.0,
            selected_frame: None,
            roofline_loop: None,
            roofline_view: None,
            roofline_drag: None,
            sort_key: SortKey::SelfPct,
            sort_desc: true,
            brush: charts::Brush::default(),
            symbol_input,
            splitter,
            _subscriptions: vec![input_sub, appearance_sub],
        };
        if let Some(path) = initial_directory {
            view.open_recording(path, cx);
        }
        view
    }

    fn open_recording(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.loading.is_some() {
            return;
        }
        self.loading = Some(path.clone());
        self.load_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { ShellSession::load(&path) })
                .await;
            this.update(cx, |this, cx| {
                this.loading = None;
                match loaded {
                    Ok(session) => this.install_session(Arc::new(session), cx),
                    Err(error) => this.load_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install_session(&mut self, session: Arc<ShellSession>, cx: &mut Context<Self>) {
        let _ = recent::remember(&mut self.recent_results, &session.result_directory);
        self.session = Some(session);
        self.filter.clear();
        self.symbol_input.update(cx, |input, cx| input.clear(cx));
        self.selected_frame = None;
        self.roofline_loop = None;
        self.roofline_view = None;
        self.roofline_drag = None;
        self.source_tabs.clear();
        self.brush.clear();
        self.analysis.reset();
        self.flame.reset();
        self.flame_layout = None;
        self.flame_hover = None;
        self.scope.reset();
        self.scope_hover = None;
        self.cpu_lanes.reset();
        self.balance.reset();
        self.active_tab = self
            .views()
            .iter()
            .position(|view| *view == ViewId::Hotspots)
            .unwrap_or(0);
        self.refresh_analysis(cx);
    }

    fn pick_recording(&mut self, cx: &mut Context<Self>) {
        if self.picking_directory {
            return;
        }
        self.picking_directory = true;
        cx.notify();
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Recording".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selected.await;
            this.update(cx, |this, cx| {
                this.picking_directory = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.open_recording(path, cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        this.load_error =
                            Some(format!("Could not open directory picker: {error:#}"));
                    }
                    Err(error) => {
                        this.load_error =
                            Some(format!("Directory picker was interrupted: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Invalidates every derived dataset and recomputes what the open tab needs.
    fn refresh_analysis(&mut self, cx: &mut Context<Self>) {
        self.filter_generation += 1;
        self.flame_zoom = None;
        self.flame_hover = None;
        self.ensure_derived(cx);
    }

    fn analysis_key(&self) -> u64 {
        self.filter_generation
    }

    fn flame_key(&self) -> u64 {
        let stacks = u64::from(self.flame_stacks == StackMode::BottomUp);
        let weight = u64::from(self.flame_weight == FlameWeight::Instructions);
        self.filter_generation * 4 + stacks * 2 + weight
    }

    /// Spawns whatever the active tab needs and is not already computing.
    /// Hotspots feed the selection panel and the status bar, so the function
    /// analysis is always kept fresh; heavier per-view datasets are lazy.
    fn ensure_derived(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };

        let key = self.analysis_key();
        if self.analysis.needs(key) {
            self.analysis.begin(key);
            let filter = self.filter.clone();
            let session = session.clone();
            cx.spawn(async move |this, cx| {
                let analysis = cx
                    .background_executor()
                    .spawn(async move {
                        let resolved = filter.resolve(&session);
                        Arc::new(FunctionAnalysis::build(&session.profile, &resolved))
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this.analysis.install(key, analysis) {
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }

        if self.active_view() == Some(ViewId::FlameScope) {
            let key = self.filter.spatial_key();
            if self.scope.needs(key) {
                self.scope.begin(key);
                let filter = self.filter.spatial();
                let session = session.clone();
                cx.spawn(async move |this, cx| {
                    let heatmap = cx
                        .background_executor()
                        .spawn(async move {
                            let resolved = filter.resolve(&session);
                            FlameScopeHeatmap::build(&session.profile, &resolved, SCOPE_MAX_BINS)
                                .map(Arc::new)
                        })
                        .await;
                    this.update(cx, |this, cx| match heatmap {
                        Some(heatmap) => {
                            if this.scope.install(key, heatmap) {
                                cx.notify();
                            }
                        }
                        None => this.scope.discard(key),
                    })
                    .ok();
                })
                .detach();
            }
        }

        if self.active_view() == Some(ViewId::Cores) {
            let key = self.filter.spatial_key();
            if self.cpu_lanes.needs(key) {
                self.cpu_lanes.begin(key);
                let filter = self.filter.spatial();
                let session = session.clone();
                cx.spawn(async move |this, cx| {
                    let heatmap = cx
                        .background_executor()
                        .spawn(async move {
                            let resolved = filter.resolve(&session);
                            CpuUtilizationHeatmap::build_with_bucket_duration(
                                &session.profile,
                                &resolved,
                                CORE_LANE_BUCKETS,
                                None,
                            )
                            .map(Arc::new)
                        })
                        .await;
                    this.update(cx, |this, cx| match heatmap {
                        Some(heatmap) => {
                            if this.cpu_lanes.install(key, heatmap) {
                                cx.notify();
                            }
                        }
                        None => this.cpu_lanes.discard(key),
                    })
                    .ok();
                })
                .detach();
            }

            let key = self.analysis_key();
            if self.balance.needs(key) {
                self.balance.begin(key);
                let filter = self.filter.clone();
                let session = session.clone();
                cx.spawn(async move |this, cx| {
                    let balance = cx
                        .background_executor()
                        .spawn(async move {
                            let resolved = filter.resolve(&session);
                            ThreadBalance::build(&session.profile, &resolved).map(Arc::new)
                        })
                        .await;
                    this.update(cx, |this, cx| match balance {
                        Some(balance) => {
                            if this.balance.install(key, balance) {
                                cx.notify();
                            }
                        }
                        None => this.balance.discard(key),
                    })
                    .ok();
                })
                .detach();
            }
        }

        if self.active_view() == Some(ViewId::Flame) {
            let key = self.flame_key();
            if self.flame.needs(key) {
                self.flame.begin(key);
                let filter = self.filter.clone();
                let inverted = self.flame_stacks == StackMode::BottomUp;
                let weight = self.stack_weight(&session);
                cx.spawn(async move |this, cx| {
                    let tree = cx
                        .background_executor()
                        .spawn(async move {
                            let resolved = filter.resolve(&session);
                            Arc::new(CallTree::build_weighted(
                                &session.profile,
                                &resolved,
                                inverted,
                                weight,
                            ))
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        if this.flame.install(key, tree) {
                            this.rebuild_flame_layout();
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    fn stack_weight(&self, session: &ShellSession) -> StackWeight {
        match self.flame_weight {
            FlameWeight::Instructions => session
                .instructions_metric
                .map(StackWeight::Counter)
                .unwrap_or(StackWeight::Samples),
            FlameWeight::Cycles => StackWeight::Samples,
        }
    }

    fn rebuild_flame_layout(&mut self) {
        self.flame_layout = self
            .flame
            .latest()
            .map(|tree| Arc::new(tree.icicle_layout(self.flame_zoom)));
    }

    fn zoom_flame(&mut self, node_id: usize, cx: &mut Context<Self>) {
        self.flame_zoom = Some(node_id);
        self.flame_hover = None;
        self.rebuild_flame_layout();
        cx.notify();
    }

    fn reset_flame_zoom(&mut self, cx: &mut Context<Self>) {
        self.flame_zoom = None;
        self.flame_hover = None;
        self.rebuild_flame_layout();
        cx.notify();
    }

    fn select_frame(&mut self, frame_id: Option<usize>, cx: &mut Context<Self>) {
        self.selected_frame = frame_id;
        cx.notify();
    }

    fn is_computing(&self) -> bool {
        self.session.is_some() && self.analysis.stale(self.analysis_key())
    }

    pub(super) fn commit_time_filter(
        &mut self,
        range_seconds: Option<(f64, f64)>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let time = range_seconds.and_then(|(t0, t1)| {
            let full = session.full_range?;
            Some(TimeRange {
                start_ns: full.start_ns.saturating_add((t0 * 1e9) as u64),
                end_ns: full
                    .start_ns
                    .saturating_add((t1 * 1e9) as u64)
                    .max(full.start_ns.saturating_add((t0 * 1e9) as u64) + 1),
            })
        });
        if self.filter.time != time {
            self.filter.time = time;
            self.refresh_analysis(cx);
        }
        cx.notify();
    }

    fn selection_seconds(&self) -> Option<(f64, f64)> {
        let session = self.session.as_ref()?;
        let full = session.full_range?;
        let time = self.filter.time?;
        Some((
            time.start_ns.saturating_sub(full.start_ns) as f64 / 1e9,
            time.end_ns.saturating_sub(full.start_ns) as f64 / 1e9,
        ))
    }

    fn focus_symbol_filter(
        &mut self,
        _: &FocusSymbolFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.symbol_input.focus_handle(cx));
    }

    /// Esc, two-stage: first clears the selection, then the filters.
    fn clear_stage(&mut self, _: &ClearStage, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_frame.is_some() {
            self.selected_frame = None;
            cx.notify();
        } else if self.filter.is_active() {
            self.clear_filters(cx);
        }
        window.focus(&self.focus_handle);
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = match self.override_dark {
            Some(dark) => !dark,
            None => !matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        self.override_dark = Some(dark);
        cx.set_global(if dark { Theme::dark() } else { Theme::light() });
        cx.notify();
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.filter.clear();
        self.brush.clear();
        self.selected_frame = None;
        self.symbol_input.update(cx, |input, cx| input.clear(cx));
        self.refresh_analysis(cx);
        cx.notify();
    }

    /// Static views available for the loaded recording, in canonical order.
    fn views(&self) -> Vec<ViewId> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        ViewId::ALL
            .into_iter()
            .filter(|view| view.is_available(session))
            .collect()
    }

    fn active_view(&self) -> Option<ViewId> {
        self.views().get(self.active_tab).copied()
    }

    fn static_tab_count(&self) -> usize {
        self.views().len()
    }

    /// The load failure the window is currently showing, if any.
    ///
    /// `--tour` reads this rather than loading a recording itself, so it can
    /// only ever observe what a user would see.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Whether a session finished loading.
    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// Names of the tabs the window is currently offering.
    pub fn view_names(&self) -> Vec<String> {
        self.views()
            .into_iter()
            .map(|view| view.title().to_string())
            .collect()
    }

    /// Switches to the tab at `ix`, exactly as clicking it does.
    pub fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.set_active_tab(ix, cx);
    }

    fn set_active_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active_tab = ix;
        self.flame_hover = None;
        self.ensure_derived(cx);
        cx.notify();
    }

    /// Opens (or focuses) the source/asm tab for a function, kicking off the
    /// disassembly query on the background executor.
    fn open_source_tab(&mut self, frame_id: usize, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(frame) = session.profile.frames.get(frame_id).cloned() else {
            return;
        };
        if let Some(existing) = self
            .source_tabs
            .iter()
            .position(|tab| tab.frame_id == frame_id)
        {
            self.active_tab = self.static_tab_count() + existing;
            cx.notify();
            return;
        }

        let document = frame.file.clone().map(|file| {
            SourceDocument::load(SourceLocation {
                path: PathBuf::from(file),
                line: frame.line.unwrap_or(1) as usize,
            })
        });
        let scroll = UniformListScrollHandle::new();
        if let Some(document) = document.as_ref() {
            scroll.scroll_to_item(document.focus_line.saturating_sub(4), ScrollStrategy::Top);
        }
        self.source_tabs.push(SourceTab {
            frame_id,
            title: frame.name.clone(),
            document,
            scroll,
            asm_scroll: UniformListScrollHandle::new(),
            asm: None,
            asm_error: None,
            hovered_line: None,
            pinned_line: None,
        });
        self.active_tab = self.static_tab_count() + self.source_tabs.len() - 1;

        if session.has_assembly
            && let Some(module) = frame.module.clone()
        {
            let directory = session.result_directory.clone();
            let function = frame.name.clone();
            cx.spawn(async move |this, cx| {
                let listing = cx
                    .background_executor()
                    .spawn(async move { asm::load(&directory, &module, &function) })
                    .await;
                this.update(cx, |this, cx| {
                    let Some(tab) = this
                        .source_tabs
                        .iter_mut()
                        .find(|tab| tab.frame_id == frame_id)
                    else {
                        return;
                    };
                    match listing {
                        Ok(listing) => tab.asm = Some(Arc::new(listing)),
                        Err(error) => tab.asm_error = Some(format!("{error:#}")),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// Whether double-clicking this function can open anything worth showing.
    fn frame_has_source(&self, frame_id: usize) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session
                .profile
                .frames
                .get(frame_id)
                .is_some_and(|frame| frame.file.is_some() || session.has_assembly)
        })
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();

        let mut recording_items: Vec<DropdownItem> = self
            .recent_results
            .iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("recording");
                DropdownItem::new(name.to_owned()).mono().checked(
                    self.session
                        .as_ref()
                        .is_some_and(|session| session.result_directory == *path),
                )
            })
            .collect();
        recording_items.push(DropdownItem::new("Open recording…").separator_before());
        let recents_len = self.recent_results.len();

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .h(px(36.0))
            .pl(px(78.0))
            .pr(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .on_mouse_down(MouseButton::Left, |_, window, _| {
                window.start_window_move();
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(13.0))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("miniperf")),
            )
            .child(
                dropdown_menu(
                    "recording-menu",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(theme.radius_md())
                        .bg(theme.muted)
                        .hover(|s| s.bg(theme.accent))
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(10.5))
                                .child(match self.session.as_ref() {
                                    Some(session) => session.name.clone(),
                                    None => "no recording".to_owned(),
                                }),
                        )
                        .when_some(self.session.as_ref(), |el, session| {
                            el.child(badge(session.scenario_label()).tint(theme.viz.series[0]))
                        })
                        .child(
                            icon(Icon::ChevronDown)
                                .size(px(12.0))
                                .color(theme.muted_foreground),
                        ),
                    self.recording_menu_open,
                )
                .min_width(288.0)
                .items(recording_items)
                .on_toggle(cx.processor(|this, open: bool, _, cx| {
                    this.recording_menu_open = open;
                    cx.notify();
                }))
                .on_select(cx.processor(move |this, ix: usize, _, cx| {
                    if ix < recents_len {
                        let path = this.recent_results[ix].clone();
                        if this
                            .session
                            .as_ref()
                            .is_none_or(|session| session.result_directory != path)
                        {
                            this.open_recording(path, cx);
                        }
                    } else {
                        this.pick_recording(cx);
                    }
                    cx.notify();
                })),
            )
            .when_some(self.loading.as_ref(), |el, path| {
                el.child(
                    chip(
                        "loading-chip",
                        format!(
                            "loading {}…",
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("recording")
                        ),
                    )
                    .active(true),
                )
            })
            .child(
                button("new-profile")
                    .icon(Icon::CircleDot)
                    .label("New profile")
                    .variant(ButtonVariant::Outline)
                    .size(ButtonSize::Xs)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.new_profile_open = true;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                button("shell-theme-toggle")
                    .icon(if theme.dark { Icon::Sun } else { Icon::Moon })
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::IconSm)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
            )
    }

    fn render_filter_bar(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let thread_count = session.threads.len();
        let enabled_threads = thread_count - self.filter.disabled_threads.len();
        let module_count = session.modules.len();
        let enabled_modules = module_count - self.filter.disabled_modules.len();

        let thread_items: Vec<DropdownItem> = session
            .threads
            .iter()
            .map(|thread| {
                DropdownItem::new(thread.label.clone())
                    .checked(!self.filter.disabled_threads.contains(&thread.thread_id))
                    .trailing(format_count(thread.samples as f64))
            })
            .chain([DropdownItem::new("All threads").separator_before()])
            .collect();

        let module_items: Vec<DropdownItem> = session
            .modules
            .iter()
            .enumerate()
            .map(|(ix, module)| {
                DropdownItem::new(module.label.clone())
                    .mono()
                    .checked(!self.filter.disabled_modules.contains(&ix))
                    .trailing(format_count(module.samples as f64))
            })
            .chain([DropdownItem::new("All modules").separator_before()])
            .collect();

        let total = session.total_samples;
        let coverage_text = match (self.filter.is_active(), self.analysis.latest()) {
            (true, Some(analysis)) => format!(
                "{}% of {} samples in scope",
                if total == 0 {
                    0
                } else {
                    (analysis.total_samples as f64 / total as f64 * 100.0).round() as u32
                },
                format_count(total as f64)
            ),
            _ => format!("{} samples", format_count(total as f64)),
        };

        let selected_label = self.selected_frame.and_then(|frame_id| {
            session
                .profile
                .frames
                .get(frame_id)
                .map(|frame| frame.name.clone())
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(36.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.3))
            .child(
                div()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("FILTER"),
            )
            .child(match self.selection_seconds() {
                Some((t0, t1)) => chip("time-chip", format!("{t0:.2}s – {t1:.2}s"))
                    .icon(Icon::Clock)
                    .active(true)
                    .on_close({
                        let entity = cx.entity();
                        move |_, cx| {
                            entity.update(cx, |this, cx| {
                                this.commit_time_filter(None, cx);
                            })
                        }
                    }),
                None => chip("time-chip", "full run").icon(Icon::Clock),
            })
            .child(
                dropdown_menu(
                    "threads-menu",
                    button("threads-trigger")
                        .label(if enabled_threads == thread_count {
                            "all threads".to_string()
                        } else {
                            format!(
                                "{enabled_threads} thread{}",
                                if enabled_threads == 1 { "" } else { "s" }
                            )
                        })
                        .variant(ButtonVariant::Outline)
                        .size(ButtonSize::Xs)
                        .toggled(self.threads_menu_open),
                    self.threads_menu_open,
                )
                .min_width(208.0)
                .items(thread_items)
                .on_toggle(cx.processor(|this, open: bool, _, cx| {
                    this.threads_menu_open = open;
                    cx.notify();
                }))
                .on_select(cx.processor(|this, ix: usize, _, cx| {
                    let Some(session) = this.session.clone() else {
                        return;
                    };
                    if let Some(thread) = session.threads.get(ix) {
                        if !this.filter.disabled_threads.remove(&thread.thread_id) {
                            this.filter.disabled_threads.insert(thread.thread_id);
                        }
                    } else {
                        this.filter.disabled_threads.clear();
                    }
                    this.refresh_analysis(cx);
                    cx.notify();
                })),
            )
            .child(
                dropdown_menu(
                    "modules-menu",
                    button("modules-trigger")
                        .label(if enabled_modules == module_count {
                            "all modules".to_string()
                        } else {
                            format!(
                                "{enabled_modules} module{}",
                                if enabled_modules == 1 { "" } else { "s" }
                            )
                        })
                        .variant(ButtonVariant::Outline)
                        .size(ButtonSize::Xs)
                        .toggled(self.modules_menu_open),
                    self.modules_menu_open,
                )
                .min_width(224.0)
                .items(module_items)
                .on_toggle(cx.processor(|this, open: bool, _, cx| {
                    this.modules_menu_open = open;
                    cx.notify();
                }))
                .on_select(cx.processor(|this, ix: usize, _, cx| {
                    let Some(session) = this.session.clone() else {
                        return;
                    };
                    if ix < session.modules.len() {
                        if !this.filter.disabled_modules.remove(&ix) {
                            this.filter.disabled_modules.insert(ix);
                        }
                    } else {
                        this.filter.disabled_modules.clear();
                    }
                    this.refresh_analysis(cx);
                    cx.notify();
                })),
            )
            .child(self.symbol_input.clone())
            .when_some(selected_label, |el, label| {
                el.child(chip("frame-chip", label).mono(true).active(true).on_close({
                    let entity = cx.entity();
                    move |_, cx| {
                        entity.update(cx, |this, cx| {
                            this.selected_frame = None;
                            cx.notify();
                        })
                    }
                }))
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(coverage_text),
            )
            .when(
                self.filter.is_active() || self.selected_frame.is_some(),
                |el| {
                    el.child(
                        button("clear-all")
                            .label("Clear all")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Xs)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
                    )
                },
            )
    }

    fn render_timeline(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let view = TimelineView {
            session: session.clone(),
            disabled_threads: self.filter.disabled_threads.clone(),
            selection: self.selection_seconds(),
            preview: self.brush.preview,
        };

        div()
            .flex()
            .flex_none()
            .flex_col()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("timeline-toggle")
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .h(px(24.0))
                    .px(px(8.0))
                    .bg(theme.muted.opacity(0.4))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.muted))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.timeline_collapsed = !this.timeline_collapsed;
                        cx.notify();
                    }))
                    .child(
                        icon(if self.timeline_collapsed {
                            Icon::ChevronRight
                        } else {
                            Icon::ChevronDown
                        })
                        .size(px(12.0))
                        .color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted_foreground)
                            .child("MASTER TIMELINE — DRAG ANYWHERE TO SCOPE EVERY VIEW BELOW"),
                    ),
            )
            .when(!self.timeline_collapsed, |el| {
                let mut column = el;
                if session.lanes.is_some() {
                    column = column.child(timeline::lanes_canvas(
                        entity.clone(),
                        theme.clone(),
                        view.clone(),
                    ));
                }
                for track_index in &session.pinned_tracks {
                    column = column.child(
                        div()
                            .border_t_1()
                            .border_color(theme.border.opacity(0.6))
                            .child(timeline::track_canvas(
                                entity.clone(),
                                theme.clone(),
                                view.clone(),
                                *track_index,
                                timeline::TRACK_H,
                            )),
                    );
                }
                column
            })
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let mut items: Vec<TabItem> = self
            .views()
            .into_iter()
            .map(|view| TabItem::new(view.title()).icon(view.icon()))
            .collect();
        for tab in &self.source_tabs {
            items.push(
                TabItem::new(tab.title.clone())
                    .icon(Icon::FileCode2)
                    .mono()
                    .closable(),
            );
        }
        let static_tabs = self.static_tab_count();

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .h(px(32.0))
            .px(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.3))
            .child(
                div().flex_1().min_w(px(0.0)).child(
                    tab_bar("detail-tabs", items, self.active_tab)
                        .on_select(cx.processor(|this, ix: usize, _, cx| {
                            this.set_active_tab(ix, cx);
                        }))
                        .on_close(cx.processor(move |this, ix: usize, _, cx| {
                            let source_ix = ix - static_tabs;
                            if source_ix < this.source_tabs.len() {
                                this.source_tabs.remove(source_ix);
                                if this.active_tab == ix {
                                    this.active_tab = static_tabs.saturating_sub(1);
                                } else if this.active_tab > ix {
                                    this.active_tab -= 1;
                                }
                                cx.notify();
                            }
                        })),
                ),
            )
            .child(
                button("panel-toggle")
                    .icon(if self.side_panel_open {
                        Icon::PanelRightClose
                    } else {
                        Icon::PanelRightOpen
                    })
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::IconSm)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.side_panel_open = !this.side_panel_open;
                        cx.notify();
                    })),
            )
    }

    fn hotspot_columns(analysis: &FunctionAnalysis) -> Vec<(Column, Option<SortKey>)> {
        let has = |get: fn(&FunctionMetrics) -> Option<f64>| {
            analysis
                .functions
                .iter()
                .any(|function| get(&function.metrics).is_some())
        };
        let mut columns = vec![
            (Column::new("Function"), None),
            (
                Column::new("Self %").width(70.0).right().sortable(),
                Some(SortKey::SelfPct),
            ),
            (
                Column::new("Total %").width(70.0).right().sortable(),
                Some(SortKey::TotalPct),
            ),
        ];
        if has(|metrics| metrics.cpu_time_ns) {
            columns.push((
                Column::new("CPU time").width(70.0).right().sortable(),
                Some(SortKey::CpuTime),
            ));
        }
        if has(|metrics| metrics.ipc) {
            columns.push((
                Column::new("IPC").width(50.0).right().sortable(),
                Some(SortKey::Ipc),
            ));
        }
        if has(|metrics| metrics.llc_mpki) {
            columns.push((
                Column::new("LLC MPKI").width(70.0).right().sortable(),
                Some(SortKey::LlcMpki),
            ));
        }
        if has(|metrics| metrics.backend_stall_fraction) {
            columns.push((
                Column::new("BE stall").width(60.0).right().sortable(),
                Some(SortKey::BeStall),
            ));
        }
        if has(|metrics| metrics.branch_mpki) {
            columns.push((
                Column::new("Br MPKI").width(60.0).right().sortable(),
                Some(SortKey::BrMpki),
            ));
        }
        columns
    }

    fn sorted_rows(&self, analysis: &FunctionAnalysis) -> Vec<usize> {
        let key = |stat: &FunctionStat| -> f64 {
            match self.sort_key {
                SortKey::SelfPct => stat.self_fraction,
                SortKey::TotalPct => stat.inclusive_fraction,
                SortKey::CpuTime => stat.metrics.cpu_time_ns.unwrap_or(-1.0),
                SortKey::Ipc => stat.metrics.ipc.unwrap_or(-1.0),
                SortKey::LlcMpki => stat.metrics.llc_mpki.unwrap_or(-1.0),
                SortKey::BeStall => stat.metrics.backend_stall_fraction.unwrap_or(-1.0),
                SortKey::BrMpki => stat.metrics.branch_mpki.unwrap_or(-1.0),
            }
        };
        let mut rows: Vec<usize> = (0..analysis.functions.len()).collect();
        rows.sort_by(|a, b| {
            let ordering = key(&analysis.functions[*a]).total_cmp(&key(&analysis.functions[*b]));
            if self.sort_desc {
                ordering.reverse()
            } else {
                ordering
            }
        });
        rows
    }

    fn render_hotspots(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(analysis) = self.analysis.latest().cloned() else {
            return empty_state(Icon::Table2, "Analyzing samples…").into_any_element();
        };

        let columns = Self::hotspot_columns(&analysis);
        let header_columns: Vec<Column> =
            columns.iter().map(|(column, _)| column.clone()).collect();
        let sort_ix = columns
            .iter()
            .position(|(_, key)| *key == Some(self.sort_key));
        let sort_keys: Vec<Option<SortKey>> = columns.iter().map(|(_, key)| *key).collect();
        let header_entity = cx.entity();
        let header = table_header_sortable(
            &header_columns,
            sort_ix.map(|ix| (ix, self.sort_desc)),
            move |ix, _, cx| {
                let sort_keys = sort_keys.clone();
                header_entity.update(cx, |this, cx| {
                    if let Some(Some(key)) = sort_keys.get(ix) {
                        if this.sort_key == *key {
                            this.sort_desc = !this.sort_desc;
                        } else {
                            this.sort_key = *key;
                            this.sort_desc = true;
                        }
                        cx.notify();
                    }
                });
            },
            cx,
        );

        let rows = self.sorted_rows(&analysis);
        if rows.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(empty_state(
                    Icon::Table2,
                    format!(
                        "No samples match the current filter — {} total available.",
                        format_count(session.total_samples as f64)
                    ),
                ))
                .into_any_element();
        }

        let entity = cx.entity();
        let session = session.clone();
        let selected = self.selected_frame;
        let symbol_query = self.filter.symbol.trim().to_lowercase();

        div()
            .size_full()
            .flex()
            .flex_col()
            .when(self.is_computing(), |el| el.opacity(0.7))
            .child(header)
            .child(
                uniform_list("hotspots-rows", rows.len(), move |range, _, cx| {
                    let theme = cx.theme().clone();
                    range
                        .map(|list_ix| {
                            let function = &analysis.functions[rows[list_ix]];
                            let frame_id = function.frame_id;
                            let entity = entity.clone();
                            let module = session.module_label(frame_id);
                            let dimmed = !symbol_query.is_empty()
                                && !function.label.to_lowercase().contains(&symbol_query);
                            let self_pct = function.self_fraction * 100.0;
                            let bar_width = (self_pct / 100.0 * 240.0).min(66.0) as f32;
                            let number = |value: String, muted: bool| {
                                div()
                                    .text_size(px(11.0))
                                    .when(muted, |el| el.text_color(theme.muted_foreground))
                                    .child(value)
                            };
                            let metric_cell =
                                |column: &Column, value: Option<f64>, format: fn(f64) -> String| {
                                    table_cell(column).child(match value {
                                        Some(value) => number(format(value), false),
                                        None => number("—".to_owned(), true),
                                    })
                                };

                            let mut row = table_row(selected == Some(frame_id), cx)
                                .child(
                                    table_cell(&columns[0].0)
                                        .gap(px(6.0))
                                        .when_some(module, |el, (label, kind)| {
                                            el.child(
                                                badge(label.to_owned())
                                                    .tint(theme.module_kind_color(kind)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .font_family(theme.font_mono.clone())
                                                .text_size(px(11.0))
                                                .truncate()
                                                .when(dimmed, |el| {
                                                    el.text_color(theme.muted_foreground)
                                                })
                                                .child(function.label.clone()),
                                        ),
                                )
                                .child(
                                    table_cell(&columns[1].0)
                                        .relative()
                                        .child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .top(px(4.0))
                                                .bottom(px(4.0))
                                                .w(px(bar_width))
                                                .rounded_r(px(2.0))
                                                .bg(theme.viz.series[0].opacity(0.18)),
                                        )
                                        .child(number(format!("{self_pct:.1}%"), false)),
                                )
                                .child(table_cell(&columns[2].0).child(number(
                                    format!("{:.1}%", function.inclusive_fraction * 100.0),
                                    true,
                                )));
                            for (column, key) in columns.iter().skip(3) {
                                let metrics = &function.metrics;
                                row = row.child(match key {
                                    Some(SortKey::CpuTime) => {
                                        table_cell(column).child(match metrics.cpu_time_ns {
                                            Some(ns) => {
                                                number(format_duration_seconds(ns / 1e9), true)
                                            }
                                            None => number("—".to_owned(), true),
                                        })
                                    }
                                    Some(SortKey::Ipc) => {
                                        metric_cell(column, metrics.ipc, |v| format!("{v:.2}"))
                                    }
                                    Some(SortKey::LlcMpki) => {
                                        metric_cell(column, metrics.llc_mpki, |v| format!("{v:.1}"))
                                    }
                                    Some(SortKey::BeStall) => {
                                        metric_cell(column, metrics.backend_stall_fraction, |v| {
                                            format!("{:.0}%", v * 100.0)
                                        })
                                    }
                                    Some(SortKey::BrMpki) => {
                                        metric_cell(column, metrics.branch_mpki, |v| {
                                            format!("{v:.1}")
                                        })
                                    }
                                    _ => table_cell(column),
                                });
                            }

                            div()
                                .id(frame_id)
                                .w_full()
                                .child(row)
                                .on_click(move |event, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        if event.click_count() >= 2 {
                                            this.open_source_tab(frame_id, cx);
                                        } else if this.selected_frame == Some(frame_id) {
                                            this.selected_frame = None;
                                        } else {
                                            this.selected_frame = Some(frame_id);
                                        }
                                        cx.notify();
                                    });
                                })
                        })
                        .collect()
                })
                .flex_1()
                .min_h(px(0.0)),
            )
            .into_any_element()
    }

    fn render_summary(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let analysis = self.analysis.latest().cloned();
        let scoped_samples = analysis
            .as_ref()
            .map(|analysis| analysis.total_samples)
            .unwrap_or(session.total_samples);

        let mut tiles = vec![summary_stat(
            "elapsed",
            format_duration_seconds(session.duration_seconds()),
            Some(format!(
                "{} samples in scope",
                format_count(scoped_samples as f64)
            )),
            cx,
        )];

        if let Some(frequency) = session.sampling_frequency_hz.filter(|hz| *hz > 0) {
            let cpu_seconds = scoped_samples as f64 / frequency as f64;
            let average_cpus = cpu_seconds / session.duration_seconds().max(1e-9);
            let sub = match session.profile.logical_cpu_count {
                Some(cpus) => format!("{average_cpus:.1} of {cpus} logical CPUs avg"),
                None => format!("{average_cpus:.1} CPUs busy on average"),
            };
            tiles.push(summary_stat(
                "cpu time",
                format_duration_seconds(cpu_seconds),
                Some(sub),
                cx,
            ));
        }

        let summary = &session.summary;
        if let Some(ipc) = (summary.cycles > 0)
            .then(|| summary.instructions as f64 / summary.cycles as f64)
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            tiles.push(summary_stat(
                "IPC",
                format!("{ipc:.2}"),
                Some(format!(
                    "{} instructions · {} cycles",
                    format_count(summary.instructions as f64),
                    format_count(summary.cycles as f64)
                )),
                cx,
            ));
        }

        tiles.push(summary_stat(
            "threads",
            session.threads.len().to_string(),
            Some(format!("{} modules", session.modules.len())),
            cx,
        ));

        let block = |content: gpui::AnyElement| {
            div()
                .flex_1()
                .min_w(px(320.0))
                .child(content)
                .into_any_element()
        };
        let mut blocks: Vec<gpui::AnyElement> = Vec::new();
        if let Some(tma) = topdown::level1(session) {
            blocks.push(block(
                ui::viz_card("top-down level 1")
                    .when_some(self.view_link(ViewId::TopDown, cx), |card, link| {
                        card.action(link)
                    })
                    .child(self.render_tma_bar(&tma, cx))
                    .child(tma_legend(&tma, cx))
                    .into_any_element(),
            ));
        }
        if let Some(card) = self.render_findings_card(session, cx) {
            blocks.push(block(card));
        }
        if let Some(card) = self.render_memory_card(session, cx) {
            blocks.push(block(card));
        }
        if let Some(card) = self.render_roofline_card(session, cx) {
            blocks.push(block(card));
        }
        blocks.push(block(
            self.render_recording_card(session, cx).into_any_element(),
        ));

        let hotspots_tab = self
            .views()
            .iter()
            .position(|view| *view == ViewId::Hotspots);
        let top: Vec<FunctionStat> = analysis
            .as_ref()
            .map(|analysis| analysis.functions.iter().take(7).cloned().collect())
            .unwrap_or_default();

        div()
            .id("summary")
            .size_full()
            .overflow_y_scroll()
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().flex().flex_wrap().gap(px(8.0)).children(tiles))
            .when_some(clock_badge(session, &theme), |el, badge| {
                el.child(div().flex().flex_wrap().gap(px(6.0)).child(badge))
            })
            .child(div().flex().flex_wrap().gap(px(8.0)).children(blocks))
            .when(!top.is_empty(), |el| {
                el.child(
                    ui::viz_card("top hotspots by self time")
                        .when_some(hotspots_tab, |card, tab| {
                            card.action(
                                button("summary-open-hotspots")
                                    .label("open Hotspots →")
                                    .variant(ButtonVariant::Link)
                                    .size(ButtonSize::Sm)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_active_tab(tab, cx);
                                    })),
                            )
                        })
                        .children(top.into_iter().map(|function| {
                            self.render_summary_hotspot(&function, hotspots_tab, cx)
                        })),
                )
            })
            .child(self.render_events_card(session, cx))
            .when_some(session.profile.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.destructive)
                        .child(format!("profile data unavailable: {error}")),
                )
            })
            .into_any_element()
    }

    fn render_tma_bar(
        &self,
        rows: &[(String, f64, ui::TmaCategory)],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let last = rows.len().saturating_sub(1);
        div()
            .flex()
            .h(px(24.0))
            .w_full()
            .rounded(px(3.0))
            .overflow_hidden()
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, (name, value, category))| {
                        let share = value.clamp(0.0, 1.0) as f32;
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h_full()
                            .w(gpui::relative(share))
                            .overflow_hidden()
                            .bg(theme.tma_color(*category))
                            .when(ix != last, |el| el.mr(px(1.0)))
                            .text_size(px(10.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(gpui::white())
                            .when(share > 0.12, |el| {
                                el.child(format!("{} {:.0}%", short_tma_label(name), share * 100.0))
                            })
                    }),
            )
    }

    fn render_summary_hotspot(
        &self,
        function: &FunctionStat,
        hotspots_tab: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let frame_id = function.frame_id;
        let share = function.self_fraction;
        div()
            .id(("summary-hotspot", frame_id))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_frame = Some(frame_id);
                if let Some(tab) = hotspots_tab {
                    this.set_active_tab(tab, cx);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .child(function.label.clone()),
            )
            .child(
                div()
                    .w(px(48.0))
                    .flex_none()
                    .text_right()
                    .text_size(px(11.0))
                    .child(format!("{:.1}%", share * 100.0)),
            )
            .child(
                div()
                    .w(px(110.0))
                    .flex_none()
                    .child(ui::meter((share * 2.4) as f32).color(theme.viz.series[0])),
            )
            .into_any_element()
    }

    /// "open X →" link to another view, present only when this recording
    /// actually offers that view.
    fn view_link(&self, view: ViewId, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let tab = self
            .views()
            .iter()
            .position(|candidate| *candidate == view)?;
        Some(
            button(("summary-open", tab))
                .label(format!("open {} →", view.title()))
                .variant(ButtonVariant::Link)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| this.set_active_tab(tab, cx)))
                .into_any_element(),
        )
    }

    /// Worst-first USE findings, the headline of a Snapshot recording.
    fn render_findings_card(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let snapshot = session.snapshot.as_ref()?;
        let findings = &snapshot.findings;
        if findings.is_empty() {
            return None;
        }
        Some(
            ui::viz_card("top findings")
                .when_some(self.view_link(ViewId::Resources, cx), |card, link| {
                    card.action(link)
                })
                .children(findings.iter().take(3).map(|finding| {
                    let color = match finding.severity {
                        crate::snapshot::Severity::High => theme.viz.status_critical,
                        crate::snapshot::Severity::Medium => theme.viz.status_serious,
                        crate::snapshot::Severity::Info => theme.viz.series[0],
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(11.0))
                        .child(badge(finding.severity.label()).tint(color))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .child(finding.finding.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(finding.resource.to_uppercase()),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_memory_card(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let summary = session.memory.as_ref()?.summary.as_ref()?;
        let utilization = summary.bandwidth_utilization.unwrap_or(0.0);
        let rows = [
            (
                "DRAM bandwidth",
                summary
                    .achieved_gbytes_per_second
                    .map(|value| format!("{value:.1} GB/s avg"))
                    .unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "Footprint",
                crate::snapshot::format_bytes(summary.accessed_footprint_bytes as f64),
            ),
            (
                "Peak RSS",
                summary
                    .peak_rss_bytes
                    .map(|bytes| crate::snapshot::format_bytes(bytes as f64))
                    .unwrap_or_else(|| "—".to_owned()),
            ),
        ];
        Some(
            ui::viz_card("memory")
                .when_some(self.view_link(ViewId::Memory, cx), |card, link| {
                    card.action(link)
                })
                .children(rows.map(|(label, value)| {
                    div()
                        .flex()
                        .gap(px(8.0))
                        .text_size(px(11.0))
                        .child(
                            div()
                                .w(px(110.0))
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(label),
                        )
                        .child(div().flex_1().min_w(px(0.0)).truncate().child(value))
                }))
                .when(utilization > 0.0, |card| {
                    card.child(ui::meter(utilization as f32).color(if utilization > 0.75 {
                        theme.viz.status_serious
                    } else {
                        theme.viz.series[0]
                    }))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{:.0}% of the calibrated DRAM roof",
                                utilization * 100.0
                            )),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_roofline_card(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let data = session.roofline.as_ref()?;
        let hottest = data
            .loops
            .iter()
            .max_by_key(|entry| entry.timing_samples.unwrap_or(0))?;
        Some(
            ui::viz_card("roofline")
                .when_some(self.view_link(ViewId::Roofline, cx), |card, link| {
                    card.action(link)
                })
                .child(
                    div()
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.5))
                        .font_weight(FontWeight::MEDIUM)
                        .child(hottest.function_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.muted_foreground)
                        .child(
                            match (hottest.fp64_gflops(), hottest.fp64_arithmetic_intensity()) {
                                (Some(gflops), Some(intensity)) => {
                                    format!("{gflops:.2} GFLOP/s at {intensity:.3} FLOP/byte")
                                }
                                _ => format!("{} loops measured", data.loops.len()),
                            },
                        ),
                )
                .when_some(data.efficiency(hottest), |card, efficiency| {
                    card.child(ui::meter(efficiency as f32).color(theme.viz.series[0]))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{:.0}% of the roof at that intensity",
                                    efficiency * 100.0
                                )),
                        )
                })
                .into_any_element(),
        )
    }

    fn render_recording_card(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let sampling = match session.sampling_frequency_hz {
            Some(hz) => format!(
                "{hz} Hz · {} samples",
                format_count(session.total_samples as f64)
            ),
            None => format!("{} samples", format_count(session.total_samples as f64)),
        };
        let rows = [
            ("Scenario", session.scenario_label().to_owned()),
            ("CPU", session.cpu_model.clone()),
            ("Sampling", sampling),
            ("Recording", session.result_directory.display().to_string()),
        ];

        ui::viz_card("recording").child(div().flex().flex_col().gap(px(2.0)).children(rows.map(
            |(label, value)| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(11.0))
                    .child(
                        div()
                            .w(px(80.0))
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(value))
            },
        )))
    }

    fn render_events_card(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let events: Vec<String> = session
            .profile
            .counter_metrics
            .iter()
            .map(|metric| metric.label.clone())
            .collect();

        ui::viz_card("recorded events").child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.0))
                .when(events.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.muted_foreground)
                            .child("no per-sample counters in this recording"),
                    )
                })
                .children(events.into_iter().map(|event| {
                    div()
                        .rounded(px(3.0))
                        .bg(theme.muted)
                        .px(px(6.0))
                        .py(px(2.0))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(10.0))
                        .child(event)
                })),
        )
    }

    fn render_flame(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(layout) = self.flame_layout.clone() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_flame_toolbar(session, cx))
                .child(empty_state(Icon::Flame, "Folding stacks…"))
                .into_any_element();
        };
        if layout.frames.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_flame_toolbar(session, cx))
                .child(empty_state(
                    Icon::Flame,
                    "No stacks in scope — widen the filter",
                ))
                .into_any_element();
        }

        let view = FlameView {
            session: session.clone(),
            layout: layout.clone(),
            symbol_query: self.filter.symbol.clone(),
            selected_frame: self.selected_frame,
            hovered_node: self.flame_hover.map(|hover| hover.node_id),
        };
        let total = layout.total_samples;
        let tooltip = self.flame_hover.and_then(|hover| {
            let frame = layout
                .frames
                .iter()
                .find(|frame| frame.node_id == hover.node_id)?;
            let module = frame
                .frame_id
                .and_then(|frame_id| session.module_label(frame_id))
                .map(|(label, _)| label.to_owned());
            Some((hover, frame.label.clone(), module, frame.inclusive_samples))
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_flame_toolbar(session, cx))
            .child(
                div()
                    .id("flame-scroll")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .bg(theme.viz.surface)
                    .when(self.analysis.stale(self.analysis_key()), |el| {
                        el.opacity(0.7)
                    })
                    .child(flame::flame_canvas(cx.entity(), theme.clone(), view))
                    .when(self.flame_zoom.is_some(), |el| {
                        el.child(
                            div().absolute().top(px(8.0)).right(px(12.0)).child(
                                button("flame-reset-zoom")
                                    .label("Reset zoom")
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reset_flame_zoom(cx)),
                                    ),
                            ),
                        )
                    })
                    .when_some(tooltip, |el, (hover, label, module, value)| {
                        let share = if total > 0 {
                            value as f64 / total as f64
                        } else {
                            0.0
                        };
                        el.child(
                            div()
                                .absolute()
                                .left(px(hover.x + 12.0))
                                .top(px(hover.y + 14.0))
                                .max_w(px(384.0))
                                .rounded(theme.radius_md())
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.popover)
                                .text_color(theme.popover_foreground)
                                .px(px(10.0))
                                .py(px(6.0))
                                .text_size(px(11.0))
                                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                                .child(div().text_color(theme.muted_foreground).child(format!(
                                        "{}{} · {:.1}% of {}",
                                        module
                                            .map(|module| format!("{module} · "))
                                            .unwrap_or_default(),
                                        self.format_flame_value(session, value),
                                        share * 100.0,
                                        match self.flame_stacks {
                                            StackMode::TopDown => "total",
                                            StackMode::BottomUp => "inverted total",
                                        }
                                    )))
                                .child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .child("double-click to zoom · click to select"),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The Timeline tab: the same lanes as the master timeline plus every
    /// counter track, split into process-scoped and socket-wide groups.
    fn render_timeline_view(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let view = TimelineView {
            session: session.clone(),
            disabled_threads: self.filter.disabled_threads.clone(),
            selection: self.selection_seconds(),
            preview: self.brush.preview,
        };

        let mut process_tracks = Vec::new();
        let mut system_tracks = Vec::new();
        if let Some(tracks) = session.tracks.as_ref() {
            for (index, track) in tracks.tracks.iter().enumerate() {
                if !track.values.iter().any(Option::is_some) {
                    continue;
                }
                if timeline::is_system_track(&track.key) {
                    system_tracks.push(index);
                } else {
                    process_tracks.push(index);
                }
            }
        }

        let track_row = |index: usize| {
            div()
                .flex_none()
                .border_t_1()
                .border_color(theme.border.opacity(0.6))
                .child(timeline::track_canvas(
                    entity.clone(),
                    theme.clone(),
                    view.clone(),
                    index,
                    TIMELINE_TRACK_H,
                ))
        };

        let mut column = div()
            .id("timeline-view")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(6.0))
                    .child(ui::section_caption("thread activity · drag to filter", cx)),
            )
            .child(timeline::lanes_canvas(
                entity.clone(),
                theme.clone(),
                view.clone(),
            ));

        if process_tracks.is_empty() && system_tracks.is_empty() {
            return column
                .child(empty_state(
                    Icon::LineChart,
                    "No counter tracks in this recording",
                ))
                .into_any_element();
        }

        if !process_tracks.is_empty() {
            column = column
                .child(
                    div()
                        .flex_none()
                        .border_t_1()
                        .border_color(theme.border)
                        .px(px(8.0))
                        .py(px(6.0))
                        .child(ui::section_caption("counters · process scope", cx)),
                )
                .children(process_tracks.into_iter().map(track_row));
        }

        if !system_tracks.is_empty() {
            column = column
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_baseline()
                        .justify_between()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.muted.opacity(0.3))
                        .px(px(8.0))
                        .py(px(4.0))
                        .child(ui::section_caption("system · uncore", cx))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(
                                    "socket-wide — follows the time filter, ignores thread/symbol filters",
                                ),
                        ),
                )
                .children(system_tracks.into_iter().map(track_row));
        }

        column.into_any_element()
    }

    fn render_flame_scope(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(heatmap) = self.scope.latest().cloned() else {
            return empty_state(Icon::Grid3x3, "Folding the recording…").into_any_element();
        };

        let view = ScopeView {
            heatmap,
            selection: self.selection_seconds(),
            brush: self.brush,
            duration: session.duration_seconds(),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(26.0))
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(ui::section_caption(
                        format!(
                            "one column per {} · drag to filter time",
                            format_fold_period(view.heatmap.fold_ns)
                        ),
                        cx,
                    )),
            )
            .child(
                div()
                    .id("flame-scope-scroll")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .bg(theme.viz.surface)
                    .child(flamescope::scope_canvas(cx.entity(), theme.clone(), view))
                    .when_some(self.scope_hover, |el, hover| {
                        el.child(
                            div()
                                .absolute()
                                .left(px(hover.x + 10.0))
                                .top(px((hover.y - 34.0).max(0.0)))
                                .rounded(theme.radius_md())
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.popover)
                                .text_color(theme.popover_foreground)
                                .px(px(8.0))
                                .py(px(4.0))
                                .text_size(px(11.0))
                                .child(hover.label()),
                        )
                    }),
            )
            .into_any_element()
    }

    fn format_flame_value(&self, session: &ShellSession, value: u64) -> String {
        match self.flame_weight {
            FlameWeight::Instructions if session.instructions_metric.is_some() => {
                format!("{} instructions", format_count(value as f64))
            }
            _ => format!("{} samples", format_count(value as f64)),
        }
    }

    fn render_flame_toolbar(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let has_instructions = session.instructions_metric.is_some();

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .child(ui::section_caption("stacks", cx))
            .child(
                ui::segmented(
                    "flame-stacks",
                    vec!["Top-down".into(), "Bottom-up".into()],
                    usize::from(self.flame_stacks == StackMode::BottomUp),
                )
                .compact()
                .on_select(cx.processor(|this, ix: usize, _, cx| {
                    let mode = if ix == 0 {
                        StackMode::TopDown
                    } else {
                        StackMode::BottomUp
                    };
                    if this.flame_stacks != mode {
                        this.flame_stacks = mode;
                        this.flame_zoom = None;
                        this.flame_hover = None;
                        this.ensure_derived(cx);
                        cx.notify();
                    }
                })),
            )
            .when(has_instructions, |el| {
                el.child(
                    div()
                        .flex_none()
                        .w(px(1.0))
                        .h(px(16.0))
                        .mx(px(4.0))
                        .bg(theme.border),
                )
                .child(ui::section_caption("weight", cx))
                .child(
                    ui::segmented(
                        "flame-weight",
                        vec!["Cycles".into(), "Instructions".into()],
                        usize::from(self.flame_weight == FlameWeight::Instructions),
                    )
                    .compact()
                    .on_select(cx.processor(|this, ix: usize, _, cx| {
                        let weight = if ix == 0 {
                            FlameWeight::Cycles
                        } else {
                            FlameWeight::Instructions
                        };
                        if this.flame_weight != weight {
                            this.flame_weight = weight;
                            this.flame_zoom = None;
                            this.flame_hover = None;
                            this.ensure_derived(cx);
                            cx.notify();
                        }
                    })),
                )
            })
            .child(
                div()
                    .ml_auto()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child("color = module kind · symbol-filter matches stay lit"),
            )
    }

    /// Source/asm tab: the function's source on the left, its disassembly on
    /// the right, both gutter-heated by sample share. Hover links the panes,
    /// click pins the link.
    fn render_source_tab(&self, source_ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(tab) = self.source_tabs.get(source_ix) else {
            return empty_state(Icon::FileCode2, "source tab is gone").into_any_element();
        };
        let listing = tab.asm.clone();
        let active_line = tab.active_line();

        let header_note = match (listing.as_ref(), tab.asm_error.as_ref()) {
            (_, Some(error)) => format!("disassembly unavailable: {error}"),
            (Some(listing), None) => format!(
                "{} samples in function · heat = share of function time · click a line to pin",
                format_count(listing.total_samples as f64)
            ),
            (None, None) => "loading disassembly…".to_owned(),
        };
        let path = tab
            .document
            .as_ref()
            .map(|document| document.path.display().to_string())
            .or_else(|| {
                listing
                    .as_ref()
                    .and_then(|listing| listing.source_file.clone())
            })
            .unwrap_or_else(|| "no source file recorded".to_owned());

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.muted.opacity(0.3))
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tab.title.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(path),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(header_note),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(self.render_source_pane(
                                source_ix,
                                listing.clone(),
                                active_line,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .child(self.render_asm_pane(source_ix, listing, active_line, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_source_pane(
        &self,
        source_ix: usize,
        listing: Option<Arc<asm::AsmListing>>,
        active_line: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(tab) = self.source_tabs.get(source_ix) else {
            return div().into_any_element();
        };
        let Some(document) = tab.document.as_ref() else {
            return empty_state(Icon::FileCode2, "No source file recorded for this function")
                .into_any_element();
        };
        if let Some(error) = &document.error {
            return empty_state(Icon::FileCode2, error.clone()).into_any_element();
        }

        let lines: Vec<String> = document.lines.clone();
        let line_count = lines.len();
        let line_samples = listing
            .as_ref()
            .map(|listing| listing.line_samples.clone())
            .unwrap_or_default();
        let max_line = listing
            .as_ref()
            .map(|listing| listing.max_line_samples)
            .unwrap_or(0);
        let total = listing
            .as_ref()
            .map(|listing| listing.total_samples)
            .unwrap_or(0);
        let entity = cx.entity();

        uniform_list("source-lines", line_count, move |range, _, cx| {
            let theme = cx.theme().clone();
            range
                .map(|ix| {
                    let line_number = ix + 1;
                    let samples = line_samples.get(&line_number).copied().unwrap_or(0);
                    let share = if total > 0 {
                        samples as f64 / total as f64
                    } else {
                        0.0
                    };
                    let hover_entity = entity.clone();
                    let click_entity = entity.clone();
                    div()
                        .id(("source-line", line_number))
                        .flex()
                        .items_center()
                        .h(px(18.0))
                        .px(px(8.0))
                        .gap(px(8.0))
                        .cursor_pointer()
                        .when(active_line == Some(line_number), |el| {
                            el.bg(theme.viz.series[0].opacity(0.12))
                        })
                        .when(active_line != Some(line_number), |el| {
                            el.hover(|s| s.bg(theme.accent.opacity(0.5)))
                        })
                        .on_hover(move |hovered, _, cx| {
                            let next = hovered.then_some(line_number);
                            hover_entity
                                .update(cx, |this, cx| this.hover_source_line(source_ix, next, cx));
                        })
                        .on_click(move |_, _, cx| {
                            click_entity.update(cx, |this, cx| {
                                this.pin_source_line(source_ix, line_number, cx)
                            });
                        })
                        .child(
                            div()
                                .w(px(34.0))
                                .flex_none()
                                .text_size(px(10.0))
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.muted_foreground)
                                .text_right()
                                .child(line_number.to_string()),
                        )
                        .child(heat_cell(samples, max_line, &theme))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_size(px(11.5))
                                .font_family(theme.font_mono.clone())
                                .child(lines[ix].clone()),
                        )
                        .when(share >= 0.1, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .rounded(px(3.0))
                                    .bg(theme.viz.series[0].opacity(0.12))
                                    .px(px(4.0))
                                    .text_size(px(9.5))
                                    .text_color(theme.viz.series[0])
                                    .child(format!("{:.0}%", share * 100.0)),
                            )
                        })
                })
                .collect()
        })
        .track_scroll(tab.scroll.clone())
        .size_full()
        .into_any_element()
    }

    fn render_asm_pane(
        &self,
        source_ix: usize,
        listing: Option<Arc<asm::AsmListing>>,
        active_line: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(tab) = self.source_tabs.get(source_ix) else {
            return div().into_any_element();
        };
        let Some(listing) = listing else {
            let message = match tab.asm_error.as_ref() {
                Some(error) => error.clone(),
                None => "Loading disassembly…".to_owned(),
            };
            return empty_state(Icon::FileCode2, message).into_any_element();
        };
        if listing.instructions.is_empty() {
            return empty_state(Icon::FileCode2, "No disassembly recorded for this function")
                .into_any_element();
        }

        let count = listing.instructions.len();
        let entity = cx.entity();
        let max_samples = listing.max_instruction_samples;
        let total_llc = listing.total_llc_misses;

        uniform_list("asm-lines", count, move |range, _, cx| {
            let theme = cx.theme().clone();
            range
                .map(|ix| {
                    let instruction = &listing.instructions[ix];
                    let line = instruction.line;
                    let llc_share = if total_llc > 0 {
                        instruction.llc_misses as f64 / total_llc as f64
                    } else {
                        0.0
                    };
                    let hover_entity = entity.clone();
                    let click_entity = entity.clone();
                    div()
                        .id(("asm-line", ix))
                        .flex()
                        .items_center()
                        .h(px(18.0))
                        .px(px(8.0))
                        .gap(px(8.0))
                        .cursor_pointer()
                        .when(line.is_some() && line == active_line, |el| {
                            el.bg(theme.viz.series[0].opacity(0.12))
                        })
                        .when(line.is_none() || line != active_line, |el| {
                            el.hover(|s| s.bg(theme.accent.opacity(0.5)))
                        })
                        .on_hover(move |hovered, _, cx| {
                            let next = hovered.then_some(line).flatten();
                            hover_entity
                                .update(cx, |this, cx| this.hover_source_line(source_ix, next, cx));
                        })
                        .when_some(line, |el, line| {
                            el.on_click(move |_, _, cx| {
                                click_entity.update(cx, |this, cx| {
                                    this.pin_source_line(source_ix, line, cx)
                                });
                            })
                        })
                        .child(
                            div()
                                .w(px(54.0))
                                .flex_none()
                                .text_size(px(10.0))
                                .font_family(theme.font_mono.clone())
                                .text_color(theme.muted_foreground)
                                .text_right()
                                .child(format!("{:x}", instruction.address)),
                        )
                        .child(heat_cell(instruction.samples, max_samples, &theme))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_size(px(11.5))
                                .font_family(theme.font_mono.clone())
                                .child(instruction.text.clone()),
                        )
                        .when(llc_share >= 0.05, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .rounded(px(3.0))
                                    .bg(theme.viz.series[1].opacity(0.14))
                                    .px(px(4.0))
                                    .text_size(px(9.5))
                                    .text_color(theme.viz.series[1])
                                    .child(format!("{:.0}% LLC miss", llc_share * 100.0)),
                            )
                        })
                })
                .collect()
        })
        .track_scroll(tab.asm_scroll.clone())
        .size_full()
        .into_any_element()
    }

    fn hover_source_line(&mut self, source_ix: usize, line: Option<usize>, cx: &mut Context<Self>) {
        if let Some(tab) = self.source_tabs.get_mut(source_ix)
            && tab.hovered_line != line
        {
            tab.hovered_line = line;
            cx.notify();
        }
    }

    fn pin_source_line(&mut self, source_ix: usize, line: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.source_tabs.get_mut(source_ix) {
            tab.pinned_line = (tab.pinned_line != Some(line)).then_some(line);
            cx.notify();
        }
    }

    fn render_content(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let static_tabs = self.static_tab_count();
        if self.active_tab >= static_tabs {
            return self.render_source_tab(self.active_tab - static_tabs, cx);
        }
        match self.active_view() {
            Some(ViewId::Summary) | None => self.render_summary(session, cx),
            Some(ViewId::Hotspots) => self.render_hotspots(session, cx),
            Some(ViewId::Flame) => self.render_flame(session, cx),
            Some(ViewId::FlameScope) => self.render_flame_scope(session, cx),
            Some(ViewId::Timeline) => self.render_timeline_view(session, cx),
            Some(ViewId::Cores) => cores::render(self, session, cx),
            Some(ViewId::TopDown) => topdown::render(self, session, cx),
            Some(ViewId::Resources) => resources::render(session, cx),
            Some(ViewId::Memory) => memory::render(session, cx),
            Some(ViewId::Roofline) => roofline::render(self, session, cx),
        }
    }

    fn render_selection_panel(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();

        let mut panel = div()
            .flex()
            .flex_none()
            .flex_col()
            .w(px(self.panel_width))
            .h_full()
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(4.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.muted.opacity(0.3))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child("SELECTION"),
            );

        let details = self.selected_frame.and_then(|frame_id| {
            self.analysis
                .latest()
                .and_then(|analysis| analysis.details(frame_id))
        });

        match details {
            Some(details) => {
                let function = &details.function;
                let frame_id = function.frame_id;
                let entity = cx.entity();
                let has_source = self.frame_has_source(frame_id);

                let mut tiles = div().flex().gap(px(4.0));
                let metrics = &function.metrics;
                if let Some(ipc) = metrics.ipc {
                    tiles = tiles.child(stat_tile("IPC", format!("{ipc:.2}")));
                }
                if let Some(mpki) = metrics.llc_mpki {
                    tiles = tiles.child(stat_tile("LLC MPKI", format!("{mpki:.1}")));
                }
                if let Some(stall) = metrics.backend_stall_fraction {
                    tiles = tiles.child(stat_tile("BE stall", format!("{:.0}%", stall * 100.0)));
                }
                if metrics.ipc.is_none() && metrics.llc_mpki.is_none() {
                    tiles = tiles
                        .child(stat_tile(
                            "self",
                            format!("{:.1}%", function.self_fraction * 100.0),
                        ))
                        .child(stat_tile(
                            "total",
                            format!("{:.1}%", function.inclusive_fraction * 100.0),
                        ))
                        .child(stat_tile(
                            "samples",
                            format_count(function.inclusive_samples as f64),
                        ));
                }

                panel = panel.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .px(px(8.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .font_family(theme.font_mono.clone())
                                        .text_size(px(11.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(function.label.clone()),
                                )
                                .when(has_source, |el| {
                                    el.child(
                                        button("open-source")
                                            .icon(Icon::FileCode2)
                                            .label("source")
                                            .variant(ButtonVariant::Ghost)
                                            .size(ButtonSize::Xs)
                                            .on_click({
                                                let entity = entity.clone();
                                                move |_, _, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        this.open_source_tab(frame_id, cx)
                                                    });
                                                }
                                            }),
                                    )
                                }),
                        )
                        .child(tiles),
                );

                let related_row = |arrow: &'static str,
                                   relation: &crate::profile_analysis::FunctionRelation,
                                   theme: &Theme,
                                   entity: &Entity<Self>|
                 -> gpui::Stateful<gpui::Div> {
                    let target = relation.frame_id;
                    let entity = entity.clone();
                    let module = session
                        .module_label(target)
                        .map(|(label, _)| label.to_owned())
                        .unwrap_or_default();
                    div()
                        .id(("related", target))
                        .relative()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(8.0))
                        .py(px(3.0))
                        .border_b_1()
                        .border_color(theme.border.opacity(0.4))
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.muted.opacity(0.5)))
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.selected_frame = Some(target);
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .absolute()
                                .top(px(2.0))
                                .bottom(px(2.0))
                                .left_0()
                                .w(px(relation.fraction_of_function.min(1.0) as f32 * 90.0))
                                .rounded_r(px(2.0))
                                .bg(theme.viz.series[0].opacity(0.14)),
                        )
                        .child(
                            div()
                                .relative()
                                .w(px(40.0))
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(format!("{:.0}%", relation.fraction_of_function * 100.0)),
                        )
                        .child(
                            div()
                                .relative()
                                .text_size(px(9.0))
                                .text_color(theme.muted_foreground)
                                .child(arrow),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .child(relation.label.clone()),
                        )
                        .child(
                            div()
                                .relative()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(module),
                        )
                        .child(
                            div()
                                .relative()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(format_count(relation.samples as f64)),
                        )
                };

                let section_head = |label: &'static str, theme: &Theme| {
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(2.0))
                        .text_size(px(10.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(label)
                };

                panel = panel.child(
                    div()
                        .id("related-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(section_head("CALLERS", &theme))
                        .children(
                            details
                                .callers
                                .iter()
                                .map(|relation| related_row("↑", relation, &theme, &entity)),
                        )
                        .when(details.callers.is_empty(), |el| {
                            el.child(
                                div()
                                    .px(px(12.0))
                                    .py(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.muted_foreground)
                                    .child("— thread root —"),
                            )
                        })
                        .child(section_head("CALLEES", &theme))
                        .children(
                            details
                                .callees
                                .iter()
                                .map(|relation| related_row("↓", relation, &theme, &entity)),
                        )
                        .when(details.callees.is_empty(), |el| {
                            el.child(
                                div()
                                    .px(px(12.0))
                                    .py(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.muted_foreground)
                                    .child("— leaf —"),
                            )
                        }),
                );
            }
            None => {
                panel = panel.child(
                    div()
                        .px(px(8.0))
                        .py(px(8.0))
                        .text_size(px(11.0))
                        .text_color(theme.muted_foreground)
                        .child(
                            "Click a function in the hotspots table to inspect it here. \
                             Double-click a row to open its source.",
                        ),
                );
            }
        }

        panel
    }

    fn render_status_bar(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let filtered = self.filter.is_active() || self.selected_frame.is_some();
        let frequency = session
            .sampling_frequency_hz
            .map(|hz| format!(" · {hz} Hz"))
            .unwrap_or_default();

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .h(px(24.0))
            .px(px(8.0))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.4))
            .text_size(px(10.5))
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground.opacity(0.8))
                    .child(session.name.clone()),
            )
            .child(div().child(session.scenario_label()))
            .child(div().child(session.cpu_model.clone()))
            .child(div().child(format!(
                "{} · {} samples{frequency}",
                format_duration_seconds(session.duration_seconds()),
                format_count(session.total_samples as f64),
            )))
            .child(if filtered {
                div()
                    .text_color(theme.viz.series[0])
                    .child("● filtered view")
            } else {
                div().child("○ unfiltered")
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(kbd("⌘F"))
                    .child(div().child("find symbol"))
                    .child(kbd("esc"))
                    .child(div().child("clear")),
            )
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        if let Some(path) = self.loading.as_ref() {
            return empty_state(
                Icon::FolderOpen,
                format!(
                    "Loading {}…",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("recording")
                ),
            )
            .into_any_element();
        }

        let entity = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .child(
                icon(Icon::FolderOpen)
                    .size(px(28.0))
                    .color(theme.muted_foreground),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted_foreground)
                    .child("Open an mperf recording to explore it."),
            )
            .child(
                button("welcome-open")
                    .icon(Icon::FolderOpen)
                    .label("Open recording…")
                    .variant(ButtonVariant::Outline)
                    .on_click(cx.listener(|this, _, _, cx| this.pick_recording(cx))),
            )
            .when(!self.recent_results.is_empty(), |el| {
                el.child(
                    div()
                        .pt(px(8.0))
                        .text_size(px(10.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child("RECENT"),
                )
                .children(
                    self.recent_results
                        .iter()
                        .take(8)
                        .enumerate()
                        .map(|(ix, path)| {
                            let entity = entity.clone();
                            let target = path.clone();
                            div()
                                .id(("recent", ix))
                                .px(px(10.0))
                                .py(px(3.0))
                                .rounded(theme.radius_md())
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.muted))
                                .on_click(move |_, _, cx| {
                                    let target = target.clone();
                                    entity.update(cx, |this, cx| this.open_recording(target, cx));
                                })
                                .child(path.display().to_string())
                        }),
                )
            })
            .into_any_element()
    }

    fn render_load_error_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let error = self.load_error.clone().unwrap_or_default();
        dialog("load-error-dialog", "Could not open recording")
            .description(error)
            .width(440.0)
            .on_close({
                let entity = cx.entity();
                move |_, cx| {
                    entity.update(cx, |this, cx| {
                        this.load_error = None;
                        cx.notify();
                    })
                }
            })
            .child(
                div().flex().justify_end().child(
                    button("load-error-close")
                        .label("Close")
                        .variant(ButtonVariant::Outline)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.load_error = None;
                            cx.notify();
                        })),
                ),
            )
    }

    fn render_new_profile_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let target_card = |id: &'static str,
                           ic: Icon,
                           title: &'static str,
                           sub: &'static str,
                           active: bool,
                           theme: &Theme| {
            div()
                .id(id)
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .p(px(10.0))
                .rounded(theme.radius_md())
                .border_1()
                .border_color(if active {
                    theme.viz.series[0].opacity(0.4)
                } else {
                    theme.border
                })
                .when(active, |el| el.bg(theme.viz.series[0].opacity(0.06)))
                .cursor_pointer()
                .hover(|s| s.bg(theme.muted.opacity(0.5)))
                .child(icon(ic).size(px(16.0)).color(theme.foreground))
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.muted_foreground)
                        .child(sub),
                )
        };

        dialog("new-profile-dialog", "New profile")
            .description("Step 1 of 3 · Target — the full runner wizard lands in R1.")
            .width(440.0)
            .on_close({
                let entity = cx.entity();
                move |_, cx| {
                    entity.update(cx, |this, cx| {
                        this.new_profile_open = false;
                        cx.notify();
                    })
                }
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(target_card(
                        "target-local",
                        Icon::Terminal,
                        "This machine",
                        "record locally with mperf on PATH",
                        true,
                        &theme,
                    ))
                    .child(target_card(
                        "target-ssh",
                        Icon::Server,
                        "remote host",
                        "ssh · provisions mperf automatically",
                        false,
                        &theme,
                    )),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        button("wizard-cancel")
                            .label("Cancel")
                            .variant(ButtonVariant::Outline)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_profile_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("wizard-next")
                            .label("Next")
                            .icon(Icon::ArrowRight)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_profile_open = false;
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl Render for ShellView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_symbol_filter))
            .on_action(cx.listener(Self::clear_stage))
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_ui.clone())
            .text_size(px(13.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if window.focused(cx).is_none() {
                        window.focus(&this.focus_handle);
                    }
                }),
            )
            .child(self.render_title_bar(cx));

        let root = match self.session.clone() {
            Some(session) => root
                .child(self.render_filter_bar(&session, cx))
                .child(self.render_timeline(&session, cx))
                .child(self.render_tab_strip(cx))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h(px(0.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .h_full()
                                .overflow_hidden()
                                .child(self.render_content(&session, cx)),
                        )
                        .when(self.side_panel_open, |el| {
                            el.child(self.splitter.clone())
                                .child(self.render_selection_panel(&session, cx))
                        }),
                )
                .child(self.render_status_bar(&session, cx)),
            None => root.child(div().flex_1().min_h(px(0.0)).child(self.render_welcome(cx))),
        };

        root.when(self.load_error.is_some(), |el| {
            el.child(self.render_load_error_dialog(cx))
        })
        .when(self.new_profile_open, |el| {
            el.child(self.render_new_profile_dialog(cx))
        })
    }
}
