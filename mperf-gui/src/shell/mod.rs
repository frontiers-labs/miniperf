mod allocations;
mod asm;
mod cores;
mod derived;
mod flame;
mod flamescope;
mod memory;
mod names;
mod navigation;
mod regions;
mod resources;
mod roofline;
mod session;
mod source_view;
mod structures;
mod timeline;
mod tma;
mod topdown;

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding, MouseButton,
    PathPromptOptions, Render, ScrollStrategy, Subscription, UniformListScrollHandle, Window,
    WindowAppearance, actions, div, prelude::*, px, uniform_list,
};

use crate::charts;
use crate::profile::TimeRange;
use crate::profile_analysis::{
    CallTree, CpuUtilizationHeatmap, FlameScopeHeatmap, FunctionAnalysis, FunctionStat,
    IcicleLayout, SampleFilter, StackWeight, ThreadBalance,
};
use crate::recent;
use crate::regions::RegionNodeId;
use crate::source::{SourceDocument, SourceLocation};
use crate::ui::{
    self, ActiveTheme, ButtonSize, ButtonVariant, DropdownItem, Icon, Theme, badge, button, chip,
    dialog, dropdown_menu, empty_state, icon, kbd, stat_tile,
};
use derived::Derived;
use flame::{FlameHover, FlameView};
use flamescope::{ScopeHover, ScopeView, format_fold_period};
use navigation::{DocumentId, Workspace};
use session::{GlobalFilter, ShellSession, format_count, format_duration_seconds};
use timeline::TimelineView;

actions!(
    tracks_shell,
    [
        FocusSymbolFilter,
        ClearStage,
        ToggleSidebar,
        ToggleTheme,
        NavigateBack,
        NavigateForward,
        CloseDocument
    ]
);

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
    } else if name.contains("front") || name.contains("fe_bound") {
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
        KeyBinding::new("cmd-b", ToggleSidebar, ctx),
        KeyBinding::new("ctrl-b", ToggleSidebar, ctx),
        KeyBinding::new("d", ToggleTheme, Some("TracksShell && !TextInput")),
        KeyBinding::new("alt-left", NavigateBack, ctx),
        KeyBinding::new("alt-right", NavigateForward, ctx),
        KeyBinding::new("cmd-w", CloseDocument, ctx),
        KeyBinding::new("ctrl-w", CloseDocument, ctx),
    ]);
}

/// Views retain their production analysis routes when selected from the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewId {
    Summary,
    Hotspots,
    Flame,
    FlameScope,
    Timeline,
    Events,
    Cores,
    Resources,
    ResourceUse,
    Structures,
    Memory,
    Allocations,
    Roofline,
}

impl ViewId {
    const ALL: [Self; 13] = [
        Self::Summary,
        Self::Hotspots,
        Self::Flame,
        Self::FlameScope,
        Self::Timeline,
        Self::Events,
        Self::Cores,
        Self::Resources,
        Self::ResourceUse,
        Self::Structures,
        Self::Memory,
        Self::Allocations,
        Self::Roofline,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::Hotspots => "Hotspots",
            Self::Flame => "Flamegraph",
            Self::FlameScope => "Flame Scope",
            Self::Timeline => "Timeline",
            Self::Events => "Events",
            Self::Cores => "Cores",
            Self::Resources => "Resources",
            Self::ResourceUse => "Resource use",
            Self::Structures => "Structures",
            Self::Memory => "Memory",
            Self::Allocations => "Allocations",
            Self::Roofline => "Roofline",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Summary => Icon::LayoutDashboard,
            Self::Hotspots => Icon::ChartNoAxesColumnIncreasing,
            Self::Flame => Icon::Flame,
            Self::FlameScope => Icon::Grid3x3,
            Self::Timeline => Icon::LineChart,
            Self::Events => Icon::Layers,
            Self::Cores => Icon::Cpu,
            Self::Resources => Icon::Cpu,
            Self::ResourceUse => Icon::Gauge,
            Self::Structures => Icon::Boxes,
            Self::Memory => Icon::MemoryStick,
            Self::Allocations => Icon::MemoryStick,
            Self::Roofline => Icon::ChartLine,
        }
    }

    fn is_available(self, session: &ShellSession) -> bool {
        match self {
            Self::Summary | Self::Resources | Self::Structures => true,
            Self::Hotspots | Self::Flame => session.has_stacks,
            Self::FlameScope => session.full_range.is_some() && session.total_samples > 0,
            Self::Timeline => {
                session.lanes.is_some()
                    || session.tracks.is_some()
                    || !session.regions.spans.is_empty()
            }
            Self::Events => !session.regions.nodes.is_empty() || session.regions.error.is_some(),
            Self::Cores => !session.profile.cpu_observations.is_empty(),
            Self::ResourceUse => session.snapshot.is_some(),
            Self::Memory => session.memory.is_some(),
            Self::Allocations => session.allocations.is_some(),
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
    Function,
    SelfPct,
    TotalPct,
    Cycles,
    Instructions,
    Ipc,
    Tma(usize),
}

struct SourceTab {
    id: DocumentId,
    frame_id: usize,
    title: String,
    document: Option<SourceDocument>,
    scroll: UniformListScrollHandle,
    source_horizontal: gpui::ScrollHandle,
    assembly_horizontal: gpui::ScrollHandle,
    asm: Option<Arc<asm::AsmListing>>,
    asm_error: Option<String>,
    function_menu_open: bool,
    /// Source line under the cursor, overridden by `pinned_line` when set.
    hovered_line: Option<usize>,
    pinned_line: Option<usize>,
}

impl SourceTab {
    fn active_line(&self) -> Option<usize> {
        self.pinned_line.or(self.hovered_line)
    }
}

/// Native recording workbench. Analyses run on the background executor.
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
    collapsed_regions: BTreeSet<RegionNodeId>,
    region_hover: Option<regions::RegionHover>,
    region_timeline: Option<std::rc::Rc<regions::RegionTimeline>>,
    workspace: Workspace,
    sidebar_open: bool,
    expanded_folders: BTreeSet<PathBuf>,
    native_controls_open: bool,
    native_selection_open: bool,
    inspector_open: bool,
    wide_inspector: bool,
    narrow_inspector_open: bool,
    resource_timeline: resources::TimelineState,
    structures: structures::State,
    source_tabs: Vec<SourceTab>,
    selected_frame: Option<usize>,
    allocation_ui: allocations::ViewState,
    roofline_ui: roofline::ViewState,
    allocation_stack: Option<u64>,
    allocation_sites: Derived<Vec<(u64, allocations::Site)>>,
    /// Index into the recording's roofline loops, for the Roofline view.
    roofline_loop: Option<usize>,
    /// Zoom/pan window of the roofline plot; `None` fits every loop.
    roofline_view: Option<roofline::Viewport>,
    roofline_drag: Option<roofline::Drag>,
    sort_key: SortKey,
    sort_desc: bool,
    pub brush: charts::Brush,
    symbol_input: Entity<ui::TextInput>,
    hotspot_input: Entity<ui::TextInput>,
    hotspot_horizontal: gpui::ScrollHandle,
    flame_input: Entity<ui::TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl ShellView {
    pub fn new(
        initial_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let override_dark = recent::load_theme();
        if let Some(dark) = override_dark {
            cx.set_global(if dark { Theme::dark() } else { Theme::light() });
        }
        let symbol_input = cx.new(|cx| {
            ui::TextInput::new("symbol-filter", "Search function or module…", cx)
                .leading_icon(Icon::Search)
                .clearable()
                .height(32.0)
                .text_size(12.0)
                .width(256.0)
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
        let hotspot_input = cx.new(|cx| {
            ui::TextInput::new("hotspot-search", "Search function or module…", cx)
                .leading_icon(Icon::Search)
                .height(32.0)
                .text_size(12.0)
                .width(256.0)
        });
        let hotspot_sub = cx.subscribe(&hotspot_input, |_, _, _: &ui::InputEvent, cx| cx.notify());
        let flame_input = cx.new(|cx| {
            ui::TextInput::new("flame-search", "Search functions or modules…", cx)
                .leading_icon(Icon::Search)
                .height(28.0)
                .text_size(12.0)
                .width(224.0)
        });
        let flame_sub = cx.subscribe(&flame_input, |_, _, _: &ui::InputEvent, cx| cx.notify());
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
            override_dark,
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
            collapsed_regions: BTreeSet::new(),
            region_hover: None,
            region_timeline: None,
            workspace: Workspace::default(),
            sidebar_open: true,
            expanded_folders: BTreeSet::new(),
            native_controls_open: false,
            native_selection_open: false,
            inspector_open: true,
            wide_inspector: false,
            narrow_inspector_open: false,
            resource_timeline: Default::default(),
            structures: Default::default(),
            source_tabs: Vec::new(),
            selected_frame: None,
            allocation_ui: allocations::ViewState::new(cx),
            roofline_ui: roofline::ViewState::new(cx),
            allocation_stack: None,
            allocation_sites: Derived::default(),
            roofline_loop: None,
            roofline_view: None,
            roofline_drag: None,
            sort_key: SortKey::SelfPct,
            sort_desc: true,
            brush: charts::Brush::default(),
            symbol_input,
            hotspot_input,
            hotspot_horizontal: gpui::ScrollHandle::new(),
            flame_input,
            _subscriptions: vec![input_sub, appearance_sub, hotspot_sub, flame_sub],
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
        self.region_timeline = Some(std::rc::Rc::new(regions::RegionTimeline::new(&session)));
        self.session = Some(session);
        self.filter.clear();
        self.collapsed_regions.clear();
        self.region_hover = None;
        self.symbol_input.update(cx, |input, cx| input.clear(cx));
        self.hotspot_input.update(cx, |input, cx| input.clear(cx));
        self.hotspot_horizontal = gpui::ScrollHandle::new();
        if matches!(self.sort_key, SortKey::Tma(_)) {
            self.sort_key = SortKey::SelfPct;
            self.sort_desc = true;
        }
        self.flame_input.update(cx, |input, cx| input.clear(cx));
        self.selected_frame = None;
        self.roofline_loop = None;
        self.roofline_view = None;
        self.roofline_drag = None;
        self.source_tabs.clear();
        self.allocation_stack = None;
        self.allocation_ui.reset(cx);
        self.roofline_ui.reset(cx);
        self.allocation_sites.reset();
        self.brush.clear();
        self.analysis.reset();
        self.flame.reset();
        self.flame_layout = None;
        self.flame_hover = None;
        self.scope.reset();
        self.scope_hover = None;
        self.cpu_lanes.reset();
        self.balance.reset();
        self.workspace = Workspace::default();
        self.resource_timeline = Default::default();
        self.structures = Default::default();
        self.expanded_folders.clear();
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

    fn request<T: Send + Sync + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        key: u64,
        filter: fn(&GlobalFilter) -> GlobalFilter,
        cache: fn(&mut Self) -> &mut Derived<T>,
        build: impl FnOnce(&ShellSession, &SampleFilter) -> Option<T> + Send + 'static,
        installed: fn(&mut Self),
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if !cache(self).request(key) {
            return;
        }
        let filter = filter(&self.filter);
        cx.spawn(async move |this, cx| {
            let value = cx
                .background_executor()
                .spawn(async move { build(&session, &filter.resolve(&session)).map(Arc::new) })
                .await;
            this.update(cx, |this, cx| {
                if cache(this).install(key, value) {
                    installed(this);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Hotspots feed selection and status; heavier datasets follow the active tab.
    fn ensure_derived(&mut self, cx: &mut Context<Self>) {
        self.request(
            cx,
            self.analysis_key(),
            GlobalFilter::clone,
            |this| &mut this.analysis,
            |session, filter| Some(FunctionAnalysis::build(&session.profile, filter)),
            |_| {},
        );
        match self.active_view() {
            Some(ViewId::Structures) => structures::ensure(self, cx),
            Some(ViewId::FlameScope) => self.request(
                cx,
                self.filter.spatial_key(),
                GlobalFilter::spatial,
                |this| &mut this.scope,
                |session, filter| {
                    FlameScopeHeatmap::build(&session.profile, filter, SCOPE_MAX_BINS)
                },
                |_| {},
            ),
            Some(ViewId::Cores) => {
                self.request(
                    cx,
                    self.filter.spatial_key(),
                    GlobalFilter::spatial,
                    |this| &mut this.cpu_lanes,
                    |session, filter| {
                        CpuUtilizationHeatmap::build_with_bucket_duration(
                            &session.profile,
                            filter,
                            CORE_LANE_BUCKETS,
                            None,
                        )
                    },
                    |_| {},
                );
                self.request(
                    cx,
                    self.analysis_key(),
                    GlobalFilter::clone,
                    |this| &mut this.balance,
                    |session, filter| ThreadBalance::build(&session.profile, filter),
                    |_| {},
                );
            }
            Some(ViewId::Flame) => {
                let Some(session) = self.session.as_ref() else {
                    return;
                };
                let inverted = self.flame_stacks == StackMode::BottomUp;
                let weight = self.stack_weight(session);
                self.request(
                    cx,
                    self.flame_key(),
                    GlobalFilter::clone,
                    |this| &mut this.flame,
                    move |session, filter| {
                        Some(CallTree::build_weighted(
                            &session.profile,
                            filter,
                            inverted,
                            weight,
                        ))
                    },
                    Self::rebuild_flame_layout,
                );
            }
            Some(ViewId::Allocations) => self.request(
                cx,
                self.analysis_key(),
                GlobalFilter::clone,
                |this| &mut this.allocation_sites,
                |session, filter| {
                    session.allocations.as_ref().map(|data| {
                        allocations::allocation_sites(data, &session.profile.frames, filter)
                    })
                },
                |_| {},
            ),
            _ => {}
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
        if !self.native_controls_open && self.active_view() != Some(ViewId::Hotspots) {
            self.select_view(ViewId::Hotspots, cx);
        }
        let input = match self.active_view() {
            Some(ViewId::Hotspots) => &self.hotspot_input,
            Some(ViewId::Flame) => &self.flame_input,
            _ => {
                self.native_controls_open = true;
                &self.symbol_input
            }
        };
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    /// Esc, two-stage: first clears the selection, then the filters.
    fn clear_stage(&mut self, _: &ClearStage, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self
            .source_tabs
            .iter_mut()
            .find(|tab| Some(&tab.id) == self.workspace.active.as_ref())
            && (tab.pinned_line.is_some() || tab.hovered_line.is_some())
        {
            tab.pinned_line = None;
            tab.hovered_line = None;
            cx.notify();
        } else if self.selected_frame.is_some() {
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
        let _ = recent::save_theme(dark);
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
        (self.workspace.active == Some(DocumentId::Profile)).then_some(self.workspace.view)
    }

    /// The load failure the window is currently showing, if any.
    ///
    /// `--tour` reads this rather than loading a recording itself, so it can
    /// only ever observe what a user would see.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref().or_else(|| {
            self.session
                .as_ref()
                .and_then(|session| session.allocations.as_ref())
                .and_then(|data| data.error.as_deref())
        })
    }

    /// Whether a session finished loading.
    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// Names of the tabs the window is currently offering.
    pub fn view_names(&self) -> Vec<String> {
        self.views()
            .into_iter()
            .map(|view| {
                if view == ViewId::Flame {
                    "Flame Graph"
                } else {
                    view.title()
                }
                .to_owned()
            })
            .collect()
    }

    /// Switches to the tab at `ix`, exactly as clicking it does.
    pub fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.set_active_tab(ix, cx);
    }

    fn set_active_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(view) = self.views().get(ix).copied() {
            self.select_view(view, cx);
        }
    }

    fn select_view(&mut self, view: ViewId, cx: &mut Context<Self>) {
        if !self
            .session
            .as_ref()
            .is_some_and(|session| view.is_available(session))
        {
            return;
        }
        self.workspace.view = view;
        self.workspace.open(DocumentId::Profile);
        self.flame_hover = None;
        self.ensure_derived(cx);
        cx.notify();
    }

    fn open_source_tab(&mut self, frame_id: usize, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(frame) = session.profile.frames.get(frame_id).cloned() else {
            return;
        };
        let id = frame
            .file
            .as_ref()
            .map_or(DocumentId::Function(frame_id), |path| {
                DocumentId::File(PathBuf::from(path))
            });
        let existing = self.source_tabs.iter().position(|tab| tab.id == id);
        let source_ix = existing.unwrap_or(self.source_tabs.len());
        let document = frame.file.clone().map(|file| {
            SourceDocument::load(SourceLocation {
                path: PathBuf::from(file),
                line: frame.line.unwrap_or(1) as usize,
            })
        });
        let title = frame
            .file
            .clone()
            .unwrap_or_else(|| frame.friendly_name.clone());
        let tab = SourceTab {
            id: id.clone(),
            frame_id,
            title,
            document,
            scroll: UniformListScrollHandle::new(),
            source_horizontal: gpui::ScrollHandle::new(),
            assembly_horizontal: gpui::ScrollHandle::new(),
            asm: None,
            asm_error: None,
            function_menu_open: false,
            hovered_line: None,
            pinned_line: None,
        };
        if existing.is_some() {
            self.source_tabs[source_ix] = tab;
        } else {
            self.source_tabs.push(tab);
        }
        self.selected_frame = Some(frame_id);
        self.workspace.open(id);
        if session.has_assembly
            && let Some(module) = frame.module.clone()
        {
            let directory = session.result_directory.clone();
            let function = frame.name.clone();
            let source_path = frame.file.map(PathBuf::from);
            let recording_directory = directory.clone();
            cx.spawn(async move |this, cx| {
                let listing = cx
                    .background_executor()
                    .spawn(async move {
                        asm::load(&directory, &module, &function, source_path.as_deref())
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this
                        .session
                        .as_ref()
                        .is_none_or(|session| session.result_directory != recording_directory)
                    {
                        return;
                    }
                    let Some(tab) = this
                        .source_tabs
                        .iter_mut()
                        .find(|tab| tab.frame_id == frame_id)
                    else {
                        return;
                    };
                    match listing {
                        Ok(listing) => {
                            if tab.document.is_none()
                                && let Some(path) = listing.source_file.as_ref()
                            {
                                tab.document = Some(SourceDocument::load(SourceLocation {
                                    path: path.clone(),
                                    line: 1,
                                }));
                            }
                            tab.asm = Some(Arc::new(listing));
                            source_view::scroll_to_function(tab);
                        }
                        Err(error) => tab.asm_error = Some(format!("{error:#}")),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        } else {
            self.source_tabs[source_ix].asm_error =
                Some("No native assembly recorded for this function".to_owned());
            source_view::scroll_to_function(&self.source_tabs[source_ix]);
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
            .when_some(self.filter.region, |el, region| {
                el.child(
                    chip("region-chip", session.regions.label(region))
                        .icon(Icon::Layers)
                        .active(true)
                        .on_close({
                            let entity = cx.entity();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.filter.region = None;
                                    this.refresh_analysis(cx);
                                    cx.notify();
                                });
                            }
                        }),
                )
            })
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
                            .child("MASTER TIMELINE · DRAG ACTIVITY OR COUNTERS TO FILTER TIME"),
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

    fn hotspot_metrics(
        scroll: &gpui::ScrollHandle,
        width: gpui::Pixels,
        columns: gpui::Div,
    ) -> impl IntoElement + use<> {
        div()
            .id("hotspot-metrics")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_x_scroll()
            .track_scroll(scroll)
            .map(|mut viewport| {
                viewport.style().restrict_scroll_to_axis = Some(true);
                viewport
            })
            .child(
                columns
                    .flex()
                    .flex_none()
                    .items_center()
                    .size_full()
                    .min_w(width),
            )
    }

    fn render_hotspots(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let tma = session.tma.as_ref().filter(|tma| tma.has_functions());
        let show_tma = tma.is_some();
        let mut columns = vec![
            ("Function", SortKey::Function, None),
            ("Self %", SortKey::SelfPct, None),
            ("Total %", SortKey::TotalPct, None),
            ("Cycles", SortKey::Cycles, None),
            (
                if show_tma { "Instr." } else { "Instructions" },
                SortKey::Instructions,
                None,
            ),
            ("IPC", SortKey::Ipc, None),
        ];
        if let Some(tma) = tma {
            columns.extend(tma.level1_rows().enumerate().map(|(index, row)| {
                (
                    short_tma_label(&row.name),
                    SortKey::Tma(index),
                    Some(row.name.replace('_', " ")),
                )
            }));
        }
        let metric_count = (columns.len() - 1) as f32;
        let metric_width = gpui::relative(1.0 / metric_count);
        let metrics_min_width = px(96.0 * metric_count);
        let function_width = gpui::relative(if show_tma { 0.36 } else { 0.44 });
        let metric_padding = if show_tma { 4.0 } else { 12.0 };
        let mut header_columns =
            columns
                .into_iter()
                .enumerate()
                .map(|(index, (label, key, metric))| {
                    let active = self.sort_key == key;
                    div()
                        .id(("hotspot-sort", index))
                        .flex()
                        .items_center()
                        .h_full()
                        .flex_none()
                        .overflow_hidden()
                        .when(index == 0, |el| {
                            el.w(function_width)
                                .min_w(px(124.0))
                                .max_w(px(400.0))
                                .pl(px(16.0))
                        })
                        .when(index > 0, |el| {
                            el.w(metric_width)
                                .min_w(px(96.0))
                                .pl(px(metric_padding))
                                .justify_end()
                        })
                        .pr(px(metric_padding))
                        .gap(px(4.0))
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if active {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(label)
                        .when_some(metric, |el, metric| {
                            el.tooltip(ui::tooltip::text_tooltip(format!(
                                "{metric} (%) · whole recording"
                            )))
                        })
                        .when(active || !show_tma, |el| {
                            el.child(
                                icon(if !active {
                                    Icon::ArrowUpDown
                                } else if self.sort_desc {
                                    Icon::ArrowDown
                                } else {
                                    Icon::ArrowUp
                                })
                                .size(px(12.0)),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.sort_key == key {
                                this.sort_desc = !this.sort_desc;
                            } else {
                                this.sort_key = key;
                                this.sort_desc = key != SortKey::Function;
                            }
                            cx.notify();
                        }))
                });
        let scroll = self.hotspot_horizontal.clone();
        let function_header = header_columns.next();
        let header = div()
            .id("hotspot-columns")
            .flex()
            .flex_none()
            .h(px(37.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .children(function_header)
            .child(Self::hotspot_metrics(
                &scroll,
                metrics_min_width,
                div().children(header_columns),
            ));
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .when(self.is_computing(), |el| el.opacity(0.7))
            .child(
                div()
                    .h(px(45.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .gap(px(12.0))
                    .child(self.hotspot_input.clone())
                    .when(show_tma, |el| {
                        el.child(ui::section_caption("TMA · whole recording", cx))
                    }),
            );
        let render_table =
            |rows: gpui::AnyElement| body.child(header).child(rows).into_any_element();
        let Some(analysis) = self.analysis.latest().cloned() else {
            return render_table(
                empty_state(Icon::Table2, "Analyzing samples…").into_any_element(),
            );
        };
        let query = self.hotspot_input.read(cx).text().trim().to_lowercase();
        let mut rows: Vec<usize> = analysis
            .functions
            .iter()
            .enumerate()
            .filter(|(_, stat)| {
                let frame = &session.profile.frames[stat.frame_id];
                query.is_empty()
                    || frame.matches_symbol(&query)
                    || frame
                        .module
                        .as_ref()
                        .is_some_and(|module| module.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        let key = self.sort_key;
        let descending = self.sort_desc;
        let collator = icu_collator::Collator::try_new(Default::default(), Default::default())
            .expect("compiled Unicode collation data");
        rows.sort_by(|left, right| {
            let left = &analysis.functions[*left];
            let right = &analysis.functions[*right];
            let value = |stat: &crate::profile_analysis::FunctionStat| match key {
                SortKey::SelfPct => Some(stat.self_fraction),
                SortKey::TotalPct => Some(stat.inclusive_fraction),
                SortKey::Cycles => stat.metrics.cycles,
                SortKey::Instructions => stat.metrics.instructions,
                SortKey::Ipc => stat.metrics.ipc,
                SortKey::Tma(index) => {
                    let frame = &session.profile.frames[stat.frame_id];
                    tma?.function_values(frame)?.get(index).copied().flatten()
                }
                SortKey::Function => None,
            };
            let order = if key == SortKey::Function {
                let order = collator.compare(&left.label, &right.label);
                if descending { order.reverse() } else { order }
            } else {
                match (value(left), value(right)) {
                    (Some(left), Some(right)) => {
                        let order = left.total_cmp(&right);
                        if descending { order.reverse() } else { order }
                    }
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            };
            order.then_with(|| collator.compare(&left.label, &right.label))
        });
        if rows.is_empty() {
            return render_table(
                empty_state(
                    Icon::Search,
                    if session.total_samples == 0 {
                        "No stack samples recorded"
                    } else {
                        "No matching functions"
                    },
                )
                .into_any_element(),
            );
        }
        let selected = self.selected_frame;
        let entity = cx.entity();
        let session = session.clone();
        render_table(
            uniform_list("hotspots-rows", rows.len(), move |range, _, cx| {
                let theme = cx.theme().clone();
                range
                    .map(|index| {
                        let function = &analysis.functions[rows[index]];
                        let frame_id = function.frame_id;
                        let mut values = vec![
                            format!("{:.1}%", function.self_fraction * 100.0),
                            format!("{:.1}%", function.inclusive_fraction * 100.0),
                            function
                                .metrics
                                .cycles
                                .map(format_count)
                                .unwrap_or_else(|| "-".to_owned()),
                            function
                                .metrics
                                .instructions
                                .map(format_count)
                                .unwrap_or_else(|| "-".to_owned()),
                            function
                                .metrics
                                .ipc
                                .map(|ipc| format!("{ipc:.2}"))
                                .unwrap_or_else(|| "-".to_owned()),
                        ];
                        if let Some(tma) = session.tma.as_ref().filter(|_| show_tma) {
                            let shares = tma.function_values(&session.profile.frames[frame_id]);
                            values.extend(tma.level1_rows().enumerate().map(|(index, _)| {
                                shares
                                    .and_then(|shares| shares.get(index).copied().flatten())
                                    .map_or_else(
                                        || "-".to_owned(),
                                        |share| format!("{:.1}%", share * 100.0),
                                    )
                            }));
                        }
                        let target = entity.clone();
                        let frame = &session.profile.frames[frame_id];
                        div()
                            .id(frame_id)
                            .h(px(28.0))
                            .flex()
                            .items_center()
                            .w_full()
                            .border_b_1()
                            .border_color(theme.border.opacity(0.6))
                            .text_size(px(12.0))
                            .cursor_pointer()
                            .when(selected == Some(frame_id), |el| {
                                el.bg(theme.accent.opacity(0.6))
                            })
                            .hover(|el| el.bg(theme.muted.opacity(0.4)))
                            .child(
                                div()
                                    .id("hotspot-function")
                                    .w(function_width)
                                    .min_w(px(124.0))
                                    .max_w(px(400.0))
                                    .flex_none()
                                    .px(px(12.0))
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .font_weight(FontWeight::MEDIUM)
                                    .when(selected == Some(frame_id), |el| {
                                        el.text_color(theme.primary)
                                    })
                                    .child(ui::table_label(frame.friendly_name.clone()))
                                    .tooltip(ui::tooltip::text_tooltip(frame.name.clone())),
                            )
                            .child(Self::hotspot_metrics(
                                &scroll,
                                metrics_min_width,
                                div().children(values.into_iter().enumerate().map(
                                    |(index, value)| {
                                        div()
                                            .w(metric_width)
                                            .min_w(px(96.0))
                                            .flex_none()
                                            .px(px(metric_padding))
                                            .text_right()
                                            .whitespace_nowrap()
                                            .when(index == 0, |el| {
                                                el.font_weight(FontWeight::MEDIUM)
                                            })
                                            .child(value)
                                    },
                                )),
                            ))
                            .on_click(move |event, _, cx| {
                                target.update(cx, |this, cx| {
                                    if event.click_count() >= 2 {
                                        this.open_source_tab(frame_id, cx);
                                    } else {
                                        this.select_frame(Some(frame_id), cx);
                                    }
                                });
                            })
                    })
                    .collect()
            })
            .flex_1()
            .min_h(px(0.0))
            .map(|mut list| {
                list.style().restrict_scroll_to_axis = Some(true);
                list
            })
            .into_any_element(),
        )
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
                    .when_some(self.view_link(ViewId::Resources, cx), |card, link| {
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
                            self.render_summary_hotspot(session, &function, hotspots_tab, cx)
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
        session: &ShellSession,
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
                    .child(session.profile.frames[frame_id].friendly_name.clone()),
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
                        .child(hottest.friendly_name.clone()),
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
            symbol_query: self.flame_input.read(cx).text().to_owned(),
            selected_frame: self.selected_frame,
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
            .when_some(self.render_flame_path(cx), |el, path| el.child(path))
            .child(
                div()
                    .id("flame-scroll")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .bg(theme.background)
                    .when(self.analysis.stale(self.analysis_key()), |el| {
                        el.opacity(0.7)
                    })
                    .child(flame::flame_canvas(cx.entity(), theme.clone(), view))
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

        if !session.regions.spans.is_empty() {
            column = column.child(self.render_region_timeline(session, view.clone(), cx));
        }
        if process_tracks.is_empty() && system_tracks.is_empty() && session.regions.spans.is_empty()
        {
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
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .h(px(37.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div().flex().items_center().gap(px(2.0)).children(
                    [
                        (StackMode::TopDown, "Top-down"),
                        (StackMode::BottomUp, "Bottom-up"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (mode, label))| {
                        button(("flame-mode", index))
                            .label(label)
                            .size(ButtonSize::Xs)
                            .variant(if self.flame_stacks == mode {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.flame_stacks != mode {
                                    this.flame_stacks = mode;
                                    this.flame_zoom = None;
                                    this.flame_hover = None;
                                    this.ensure_derived(cx);
                                    cx.notify();
                                }
                            }))
                    }),
                ),
            )
            .child(
                div().flex().items_center().gap(px(2.0)).children(
                    [
                        (FlameWeight::Cycles, "Cycles"),
                        (FlameWeight::Instructions, "Instructions"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (weight, label))| {
                        button(("flame-weight", index))
                            .label(label)
                            .size(ButtonSize::Xs)
                            .disabled(
                                weight == FlameWeight::Instructions
                                    && session.instructions_metric.is_none(),
                            )
                            .variant(if self.flame_weight == weight {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.flame_weight != weight {
                                    this.flame_weight = weight;
                                    this.flame_zoom = None;
                                    this.flame_hover = None;
                                    this.ensure_derived(cx);
                                    cx.notify();
                                }
                            }))
                    }),
                ),
            )
            .child(div().flex_1())
            .child(self.flame_input.clone())
    }

    fn render_flame_path(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let layout = self.flame_layout.as_ref()?;
        let tree = self.flame.latest()?;
        let query = self.flame_input.read(cx).text().trim().to_lowercase();
        if query.is_empty() && self.flame_zoom.is_none() {
            return None;
        }
        let matches = |frame: &crate::profile_analysis::IcicleFrame| {
            frame
                .frame_id
                .and_then(|id| self.session.as_ref()?.profile.frames.get(id))
                .map_or_else(
                    || frame.label.to_lowercase().contains(&query),
                    |frame| {
                        frame.matches_symbol(&query)
                            || frame
                                .module
                                .as_ref()
                                .is_some_and(|module| module.to_lowercase().contains(&query))
                    },
                )
        };
        let matching: BTreeSet<usize> = layout
            .frames
            .iter()
            .filter(|frame| matches(frame))
            .map(|frame| frame.node_id)
            .collect();
        let weight: u64 = layout
            .frames
            .iter()
            .filter(|frame| matching.contains(&frame.node_id))
            .filter(|frame| {
                let mut parent = tree.nodes[frame.node_id].parent;
                while let Some(id) = parent {
                    if matching.contains(&id) {
                        return false;
                    }
                    parent = tree.nodes[id].parent;
                }
                true
            })
            .map(|frame| frame.inclusive_samples)
            .sum();
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(12.0))
                .px(px(8.0))
                .py(px(4.0))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(12.0))
                .when(self.flame_zoom.is_some(), |el| {
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .children(layout.focus_path.iter().enumerate().map(|(index, id)| {
                                let node = &tree.nodes[*id];
                                let target = *id;
                                let root = tree.root;
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .when(index > 0, |el| {
                                        el.child(
                                            icon(Icon::ChevronRight)
                                                .size(px(12.0))
                                                .color(theme.muted_foreground),
                                        )
                                    })
                                    .child(
                                        button(("flame-path", index))
                                            .label(node.label.clone())
                                            .variant(ButtonVariant::Ghost)
                                            .size(ButtonSize::Xs)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if target == root {
                                                    this.reset_flame_zoom(cx);
                                                } else {
                                                    this.zoom_flame(target, cx);
                                                }
                                            })),
                                    )
                            }))
                            .child(
                                button("flame-reset-path")
                                    .icon(Icon::RotateCcw)
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::IconXs)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reset_flame_zoom(cx)),
                                    ),
                            ),
                    )
                })
                .when(!query.is_empty(), |el| {
                    el.child(
                        div()
                            .ml_auto()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} matches · {:.1}%",
                                matching.len(),
                                weight as f64 / layout.total_samples.max(1) as f64 * 100.0
                            )),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_content(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(id) = self.workspace.active.as_ref() {
            if let Some(source_ix) = self.source_tabs.iter().position(|tab| &tab.id == id) {
                return self.render_source_tab(source_ix, cx);
            }
        } else {
            return div().into_any_element();
        }
        match self.active_view() {
            Some(ViewId::Summary) | None => self.render_summary(session, cx),
            Some(ViewId::Hotspots) => self.render_hotspots(session, cx),
            Some(ViewId::Flame) => self.render_flame(session, cx),
            Some(ViewId::FlameScope) => self.render_flame_scope(session, cx),
            Some(ViewId::Timeline) => self.render_timeline_view(session, cx),
            Some(ViewId::Events) => self.render_events(session, cx),
            Some(ViewId::Cores) => cores::render(self, session, cx),
            Some(ViewId::Resources) => resources::render(self, session, cx),
            Some(ViewId::ResourceUse) => resources::render_snapshot(session, cx),
            Some(ViewId::Structures) => structures::render(self, session, cx),
            Some(ViewId::Memory) => memory::render(session, cx),
            Some(ViewId::Allocations) => allocations::render(self, session, cx),
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
            .w(px(272.0))
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
                                        .child(
                                            session.profile.frames[function.frame_id]
                                                .friendly_name
                                                .clone(),
                                        ),
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
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(10.0))
                                .child(function.label.clone()),
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
                                this.open_source_tab(target, cx);
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
                                .child(
                                    session.profile.frames[relation.frame_id]
                                        .friendly_name
                                        .clone(),
                                ),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        self.wide_inspector = f32::from(window.viewport_size().width) >= 1280.0;

        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_symbol_filter))
            .on_action(cx.listener(Self::clear_stage))
            .on_action(cx.listener(Self::toggle_sidebar_action))
            .on_action(cx.listener(Self::toggle_theme_action))
            .on_action(cx.listener(Self::navigate_back_action))
            .on_action(cx.listener(Self::navigate_forward_action))
            .on_action(cx.listener(Self::close_document_action))
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
            .child(self.render_title_bar(window, cx));

        let root = match self.session.clone() {
            Some(session) => root.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .when(self.sidebar_open, |el| {
                        el.child(self.render_sidebar(&session, cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .child(self.render_tab_strip(cx))
                            .when(self.native_controls_open, |el| {
                                el.child(self.render_filter_bar(&session, cx))
                                    .child(self.render_timeline(&session, cx))
                            })
                            .child(
                                div().flex_1().min_h(px(0.0)).overflow_hidden().child(
                                    div()
                                        .flex()
                                        .size_full()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .h_full()
                                                .child(self.render_content(&session, cx)),
                                        )
                                        .when_some(
                                            self.selected_frame.filter(|_| {
                                                self.active_view() == Some(ViewId::Hotspots)
                                            }),
                                            |el, frame| {
                                                el.child(
                                                    self.render_function_inspector(
                                                        frame, &session, cx,
                                                    ),
                                                )
                                            },
                                        )
                                        .when(
                                            self.native_selection_open
                                                && self
                                                    .active_view()
                                                    .is_some_and(|view| view != ViewId::Hotspots),
                                            |el| {
                                                el.child(self.render_selection_panel(&session, cx))
                                            },
                                        ),
                                ),
                            )
                            .when(self.native_controls_open, |el| {
                                el.child(self.render_status_bar(&session, cx))
                            }),
                    ),
            ),
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
