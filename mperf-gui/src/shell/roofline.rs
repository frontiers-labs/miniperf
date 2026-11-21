//! Roofline view: loops plotted against the machine's calibrated ceilings,
//! a detail panel for the selected loop and the loop table underneath.

use std::sync::Arc;

use gpui::{
    Bounds, Context, CursorStyle, DispatchPhase, Entity, FontWeight, HitboxBehavior, Hsla,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Negate, PathBuilder, Pixels, Point,
    ScrollWheelEvent, TransformationMatrix, Window, canvas, div, fill, point, prelude::*, px,
    radians, size,
};

use super::ShellView;
use super::session::ShellSession;
use crate::charts::{shape_label, truncate_label};
use crate::roofline::{RooflineData, RooflineLoop, roofline_label_asset};
use crate::ui::{
    self, ActiveTheme, ButtonSize, ButtonVariant, Icon, Theme, badge, button, empty_state,
};

const PAD_LEFT: f32 = 57.0;
const PAD_RIGHT: f32 = 31.0;
const PAD_TOP: f32 = 18.0;
const PAD_BOTTOM: f32 = 44.0;
const LABEL_W: f32 = 176.0;
const LABEL_H: f32 = 18.0;
/// Zoom limits in decades of visible span, so the plot can neither collapse
/// onto one point nor drift into empty space.
const MIN_SPAN_LOG: f64 = 0.15;
const MAX_SPAN_LOG: f64 = 12.0;

/// Log-space window onto the plot. `None` on the view means "fit the loops".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    x: (f64, f64),
    y: (f64, f64),
}

/// An in-flight pan: where it started and the viewport it started from.
#[derive(Clone, Copy)]
pub struct Drag {
    origin: Point<Pixels>,
    viewport: Viewport,
}

impl Viewport {
    /// Zooms about a data-space anchor, keeping that point under the cursor.
    fn zoom(self, anchor: (f64, f64), factor: f64) -> Self {
        Self {
            x: zoom_axis(self.x, anchor.0, factor),
            y: zoom_axis(self.y, anchor.1, factor),
        }
    }

    /// Shifts the window by whole decades on each axis.
    fn pan(self, x_log: f64, y_log: f64) -> Self {
        Self {
            x: shift_axis(self.x, x_log),
            y: shift_axis(self.y, y_log),
        }
    }
}

fn zoom_axis(axis: (f64, f64), anchor: f64, factor: f64) -> (f64, f64) {
    let (min, max) = (axis.0.log10(), axis.1.log10());
    let anchor = anchor.max(1e-12).log10().clamp(min, max);
    let span = ((max - min) * factor).clamp(MIN_SPAN_LOG, MAX_SPAN_LOG);
    let ratio = if max > min {
        (anchor - min) / (max - min)
    } else {
        0.5
    };
    (
        10f64.powf(anchor - span * ratio),
        10f64.powf(anchor + span * (1.0 - ratio)),
    )
}

fn shift_axis(axis: (f64, f64), delta_log: f64) -> (f64, f64) {
    (
        10f64.powf(axis.0.log10() + delta_log),
        10f64.powf(axis.1.log10() + delta_log),
    )
}

/// One plotted loop in log-log space.
#[derive(Clone)]
struct Dot {
    index: usize,
    label: String,
    intensity: f64,
    gflops: f64,
}

/// Everything the canvas paints, plus the viewport that fits it all.
#[derive(Clone)]
struct Plot {
    dots: Vec<Dot>,
    /// All-core bandwidth ceilings as (label, GB/s), fastest first.
    roofs: Vec<(String, f64)>,
    compute: Option<f64>,
    /// The same ceilings measured on one thread, when the calibration has
    /// them; loops that ran on one thread are rated against these.
    single_roofs: Vec<(String, f64)>,
    single_compute: Option<f64>,
    compute_visible: bool,
    fit: Viewport,
}

/// Pixel mapping for one painted frame.
#[derive(Clone, Copy)]
struct Axes {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    view: Viewport,
}

impl Axes {
    fn new(bounds: Bounds<Pixels>, view: Viewport) -> Self {
        Self {
            left: f32::from(bounds.left()) + PAD_LEFT,
            top: f32::from(bounds.top()) + PAD_TOP,
            width: (f32::from(bounds.size.width) - PAD_LEFT - PAD_RIGHT).max(1.0),
            height: (f32::from(bounds.size.height) - PAD_TOP - PAD_BOTTOM).max(1.0),
            view,
        }
    }

    fn x_at(&self, value: f64) -> f32 {
        self.left + fraction(value, self.view.x) * self.width
    }

    fn y_at(&self, value: f64) -> f32 {
        self.top + (1.0 - fraction(value, self.view.y)) * self.height
    }

    fn x_of(&self, x: f32) -> f64 {
        value_at((x - self.left) / self.width, self.view.x)
    }

    fn y_of(&self, y: f32) -> f64 {
        value_at(1.0 - (y - self.top) / self.height, self.view.y)
    }

    fn x_span_log(&self) -> f64 {
        self.view.x.1.log10() - self.view.x.0.log10()
    }

    fn y_span_log(&self) -> f64 {
        self.view.y.1.log10() - self.view.y.0.log10()
    }

    fn contains(&self, intensity: f64, gflops: f64) -> bool {
        (self.view.x.0..=self.view.x.1).contains(&intensity)
            && (self.view.y.0..=self.view.y.1).contains(&gflops)
    }
}

fn fraction(value: f64, axis: (f64, f64)) -> f32 {
    let span = (axis.1.log10() - axis.0.log10()).max(1e-9);
    ((value.max(1e-12).log10() - axis.0.log10()) / span) as f32
}

fn value_at(fraction: f32, axis: (f64, f64)) -> f64 {
    let span = axis.1.log10() - axis.0.log10();
    10f64.powf(axis.0.log10() + fraction as f64 * span)
}

pub(super) struct ViewState {
    search: Entity<ui::TextInput>,
    sort: Sort,
    hidden_roofs: std::collections::BTreeSet<String>,
    chart_focus: gpui::FocusHandle,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Intensity,
    Throughput,
    Utilization,
}

#[derive(Clone, Copy)]
struct Sort {
    key: SortKey,
    descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Throughput,
            descending: true,
        }
    }
}

impl ViewState {
    pub(super) fn new(cx: &mut Context<ShellView>) -> Self {
        let search = cx.new(|cx| {
            ui::TextInput::new("roofline-search", "Search kernel or location…", cx)
                .leading_icon(Icon::Search)
                .height(32.)
                .text_size(12.)
                .width(256.)
        });
        let subscription = cx.subscribe(&search, |_, _, _: &ui::InputEvent, cx| cx.notify());
        Self {
            search,
            sort: Sort::default(),
            hidden_roofs: Default::default(),
            chart_focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    pub(super) fn reset(&mut self, cx: &mut Context<ShellView>) {
        self.search.update(cx, |input, cx| input.clear(cx));
        self.sort = Sort::default();
        self.hidden_roofs.clear();
    }
}

pub fn render(
    view: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(data) = session.roofline.as_ref() else {
        return empty_state(Icon::ChartLine, "No roofline loops in this recording")
            .into_any_element();
    };
    let Some(plot) = Plot::build(data) else {
        return empty_state(
            Icon::ChartLine,
            "No loop reached a plottable FP64 throughput point",
        )
        .into_any_element();
    };
    let query = view
        .roofline_ui
        .search
        .read(cx)
        .text()
        .trim()
        .to_lowercase();
    let mut order: Vec<_> = data
        .loops
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty()
                || format!(
                    "{} {} {}",
                    entry.function_name,
                    entry.friendly_name,
                    location(entry)
                )
                .to_lowercase()
                .contains(&query)
        })
        .map(|(index, _)| index)
        .collect();
    let sort = view.roofline_ui.sort;
    order.sort_by(|a, b| {
        let left = &data.loops[*a];
        let right = &data.loops[*b];
        let order = if sort.key == SortKey::Name {
            left.friendly_name.cmp(&right.friendly_name)
        } else {
            let values = match sort.key {
                SortKey::Intensity => (
                    left.fp64_arithmetic_intensity(),
                    right.fp64_arithmetic_intensity(),
                ),
                SortKey::Throughput => (left.fp64_gflops(), right.fp64_gflops()),
                SortKey::Utilization => (data.efficiency(left), data.efficiency(right)),
                SortKey::Name => unreachable!(),
            };
            match values {
                (Some(a), Some(b)) => a.total_cmp(&b),
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => return std::cmp::Ordering::Greater,
                (_, None) => return std::cmp::Ordering::Less,
            }
        };
        (if sort.descending {
            order.reverse()
        } else {
            order
        })
        .then_with(|| a.cmp(b))
    });
    let selected = view
        .roofline_loop
        .filter(|index| *index < data.loops.len())
        .or_else(|| plot.dots.first().map(|dot| dot.index));
    let viewport = view.roofline_view.unwrap_or(plot.fit);
    let mut drawing = plot.clone();
    drawing.dots.retain(|dot| order.contains(&dot.index));
    drawing
        .roofs
        .retain(|(label, _)| !view.roofline_ui.hidden_roofs.contains(label));
    drawing
        .single_roofs
        .retain(|(label, _)| !view.roofline_ui.hidden_roofs.contains(label));
    if view.roofline_ui.hidden_roofs.contains("FP64") {
        drawing.compute_visible = false;
    }
    let mut toolbar = div()
        .id("roofline-toolbar")
        .h(px(45.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(8.))
        .border_b_1()
        .border_color(theme.border)
        .child(view.roofline_ui.search.clone());
    for (index, label) in plot
        .roofs
        .iter()
        .map(|(label, _)| label.as_str())
        .chain(plot.compute.map(|_| "FP64"))
        .enumerate()
    {
        let label = label.to_owned();
        let hidden = view.roofline_ui.hidden_roofs.contains(&label);
        toolbar = toolbar.child(
            button(("roofline-roof", index))
                .label(label.clone())
                .size(ButtonSize::Sm)
                .variant(if hidden {
                    ButtonVariant::Ghost
                } else {
                    ButtonVariant::Outline
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.roofline_ui.hidden_roofs.remove(&label) {
                        this.roofline_ui.hidden_roofs.insert(label.clone());
                    }
                    cx.notify();
                })),
        );
    }
    toolbar = toolbar.child(
        button("roofline-reset")
            .label("Reset")
            .icon(Icon::RotateCcw)
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Ghost)
            .on_click(cx.listener(|this, _, _, cx| {
                this.roofline_ui.reset(cx);
                this.roofline_loop = None;
                this.roofline_view = None;
                this.roofline_drag = None;
                cx.notify();
            })),
    );
    div()
        .id("roofline")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .child(toolbar)
        .child(
            div()
                .id("roofline-chart-scroll")
                .flex_none()
                .min_w(px(0.))
                .overflow_x_scroll()
                .border_b_1()
                .border_color(theme.border)
                .px(px(8.))
                .pt(px(8.))
                .child(div().min_w(px(550.)).child(plot_canvas(
                    cx.entity(),
                    theme.clone(),
                    drawing,
                    viewport,
                    selected,
                    frame_ids(session, data),
                    view.roofline_ui.chart_focus.clone(),
                ))),
        )
        .child(detail_panel(session, data, selected, &theme, cx))
        .child(
            div()
                .id("roofline-table-scroll")
                .flex_none()
                .min_w(px(0.))
                .overflow_x_scroll()
                .child(loops_table(view, data, &order, selected, &theme, cx)),
        )
        .when(order.is_empty(), |el| {
            el.child(
                div()
                    .p(px(32.))
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("No matching kernels"),
            )
        })
        .into_any_element()
}
impl Plot {
    fn build(data: &RooflineData) -> Option<Self> {
        let dots: Vec<Dot> = data
            .loops
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                Some(Dot {
                    index,
                    label: entry.friendly_name.clone(),
                    intensity: entry.fp64_arithmetic_intensity()?,
                    gflops: entry.fp64_gflops()?,
                })
            })
            .collect();
        if dots.is_empty() {
            return None;
        }

        let roofs: Vec<(String, f64)> = data
            .bandwidth_roofs()
            .into_iter()
            .map(|(label, bandwidth)| (label.to_owned(), bandwidth))
            .collect();
        let compute = data
            .calibration
            .as_ref()
            .map(|calibration| calibration.fp64_gflops)
            .filter(|value| value.is_finite() && *value > 0.0);
        let (single_compute, single_roofs) = data
            .single_thread_roofs()
            .map(|(compute, roofs)| {
                (
                    Some(compute),
                    roofs
                        .into_iter()
                        .map(|(label, bandwidth)| (label.to_owned(), bandwidth))
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();

        // Fit the loops with breathing room, then stretch just far enough to
        // show every ridge point — otherwise a slow roof runs off the edge and
        // reads as if it never meets the compute ceiling.
        let intensities: Vec<f64> = dots.iter().map(|dot| dot.intensity).collect();
        let throughputs: Vec<f64> = dots.iter().map(|dot| dot.gflops).collect();
        let ridges_of = |compute: Option<f64>, roofs: &[(String, f64)]| -> Vec<f64> {
            compute
                .map(|compute| {
                    roofs
                        .iter()
                        .map(|(_, bandwidth)| compute / bandwidth)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut ridges = ridges_of(compute, &roofs);
        ridges.extend(ridges_of(single_compute, &single_roofs));
        let ceilings: Vec<f64> = compute.into_iter().chain(single_compute).collect();
        Some(Self {
            fit: Viewport {
                x: log_extent(&intensities, &ridges),
                y: log_extent(&throughputs, &ceilings),
            },
            dots,
            roofs,
            compute,
            single_roofs,
            single_compute,
            compute_visible: true,
        })
    }
}

/// Where a bandwidth roof is visible: it enters at the bottom of the plot
/// and ends at the compute ridge or the right edge, whichever comes first.
fn roof_segment(bandwidth: f64, compute: Option<f64>, view: Viewport) -> Option<(f64, f64)> {
    let start = view.x.0.max(view.y.0 / bandwidth);
    let mut end = view.x.1.min(view.y.1 / bandwidth);
    if let Some(compute) = compute {
        end = end.min(compute / bandwidth);
    }
    (start < end).then_some((start, end))
}

/// Log extent of `values` with a 3× margin, widened to cover `anchors` (the
/// calibrated ceilings) exactly, without padding them further.
fn log_extent(values: &[f64], anchors: &[f64]) -> (f64, f64) {
    let positive = |slice: &[f64]| -> Option<(f64, f64)> {
        slice
            .iter()
            .filter(|value| value.is_finite() && **value > 0.0)
            .fold(None, |extent: Option<(f64, f64)>, value| {
                Some(match extent {
                    Some((min, max)) => (min.min(*value), max.max(*value)),
                    None => (*value, *value),
                })
            })
    };
    let Some((min, max)) = positive(values) else {
        return positive(anchors).unwrap_or((0.1, 10.0));
    };
    let (mut min, mut max) = ((min / 3.0).max(1e-6), max * 3.0);
    if let Some((anchor_min, anchor_max)) = positive(anchors) {
        min = min.min(anchor_min);
        max = max.max(anchor_max);
    }
    (min, max)
}

fn frame_ids(session: &Arc<ShellSession>, data: &RooflineData) -> Vec<Option<usize>> {
    data.loops
        .iter()
        .map(|entry| frame_for(session, entry))
        .collect()
}

fn plot_canvas(
    entity: Entity<ShellView>,
    theme: Theme,
    plot: Plot,
    viewport: Viewport,
    selected: Option<usize>,
    frames: Vec<Option<usize>>,
    focus: gpui::FocusHandle,
) -> impl IntoElement {
    let indices: Vec<_> = plot.dots.iter().map(|dot| dot.index).collect();
    let keyboard_entity = entity.clone();
    div()
        .id("roofline-chart-focus")
        .track_focus(&focus)
        .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
        .on_key_down(move |event: &gpui::KeyDownEvent, _, cx| {
            if indices.is_empty() {
                return;
            }
            keyboard_entity.update(cx, |this, cx| {
                let current = this
                    .roofline_loop
                    .and_then(|selected| indices.iter().position(|index| *index == selected))
                    .unwrap_or(0);
                let next = match event.keystroke.key.as_str() {
                    "left" | "up" => current.saturating_sub(1),
                    "right" | "down" => (current + 1).min(indices.len() - 1),
                    "home" => 0,
                    "end" => indices.len() - 1,
                    "enter" | "space" => current,
                    _ => return,
                };
                this.roofline_loop = Some(indices[next]);
                cx.stop_propagation();
                cx.notify();
            });
        })
        .child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, cx| {
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                    window.paint_quad(fill(bounds, theme.background));

                    let axes = Axes::new(bounds, viewport);
                    paint_grid(&axes, &theme, window, cx);
                    paint_roofs(&axes, &plot, &theme, window, cx);
                    paint_dots(&axes, &plot, selected, &theme, window, cx);
                    register_mouse(&entity, &axes, &plot, &frames, &hitbox, window);
                },
            )
            .w_full()
            .map(|mut chart| {
                chart.style().aspect_ratio = Some(950.0 / 306.0);
                chart
            })
            .max_h(px(340.0)),
        )
}

fn paint_grid(axes: &Axes, theme: &Theme, window: &mut Window, cx: &mut gpui::App) {
    for tick in decade_ticks(axes.view.x) {
        let x = axes.x_at(tick);
        dashed_line(
            point(px(x), px(axes.top)),
            point(px(x), px(axes.top + axes.height)),
            theme.border,
            window,
        );
        let line = shape_label(&format_tick(tick), 10.0, theme.muted_foreground, window);
        let _ = line.paint(
            point(px(x) - line.width / 2., px(axes.top + axes.height + 9.)),
            px(12.),
            window,
            cx,
        );
    }
    for tick in decade_ticks(axes.view.y) {
        let y = axes.y_at(tick);
        dashed_line(
            point(px(axes.left), px(y)),
            point(px(axes.left + axes.width), px(y)),
            theme.border,
            window,
        );
        let line = shape_label(&format_tick(tick), 10.0, theme.muted_foreground, window);
        let _ = line.paint(
            point(px(axes.left - 9.) - line.width, px(y - 6.)),
            px(12.),
            window,
            cx,
        );
    }
    let line = shape_label(
        "Arithmetic intensity · FLOP / byte · log scale",
        11.,
        theme.muted_foreground,
        window,
    );
    let _ = line.paint(
        point(
            px(axes.left + axes.width / 2.) - line.width / 2.,
            px(axes.top + axes.height + 30.),
        ),
        px(13.),
        window,
        cx,
    );
    paint_rotated_label(
        "GFLOP / s",
        point(px(axes.left - 43.), px(axes.top + axes.height / 2.)),
        -std::f32::consts::FRAC_PI_2,
        theme.muted_foreground,
        window,
        cx,
    );
}
/// Bandwidth roofs rise at unit slope until the compute ceiling caps them.
/// Each segment is clipped in data space, so a roof that leaves the viewport
/// keeps its true slope instead of bending along the axis. The single-thread
/// ceilings are painted the same way, fainter and marked as such.
fn paint_roofs(axes: &Axes, plot: &Plot, theme: &Theme, window: &mut Window, cx: &mut gpui::App) {
    if plot.single_compute.is_some() {
        paint_roof_set(
            axes,
            &plot.single_roofs,
            plot.single_compute,
            plot.compute_visible,
            " · 1 thread",
            0.4,
            theme,
            window,
            cx,
        );
    }
    paint_roof_set(
        axes,
        &plot.roofs,
        plot.compute,
        plot.compute_visible,
        "",
        1.0,
        theme,
        window,
        cx,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_roof_set(
    axes: &Axes,
    roofs: &[(String, f64)],
    compute: Option<f64>,
    compute_visible: bool,
    suffix: &str,
    alpha: f32,
    theme: &Theme,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    for (index, (label, bandwidth)) in roofs.iter().enumerate() {
        let Some((start, end)) = roof_segment(*bandwidth, compute, axes.view) else {
            continue;
        };
        let color =
            theme.chart_series[(index + 1) % theme.chart_series.len()].opacity(alpha * 0.75);
        paint_line(
            point(px(axes.x_at(start)), px(axes.y_at(bandwidth * start))),
            point(px(axes.x_at(end)), px(axes.y_at(bandwidth * end))),
            color,
            window,
        );
        let at = 10f64.powf(
            start.log10() + (0.7 - index as f64 * 0.12).max(0.2) * (end.log10() - start.log10()),
        );
        let line = shape_label(
            &format!("{label} · {bandwidth:.0} GB/s{suffix}"),
            10.,
            theme.muted_foreground.opacity(alpha),
            window,
        );
        let _ = line.paint(
            point(px(axes.x_at(at)), px(axes.y_at(bandwidth * at) - 17.)),
            px(12.),
            window,
            cx,
        );
        if let Some(compute) = compute {
            let ridge = compute / bandwidth;
            if ridge >= axes.view.x.0
                && ridge < axes.view.x.1
                && (axes.view.y.0..=axes.view.y.1).contains(&compute)
            {
                paint_line(
                    point(px(axes.x_at(ridge)), px(axes.y_at(compute))),
                    point(px(axes.left + axes.width), px(axes.y_at(compute))),
                    color,
                    window,
                );
            }
        }
    }
    if compute_visible
        && let Some(compute) = compute
        && (axes.view.y.0..=axes.view.y.1).contains(&compute)
    {
        let y = axes.y_at(compute);
        dashed_line(
            point(px(axes.left), px(y)),
            point(px(axes.left + axes.width), px(y)),
            theme.muted_foreground.opacity(alpha),
            window,
        );
        let line = shape_label(
            &format!("FP64 · {compute:.0} GFLOP/s{suffix}"),
            10.,
            theme.muted_foreground.opacity(alpha),
            window,
        );
        let _ = line.paint(
            point(px(axes.left + axes.width - 6.) - line.width, px(y - 17.)),
            px(12.),
            window,
            cx,
        );
    }
}
fn paint_dots(
    axes: &Axes,
    plot: &Plot,
    selected: Option<usize>,
    theme: &Theme,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let mut placed: Vec<Bounds<Pixels>> = Vec::new();
    for dot in &plot.dots {
        if !axes.contains(dot.intensity, dot.gflops) {
            continue;
        }
        let center = point(px(axes.x_at(dot.intensity)), px(axes.y_at(dot.gflops)));
        let is_selected = selected == Some(dot.index);
        let radius = if is_selected { 6. } else { 4. };
        paint_dot(
            center,
            radius,
            if is_selected {
                theme.foreground
            } else {
                theme.chart_series[1]
            },
            theme.background,
            2.,
            window,
        );
        let line = shape_label(
            &truncate_label(&dot.label, 36),
            10.,
            theme.foreground,
            window,
        );
        let mut origin = point(center.x + px(radius + 4.), center.y - px(6.));
        if f32::from(origin.x + line.width) > axes.left + axes.width {
            origin.x = center.x - line.width - px(radius + 4.);
        }
        let box_ = Bounds::new(origin, size(line.width, px(12.)));
        if placed.iter().any(|other| other.intersects(&box_)) {
            continue;
        }
        placed.push(box_);
        let _ = line.paint(origin, px(12.), window, cx);
    }
}
/// Scroll zooms about the cursor, drag pans, click selects, double-click
/// opens the loop's source or resets the zoom when it lands on empty space.
fn register_mouse(
    entity: &Entity<ShellView>,
    axes: &Axes,
    plot: &Plot,
    frames: &[Option<usize>],
    hitbox: &gpui::Hitbox,
    window: &mut Window,
) {
    let positions: Vec<(usize, Point<Pixels>, f32)> = plot
        .dots
        .iter()
        .filter(|dot| axes.contains(dot.intensity, dot.gflops))
        .map(|dot| {
            (
                dot.index,
                point(px(axes.x_at(dot.intensity)), px(axes.y_at(dot.gflops))),
                6.0,
            )
        })
        .collect();

    let axes = *axes;
    let zoom_hitbox = hitbox.clone();
    let zoom_entity = entity.clone();
    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !zoom_hitbox.is_hovered(window) {
            return;
        }
        let delta = f32::from(event.delta.pixel_delta(px(16.0)).y);
        if delta == 0.0 {
            return;
        }
        let anchor = (
            axes.x_of(f32::from(event.position.x)),
            axes.y_of(f32::from(event.position.y)),
        );
        let factor = (-delta as f64 * 0.004).exp();
        zoom_entity.update(cx, |this, cx| {
            this.roofline_view = Some(axes.view.zoom(anchor, factor));
            cx.notify();
        });
        cx.stop_propagation();
    });

    let frames = frames.to_vec();
    let down_hitbox = hitbox.clone();
    let down_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble
            || event.button != MouseButton::Left
            || !down_hitbox.is_hovered(window)
        {
            return;
        }
        let hit = nearest(&positions, event.position);
        down_entity.update(cx, |this, cx| {
            match (event.click_count >= 2, hit) {
                (true, Some(index)) => {
                    let frame_id = frames.get(index).copied().flatten();
                    this.select_frame(frame_id, cx);
                    if let Some(frame_id) = frame_id {
                        this.open_source_tab(frame_id, cx);
                    }
                }
                (true, None) => this.roofline_view = None,
                (false, hit) => {
                    if let Some(index) = hit {
                        this.roofline_loop = Some(index);
                    }
                    this.roofline_drag = Some(Drag {
                        origin: event.position,
                        viewport: axes.view,
                    });
                }
            }
            cx.notify();
        });
    });

    let move_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        move_entity.update(cx, |this, cx| {
            let Some(drag) = this.roofline_drag else {
                return;
            };
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.abs() + dy.abs() < 2.0 {
                return;
            }
            this.roofline_view = Some(drag.viewport.pan(
                -dx as f64 / axes.width as f64 * axes.x_span_log(),
                dy as f64 / axes.height as f64 * axes.y_span_log(),
            ));
            cx.notify();
        });
    });

    let up_entity = entity.clone();
    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        up_entity.update(cx, |this, _| this.roofline_drag = None);
    });
}

fn nearest(positions: &[(usize, Point<Pixels>, f32)], at: Point<Pixels>) -> Option<usize> {
    positions
        .iter()
        .map(|(index, center, radius)| {
            let dx = f32::from(at.x - center.x);
            let dy = f32::from(at.y - center.y);
            (*index, (dx * dx + dy * dy).sqrt() - radius)
        })
        .filter(|(_, distance)| *distance <= 6.0)
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
}

fn dashed_line(from: Point<Pixels>, to: Point<Pixels>, color: Hsla, window: &mut Window) {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    let length = dx.hypot(dy).max(1.);
    let mut at = 0.;
    while at < length {
        let end = (at + 3.).min(length);
        paint_line(
            point(from.x + px(dx * at / length), from.y + px(dy * at / length)),
            point(
                from.x + px(dx * end / length),
                from.y + px(dy * end / length),
            ),
            color,
            window,
        );
        at += 7.;
    }
}

fn paint_line(from: Point<Pixels>, to: Point<Pixels>, color: Hsla, window: &mut Window) {
    let mut path = PathBuilder::stroke(px(1.2));
    path.move_to(from);
    path.line_to(to);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_dot(
    center: Point<Pixels>,
    radius: f32,
    background: Hsla,
    border: Hsla,
    border_width: f32,
    window: &mut Window,
) {
    let bounds = Bounds::new(
        point(center.x - px(radius), center.y - px(radius)),
        size(px(radius * 2.0), px(radius * 2.0)),
    );
    window.paint_quad(
        fill(bounds, background)
            .corner_radii(px(radius))
            .border_widths(px(border_width))
            .border_color(border),
    );
}

/// Canvas text cannot rotate, so the vertical axis label uses an SVG asset.
fn paint_rotated_label(
    text: &str,
    center: Point<Pixels>,
    angle: f32,
    color: Hsla,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let bounds = Bounds::new(
        point(center.x - px(LABEL_W / 2.0), center.y - px(LABEL_H / 2.0)),
        size(px(LABEL_W), px(LABEL_H)),
    );
    let pivot = bounds.center().scale(window.scale_factor());
    let transformation = TransformationMatrix::unit()
        .translate(pivot)
        .rotate(radians(angle))
        .translate(pivot.negate());
    let _ = window.paint_svg(
        bounds,
        roofline_label_asset(text).into(),
        transformation,
        color,
        cx,
    );
}

/// Powers of ten inside the extent, plus the 2× and 5× steps when the plot
/// spans fewer than two decades.
fn decade_ticks(extent: (f64, f64)) -> Vec<f64> {
    let (min, max) = extent;
    let dense = max / min.max(1e-12) < 100.0;
    let mut ticks = Vec::new();
    let mut decade = 10f64.powf(min.max(1e-12).log10().floor());
    while decade <= max && ticks.len() < 40 {
        for multiplier in if dense {
            [1.0, 2.0, 5.0].as_slice()
        } else {
            [1.0].as_slice()
        } {
            let tick = decade * multiplier;
            if tick >= min && tick <= max {
                ticks.push(tick);
            }
        }
        decade *= 10.0;
    }
    ticks
}

fn format_tick(value: f64) -> String {
    if value >= 1000.0 {
        format!("{}k", value / 1000.0)
    } else if value >= 1.0 {
        format!("{value:.0}")
    } else if value >= 0.01 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

/// The recording stores the collector's own quality tags; the view says what
/// they mean for the reader instead.
fn quality_label(quality: &str) -> &str {
    match quality {
        "high-confidence" => "high confidence",
        "low-confidence" => "low confidence",
        "insufficient-samples" => "insufficient data",
        "unclassified-instructions" => "unclassified instructions",
        other => other,
    }
}

fn quality_badge(entry: &RooflineLoop, theme: &Theme) -> Option<impl IntoElement + use<>> {
    let quality = entry.timing_quality.as_deref()?;
    let color = match quality {
        "high-confidence" => theme.viz.status_good,
        "low-confidence" => theme.viz.status_warn,
        _ => theme.viz.status_serious,
    };
    Some(badge(quality_label(quality).to_owned()).tint(color))
}

/// The profile frame this loop belongs to, when the recording sampled it —
/// without one there is nothing for a source tab to show.
fn frame_for(session: &Arc<ShellSession>, entry: &RooflineLoop) -> Option<usize> {
    let mut matches = session.profile.frames.iter().filter(|frame| {
        frame.name == entry.function_name
            && (entry.file_name.is_empty()
                || frame.file.as_deref() == Some(entry.file_name.as_str()))
    });
    let first = matches.next()?;
    if entry.line > 0 {
        if first.line == Some(entry.line as u32) {
            return Some(first.id);
        }
        if let Some(exact) = matches
            .clone()
            .find(|frame| frame.line == Some(entry.line as u32))
        {
            return Some(exact.id);
        }
    }
    matches.next().is_none().then_some(first.id)
}

fn location(entry: &RooflineLoop) -> String {
    match (entry.file_name.trim().is_empty(), entry.line > 0) {
        (false, true) => format!("{}:{}", entry.file_name, entry.line),
        (false, false) => entry.file_name.clone(),
        (true, _) => entry
            .module_offset
            .clone()
            .unwrap_or_else(|| "no source location".to_owned()),
    }
}

fn detail_panel(
    session: &Arc<ShellSession>,
    data: &RooflineData,
    selected: Option<usize>,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let Some(entry) = selected.and_then(|index| data.loops.get(index)) else {
        return div().into_any_element();
    };
    let metric = |label: &'static str, value: String| {
        div()
            .flex()
            .gap(px(4.))
            .child(div().text_color(theme.muted_foreground).child(label))
            .child(value)
    };
    let optional = |value: Option<f64>, suffix: &str| {
        value.map_or_else(|| "Unknown".into(), |value| format!("{value:.2}{suffix}"))
    };
    let mut panel = div()
        .flex_none()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(20.))
        .gap_y(px(4.))
        .px(px(12.))
        .py(px(8.))
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(12.))
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .font_weight(FontWeight::MEDIUM)
                .child(entry.friendly_name.clone()),
        )
        .child(metric(
            "Intensity",
            optional(entry.fp64_arithmetic_intensity(), " FLOP/B"),
        ))
        .child(metric(
            "Throughput",
            optional(entry.fp64_gflops(), " GFLOP/s"),
        ))
        .child(metric(
            "Roof utilization",
            data.efficiency(entry)
                .map_or_else(|| "Unknown".into(), |value| format!("{:.1}%", value * 100.)),
        ));
    if let Some(frame_id) = frame_for(session, entry) {
        panel = panel.child(
            button("roofline-open-source")
                .label(location(entry))
                .variant(ButtonVariant::Link)
                .size(ButtonSize::Xs)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_frame(Some(frame_id), cx);
                    this.open_source_tab(frame_id, cx);
                })),
        );
    } else {
        panel = panel.child(
            div()
                .text_color(theme.muted_foreground)
                .child(location(entry)),
        );
    }
    panel = panel
        .children(quality_badge(entry, theme))
        .when_some(entry.thread_count, |el, value| {
            el.child(metric("Threads", value.to_string()))
        })
        .when_some(entry.trip_count, |el, value| {
            el.child(metric("Trips", crate::snapshot::format_count(value as f64)))
        })
        .when_some(entry.timing_samples, |el, value| {
            el.child(metric(
                "Timing",
                format!(
                    "{value} samples{}",
                    entry
                        .timing_relative_error
                        .map(|error| format!(" · ±{:.1}%", error * 100.))
                        .unwrap_or_default()
                ),
            ))
        })
        .child(metric(
            "Vectorized",
            if entry.vector_double_ops.is_some_and(|ops| ops > 0.) {
                "yes"
            } else {
                "no"
            }
            .into(),
        ));
    panel.into_any_element()
}

fn loops_table(
    view: &ShellView,
    data: &RooflineData,
    order: &[usize],
    selected: Option<usize>,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let columns = [
        (SortKey::Name, "Kernel", 0.46),
        (SortKey::Intensity, "FLOP / byte", 0.18),
        (SortKey::Throughput, "GFLOP / s", 0.18),
        (SortKey::Utilization, "Roof utilization", 0.18),
    ];
    let cell = |index: usize| {
        div()
            .w(gpui::relative(columns[index].2))
            .min_w(px(0.))
            .px(px(12.))
            .truncate()
            .when(index > 0, |el| el.text_right())
    };
    div()
        .flex()
        .flex_col()
        .min_w(px(620.))
        .text_size(px(12.))
        .child(
            div()
                .flex()
                .h(px(37.))
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.muted.opacity(0.35))
                .text_color(theme.muted_foreground)
                .children(
                    columns
                        .iter()
                        .enumerate()
                        .map(|(index, (key, title, width))| {
                            let key = *key;
                            let active = view.roofline_ui.sort.key == key;
                            let icon = if !active {
                                Icon::ArrowUpDown
                            } else if view.roofline_ui.sort.descending {
                                Icon::ArrowDown
                            } else {
                                Icon::ArrowUp
                            };
                            div()
                                .id(("roofline-sort", index))
                                .w(gpui::relative(*width))
                                .h_full()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(12.))
                                .when(index > 0, |el| el.justify_end())
                                .text_color(if active {
                                    theme.foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .cursor_pointer()
                                .child(*title)
                                .child(icon.render(px(12.), theme.muted_foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let sort = &mut this.roofline_ui.sort;
                                    sort.descending = if sort.key == key {
                                        !sort.descending
                                    } else {
                                        key != SortKey::Name
                                    };
                                    sort.key = key;
                                    cx.notify();
                                }))
                        }),
                ),
        )
        .children(order.iter().map(|&index| {
            let entry = &data.loops[index];
            let optional = |value: Option<f64>| {
                value.map_or_else(|| "Unknown".into(), |value| format!("{value:.2}"))
            };
            ui::table_row(selected == Some(index), cx)
                .id(("roofline-loop", index))
                .when(!entry.file_name.is_empty(), |el| el.h(px(40.)))
                .child(
                    cell(0)
                        .font_family(theme.font_mono.clone())
                        .child(entry.friendly_name.clone())
                        .when(!entry.file_name.is_empty(), |el| {
                            el.child(
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(location(entry)),
                            )
                        }),
                )
                .child(cell(1).child(optional(entry.fp64_arithmetic_intensity())))
                .child(cell(2).child(optional(entry.fp64_gflops())))
                .child(
                    cell(3).child(
                        data.efficiency(entry).map_or_else(
                            || "Unknown".into(),
                            |value| format!("{:.1}%", value * 100.),
                        ),
                    ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.roofline_loop = Some(index);
                    cx.notify();
                }))
        }))
        .into_any_element()
}
