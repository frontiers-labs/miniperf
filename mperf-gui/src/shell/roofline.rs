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

const PLOT_H: f32 = 320.0;
const PAD_LEFT: f32 = 38.0;
const PAD_RIGHT: f32 = 12.0;
const PAD_TOP: f32 = 10.0;
const PAD_BOTTOM: f32 = 22.0;
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
    time_share: f64,
    vectorized: bool,
    confident: bool,
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

    /// Screen angle of a bandwidth roof, which is a unit slope in log space.
    fn roof_angle(&self) -> f32 {
        -((self.height as f64 * self.x_span_log()) / (self.width as f64 * self.y_span_log())).atan()
            as f32
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

pub fn render(
    view: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(data) = session.roofline.as_ref() else {
        return empty_state(Icon::Mountain, "No roofline loops in this recording")
            .into_any_element();
    };
    let Some(plot) = Plot::build(data) else {
        return empty_state(
            Icon::Mountain,
            "No loop reached a plottable FP64 throughput point",
        )
        .into_any_element();
    };

    let selected = view
        .roofline_loop
        .filter(|index| *index < data.loops.len())
        .or_else(|| plot.dots.first().map(|dot| dot.index));
    let viewport = view.roofline_view.unwrap_or(plot.fit);

    div()
        .id("roofline")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(4.0))
                        .p(px(8.0))
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.0))
                                .child(ui::section_caption(
                                    "FP64 roofline · calibrated on this machine",
                                    cx,
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(theme.muted_foreground)
                                                .child(
                                                    "dot size = share of run time · hollow = \
                                                     scalar · scroll to zoom, drag to pan",
                                                ),
                                        )
                                        .when(view.roofline_view.is_some(), |el| {
                                            el.child(
                                                button("roofline-reset-zoom")
                                                    .label("reset zoom")
                                                    .variant(ButtonVariant::Link)
                                                    .size(ButtonSize::Xs)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.roofline_view = None;
                                                        cx.notify();
                                                    })),
                                            )
                                        }),
                                ),
                        )
                        .child(plot_canvas(
                            cx.entity(),
                            theme.clone(),
                            plot.clone(),
                            viewport,
                            selected,
                            frame_ids(session, data),
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child("arithmetic intensity, FLOP/byte →")
                                .child("↑ GFLOP/s"),
                        ),
                )
                .child(detail_panel(session, data, &plot, selected, &theme, cx)),
        )
        .child(loops_table(data, &plot, selected, &theme, cx))
        .into_any_element()
}

impl Plot {
    fn build(data: &RooflineData) -> Option<Self> {
        let total_samples: u64 = data
            .loops
            .iter()
            .filter_map(|entry| entry.timing_samples)
            .sum();
        let dots: Vec<Dot> = data
            .loops
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                Some(Dot {
                    index,
                    label: entry.function_name.clone(),
                    intensity: entry.fp64_arithmetic_intensity()?,
                    gflops: entry.fp64_gflops()?,
                    time_share: entry.timing_samples.unwrap_or(0) as f64
                        / total_samples.max(1) as f64,
                    vectorized: entry.vector_double_ops.is_some_and(|ops| ops > 0.0),
                    confident: entry.timing_quality.as_deref() == Some("high-confidence"),
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
        })
    }

    /// The lowest ceiling at this intensity: bandwidth-bound on the slope,
    /// compute-bound past the ridge. Single-thread loops meet the
    /// single-thread ceilings.
    fn limit_at(&self, intensity: f64, single_thread: bool) -> Option<f64> {
        let (roofs, compute) = if single_thread && self.single_compute.is_some() {
            (&self.single_roofs, self.single_compute)
        } else {
            (&self.roofs, self.compute)
        };
        let bandwidth = roofs.first().map(|(_, roof)| roof * intensity);
        match (bandwidth, compute) {
            (Some(bandwidth), Some(compute)) => Some(bandwidth.min(compute)),
            (bandwidth, compute) => bandwidth.or(compute),
        }
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

fn dot_radius(share: f64) -> f32 {
    2.0 + (share.max(0.0).sqrt() * 5.0) as f32
}

/// Frame ids for the plotted loops, so a double-click can open source.
fn frame_ids(session: &Arc<ShellSession>, data: &RooflineData) -> Vec<Option<usize>> {
    data.loops
        .iter()
        .map(|entry| frame_for(session, &entry.function_name))
        .collect()
}

fn plot_canvas(
    entity: Entity<ShellView>,
    theme: Theme,
    plot: Plot,
    viewport: Viewport,
    selected: Option<usize>,
    frames: Vec<Option<usize>>,
) -> impl IntoElement {
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            window.paint_quad(fill(bounds, theme.viz.surface));

            let axes = Axes::new(bounds, viewport);
            paint_grid(&axes, &theme, window, cx);
            paint_roofs(&axes, &plot, &theme, window, cx);
            paint_dots(&axes, &plot, selected, &theme, window, cx);
            register_mouse(&entity, &axes, &plot, &frames, &hitbox, window);
        },
    )
    .w_full()
    .h(px(PLOT_H))
}

fn paint_grid(axes: &Axes, theme: &Theme, window: &mut Window, cx: &mut gpui::App) {
    for tick in decade_ticks(axes.view.x) {
        let x = axes.x_at(tick);
        window.paint_quad(fill(
            Bounds::new(point(px(x), px(axes.top)), size(px(1.0), px(axes.height))),
            theme.viz.grid.opacity(0.6),
        ));
        let line = shape_label(&format_tick(tick), 9.0, theme.viz.muted, window);
        let _ = line.paint(
            point(px(x - 8.0), px(axes.top + axes.height + 4.0)),
            px(10.0),
            window,
            cx,
        );
    }
    for tick in decade_ticks(axes.view.y) {
        let y = axes.y_at(tick);
        window.paint_quad(fill(
            Bounds::new(point(px(axes.left), px(y)), size(px(axes.width), px(1.0))),
            theme.viz.grid.opacity(0.6),
        ));
        let line = shape_label(&format_tick(tick), 9.0, theme.viz.muted, window);
        let _ = line.paint(
            point(px(axes.left - PAD_LEFT + 2.0), px(y - 5.0)),
            px(10.0),
            window,
            cx,
        );
    }
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
            " · 1 thread",
            0.4,
            theme,
            window,
            cx,
        );
    }
    paint_roof_set(axes, &plot.roofs, plot.compute, "", 1.0, theme, window, cx);
}

#[allow(clippy::too_many_arguments)]
fn paint_roof_set(
    axes: &Axes,
    roofs: &[(String, f64)],
    compute: Option<f64>,
    suffix: &str,
    alpha: f32,
    theme: &Theme,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let angle = axes.roof_angle();
    for (index, (label, bandwidth)) in roofs.iter().enumerate() {
        let Some((start, end)) = roof_segment(*bandwidth, compute, axes.view) else {
            continue;
        };
        let color = if index == 0 {
            theme.viz.axis.opacity(alpha)
        } else {
            theme.viz.axis.opacity(0.65 * alpha)
        };
        paint_line(
            point(px(axes.x_at(start)), px(axes.y_at(bandwidth * start))),
            point(px(axes.x_at(end)), px(axes.y_at(bandwidth * end))),
            color,
            window,
        );

        // Stagger the labels along their parallel roofs so the close cache
        // levels do not stack on top of each other.
        let along = (0.18 + index as f64 * 0.16).clamp(0.05, 0.85);
        let at = 10f64.powf(start.log10() + along * (end.log10() - start.log10()));
        paint_rotated_label(
            &format!("{label} {bandwidth:.0} GB/s{suffix}"),
            point(px(axes.x_at(at)), px(axes.y_at(bandwidth * at) - 10.0)),
            angle,
            theme.viz.muted.opacity(alpha),
            window,
            cx,
        );
    }

    if let Some(compute) = compute
        && (axes.view.y.0..=axes.view.y.1).contains(&compute)
    {
        let ridge = roofs
            .first()
            .map(|(_, bandwidth)| (compute / bandwidth).max(axes.view.x.0))
            .unwrap_or(axes.view.x.0);
        if ridge < axes.view.x.1 {
            paint_line(
                point(px(axes.x_at(ridge)), px(axes.y_at(compute))),
                point(px(axes.x_at(axes.view.x.1)), px(axes.y_at(compute))),
                theme.viz.axis.opacity(alpha),
                window,
            );
            let at = 10f64.powf(ridge.log10() + 0.7 * (axes.view.x.1.log10() - ridge.log10()));
            paint_rotated_label(
                &format!("FP64 peak {compute:.0} GFLOP/s{suffix}"),
                point(px(axes.x_at(at)), px(axes.y_at(compute) - 10.0)),
                0.0,
                theme.viz.muted.opacity(alpha),
                window,
                cx,
            );
        }
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
    // Loops that land on the same spot would otherwise print their labels on
    // top of each other; the first one placed wins.
    let mut placed: Vec<Bounds<Pixels>> = Vec::new();
    for dot in &plot.dots {
        if !axes.contains(dot.intensity, dot.gflops) {
            continue;
        }
        let center = point(px(axes.x_at(dot.intensity)), px(axes.y_at(dot.gflops)));
        let radius = dot_radius(dot.time_share);
        let is_selected = selected == Some(dot.index);
        let alpha = if dot.confident { 1.0 } else { 0.55 };
        paint_dot(
            center,
            radius,
            if dot.vectorized {
                theme.viz.series[0].opacity(alpha)
            } else {
                theme.viz.surface
            },
            if is_selected {
                theme.viz.ink
            } else {
                theme.viz.series[0].opacity(alpha)
            },
            if is_selected { 2.0 } else { 1.0 },
            window,
        );
        if dot.time_share >= 0.09 || is_selected {
            let line = shape_label(
                &truncate_label(&dot.label, 24),
                9.5,
                theme.viz.ink_2,
                window,
            );
            let origin = point(center.x + px(radius + 3.0), center.y - px(6.0));
            let box_ = Bounds::new(origin, size(line.width, px(11.0)));
            if placed.iter().any(|other| other.intersects(&box_)) {
                continue;
            }
            placed.push(box_);
            let _ = line.paint(origin, px(11.0), window, cx);
        }
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
                dot_radius(dot.time_share),
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

/// Canvas text cannot rotate, so roof labels go through the generated SVG
/// asset, turned to match the roof's on-screen slope.
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
fn frame_for(session: &Arc<ShellSession>, function: &str) -> Option<usize> {
    session
        .profile
        .frames
        .iter()
        .find(|frame| frame.name == function)
        .map(|frame| frame.id)
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
    plot: &Plot,
    selected: Option<usize>,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .w(px(320.0))
        .flex_none()
        .gap(px(4.0))
        .p(px(8.0))
        .text_size(px(11.0));

    let Some(entry) = selected.and_then(|index| data.loops.get(index)) else {
        return panel
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Click a loop dot."),
            )
            .into_any_element();
    };

    let intensity = entry.fp64_arithmetic_intensity();
    let gflops = entry.fp64_gflops();
    let single_thread = data.uses_single_thread_roofs(entry);
    let of_roof = intensity.zip(gflops).and_then(|(intensity, gflops)| {
        plot.limit_at(intensity, single_thread)
            .filter(|roof| *roof > 0.0)
            .map(|roof| {
                format!(
                    "{:.0}% of the {}roof at this intensity",
                    gflops / roof * 100.0,
                    if single_thread { "single-thread " } else { "" }
                )
            })
    });

    let row = |label: &'static str, value: String| {
        div()
            .flex()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(96.0))
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .child(div().flex_1().min_w(px(0.0)).child(value))
    };

    panel = panel
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(entry.function_name.clone()),
                )
                .children(quality_badge(entry, theme)),
        )
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(location(entry)),
        )
        .child(row(
            "Intensity",
            intensity
                .map(|value| format!("{value:.3} FLOP/byte"))
                .unwrap_or_else(|| "—".to_owned()),
        ))
        .child(row(
            "Throughput",
            match (gflops, of_roof) {
                (Some(gflops), Some(of_roof)) => format!("{gflops:.2} GFLOP/s · {of_roof}"),
                (Some(gflops), None) => format!("{gflops:.2} GFLOP/s"),
                _ => "—".to_owned(),
            },
        ))
        .child(row(
            "Vectorized",
            if entry.vector_double_ops.is_some_and(|ops| ops > 0.0) {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
        ))
        .when_some(entry.trip_count, |el, trips| {
            el.child(row("Trips", crate::snapshot::format_count(trips as f64)))
        })
        .when_some(entry.timing_samples, |el, samples| {
            let error = entry
                .timing_relative_error
                .map(|error| format!(" · ±{:.1}%", error * 100.0))
                .unwrap_or_default();
            el.child(row("Timing", format!("{samples} samples{error}")))
        });

    if let Some(frame_id) = frame_for(session, &entry.function_name) {
        panel = panel.child(
            button("roofline-open-source")
                .label("open source & disassembly →")
                .variant(ButtonVariant::Link)
                .size(ButtonSize::Sm)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_frame(Some(frame_id), cx);
                    this.open_source_tab(frame_id, cx);
                })),
        );
    }

    panel.into_any_element()
}

fn loops_table(
    data: &RooflineData,
    plot: &Plot,
    selected: Option<usize>,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let total_samples: u64 = data
        .loops
        .iter()
        .filter_map(|entry| entry.timing_samples)
        .sum();
    let mut order: Vec<usize> = (0..data.loops.len()).collect();
    order.sort_by(|left, right| {
        data.loops[*right]
            .timing_samples
            .unwrap_or(0)
            .cmp(&data.loops[*left].timing_samples.unwrap_or(0))
    });

    let cell = |width: Option<f32>, right: bool| {
        let cell = div()
            .px(px(8.0))
            .truncate()
            .when(right, |el| el.text_right());
        match width {
            Some(width) => cell.w(px(width)).flex_none(),
            None => cell.flex_1().min_w(px(0.0)),
        }
    };

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(ui::section_caption("loops · by run-time share", cx)),
        )
        .child(
            div()
                .flex()
                .h(px(24.0))
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(cell(None, false).child("Loop"))
                .child(cell(Some(220.0), false).child("Location"))
                .child(cell(Some(64.0), true).child("Time"))
                .child(cell(Some(56.0), true).child("Threads"))
                .child(cell(Some(88.0), true).child("AI"))
                .child(cell(Some(88.0), true).child("GFLOP/s"))
                .child(cell(Some(72.0), true).child("of roof"))
                .child(cell(Some(64.0), false).child("Vector"))
                .child(cell(Some(150.0), false).child("Confidence")),
        )
        .children(order.into_iter().map(|index| {
            let entry = &data.loops[index];
            let is_selected = selected == Some(index);
            let share = entry.timing_samples.unwrap_or(0) as f64 / total_samples.max(1) as f64;
            let intensity = entry.fp64_arithmetic_intensity();
            let gflops = entry.fp64_gflops();
            let single_thread = data.uses_single_thread_roofs(entry);
            let of_roof = intensity.zip(gflops).and_then(|(intensity, gflops)| {
                plot.limit_at(intensity, single_thread)
                    .filter(|roof| *roof > 0.0)
                    .map(|roof| format!("{:.0}%", gflops / roof * 100.0))
            });
            let optional = |value: Option<f64>, precision: usize| {
                value
                    .map(|value| format!("{value:.precision$}"))
                    .unwrap_or_else(|| "—".to_owned())
            };
            div()
                .id(("roofline-loop", index))
                .flex()
                .items_center()
                .h(px(24.0))
                .border_b_1()
                .border_color(theme.border.opacity(0.4))
                .text_size(px(11.0))
                .cursor_pointer()
                .when(is_selected, |el| el.bg(theme.accent))
                .when(!is_selected, |el| {
                    el.hover(|s| s.bg(theme.muted.opacity(0.5)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.roofline_loop = Some(index);
                    cx.notify();
                }))
                .child(
                    cell(None, false)
                        .font_family(theme.font_mono.clone())
                        .child(entry.function_name.clone()),
                )
                .child(
                    cell(Some(220.0), false)
                        .text_color(theme.muted_foreground)
                        .child(location(entry)),
                )
                .child(cell(Some(64.0), true).child(format!("{:.1}%", share * 100.0)))
                .child(
                    cell(Some(56.0), true).child(
                        entry
                            .thread_count
                            .map(|threads| threads.to_string())
                            .unwrap_or_else(|| "—".to_owned()),
                    ),
                )
                .child(cell(Some(88.0), true).child(optional(intensity, 3)))
                .child(cell(Some(88.0), true).child(optional(gflops, 2)))
                .child(cell(Some(72.0), true).child(of_roof.unwrap_or_else(|| "—".to_owned())))
                .child(cell(Some(64.0), false).child(
                    if entry.vector_double_ops.is_some_and(|ops| ops > 0.0) {
                        "yes"
                    } else {
                        "—"
                    },
                ))
                .child(
                    cell(Some(150.0), false)
                        .text_color(theme.muted_foreground)
                        .child(
                            entry
                                .timing_quality
                                .as_deref()
                                .map(quality_label)
                                .unwrap_or("—")
                                .to_owned(),
                        ),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        Viewport {
            x: (0.01, 10.0),
            y: (1.0, 1000.0),
        }
    }

    fn plot() -> Plot {
        Plot {
            dots: Vec::new(),
            roofs: vec![("DRAM".to_owned(), 31.0)],
            compute: Some(184.0),
            single_roofs: vec![("DRAM".to_owned(), 12.0)],
            single_compute: Some(46.0),
            fit: viewport(),
        }
    }

    fn axes(view: Viewport) -> Axes {
        Axes {
            left: 0.0,
            top: 0.0,
            width: 300.0,
            height: 300.0,
            view,
        }
    }

    #[test]
    fn a_roof_is_clipped_to_the_viewport_not_bent_along_the_axis() {
        let (start, end) = roof_segment(31.0, plot().compute, viewport()).unwrap();

        // Enters where it crosses the bottom of the plot, ends at the ridge.
        assert!((start - 1.0 / 31.0).abs() < 1e-9);
        assert!((end - 184.0 / 31.0).abs() < 1e-9);

        // Both endpoints sit inside the plot, so the drawn slope is the real
        // one: three x decades over three y decades on a square plot.
        let axes = axes(viewport());
        let run = axes.x_at(end) - axes.x_at(start);
        let rise = axes.y_at(31.0 * start) - axes.y_at(31.0 * end);
        assert!((rise / run - 1.0).abs() < 1e-4, "slope {}", rise / run);
    }

    /// The reported artifact: with L1/L2/L3 far above DRAM, the DRAM roof used
    /// to bend along the axis floor and never reach the compute ceiling.
    #[test]
    fn the_slowest_roof_still_reaches_the_compute_ridge_in_the_default_fit() {
        let dots = [0.08];
        let compute = 184.0;
        let roofs = [487.0, 366.0, 298.0, 31.0];
        let ridges: Vec<f64> = roofs.iter().map(|roof| compute / roof).collect();
        let fit = Viewport {
            x: log_extent(&dots, &ridges),
            y: log_extent(&[17.0], &[compute]),
        };
        let plot = Plot {
            dots: Vec::new(),
            roofs: vec![("DRAM".to_owned(), 31.0)],
            compute: Some(compute),
            single_roofs: Vec::new(),
            single_compute: None,
            fit,
        };

        let (start, end) = roof_segment(31.0, plot.compute, fit).unwrap();
        assert!(
            start > fit.x.0,
            "the roof enters where it crosses the floor"
        );
        assert!((end - compute / 31.0).abs() < 1e-9);
        assert!((end - fit.x.1).abs() < 1e-9, "the ridge is inside the plot");
    }

    #[test]
    fn a_roof_outside_the_viewport_is_dropped() {
        let view = Viewport {
            x: (0.01, 0.02),
            y: (1000.0, 2000.0),
        };
        assert!(roof_segment(31.0, plot().compute, view).is_none());
    }

    #[test]
    fn single_thread_loops_meet_the_single_thread_ceilings() {
        let plot = plot();
        assert_eq!(plot.limit_at(1.0, false), Some(31.0));
        assert_eq!(plot.limit_at(1.0, true), Some(12.0));
        assert_eq!(plot.limit_at(100.0, false), Some(184.0));
        assert_eq!(plot.limit_at(100.0, true), Some(46.0));
    }

    #[test]
    fn zoom_keeps_the_anchor_under_the_cursor() {
        let zoomed = viewport().zoom((1.0, 100.0), 0.5);
        assert!((fraction(1.0, zoomed.x) - fraction(1.0, viewport().x)).abs() < 1e-6);
        assert!((fraction(100.0, zoomed.y) - fraction(100.0, viewport().y)).abs() < 1e-6);
        assert!(zoomed.x.1.log10() - zoomed.x.0.log10() < 1.6);
    }

    #[test]
    fn zoom_stops_at_the_span_limits() {
        let mut view = viewport();
        for _ in 0..40 {
            view = view.zoom((1.0, 100.0), 0.5);
        }
        assert!((view.x.1.log10() - view.x.0.log10() - MIN_SPAN_LOG).abs() < 1e-9);

        let mut view = viewport();
        for _ in 0..40 {
            view = view.zoom((1.0, 100.0), 2.0);
        }
        assert!((view.y.1.log10() - view.y.0.log10() - MAX_SPAN_LOG).abs() < 1e-9);
    }

    #[test]
    fn pan_moves_the_window_without_changing_its_span() {
        let panned = viewport().pan(0.5, -0.25);
        assert!((panned.x.0.log10() - (viewport().x.0.log10() + 0.5)).abs() < 1e-9);
        assert!(
            (panned.y.1.log10()
                - panned.y.0.log10()
                - (viewport().y.1.log10() - viewport().y.0.log10()))
            .abs()
                < 1e-9
        );
    }
}
