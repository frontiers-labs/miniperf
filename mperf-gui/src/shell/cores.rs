//! Cores view: per-CPU occupancy lanes, the concurrency histogram and the
//! per-thread balance table.

use std::sync::Arc;

use gpui::{
    Bounds, Context, CursorStyle, Entity, FontWeight, HitboxBehavior, canvas, div, fill, point,
    prelude::*, px, size,
};

use super::ShellView;
use super::session::{ShellSession, format_duration_seconds};
use crate::charts::{PlotFrame, heat, shape_label, truncate_label};
use crate::profile::CpuObservationSource;
use crate::profile_analysis::{ConcurrencyHistogram, CpuUtilizationHeatmap, ThreadBalanceRow};
use crate::ui::{self, ActiveTheme, Icon, Theme, badge, empty_state};

const LANE_H: f32 = 18.0;
const AXIS_H: f32 = 16.0;
const GUTTER: f32 = 72.0;
const MAX_LANES: usize = 64;

/// Everything the lanes canvas needs for one frame.
#[derive(Clone)]
pub struct LanesView {
    pub heatmap: Arc<CpuUtilizationHeatmap>,
    pub duration: f64,
    pub selection: Option<(f64, f64)>,
    pub preview: Option<(f64, f64)>,
}

pub fn render(
    view: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(heatmap) = view.cpu_lanes.latest().cloned() else {
        return empty_state(
            Icon::Cpu,
            if view.cpu_lanes.is_computing() {
                "Measuring CPU occupancy…"
            } else {
                "No CPU occupancy in the current scope"
            },
        )
        .into_any_element();
    };

    let inferred = heatmap.source == CpuObservationSource::SampledOccupancy;
    let lanes = LanesView {
        heatmap: heatmap.clone(),
        duration: session.duration_seconds().max(1e-9),
        selection: view.selection_seconds(),
        preview: view.brush.preview,
    };
    let histogram = ConcurrencyHistogram::build(&heatmap, session.profile.logical_cpu_count);

    div()
        .id("cores")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .child(
            ui::viz_card(if heatmap.uses_cpu_lanes {
                "per-CPU occupancy · color = busy fraction"
            } else {
                "per-thread occupancy · color = busy fraction"
            })
            .action(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(inferred, |el| {
                        el.child(badge("inferred from samples").tint(theme.viz.status_warn))
                    })
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.muted_foreground)
                            .child("drag to filter time"),
                    ),
            )
            .child(lanes_canvas(cx.entity(), theme.clone(), lanes)),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(320.0))
                        .child(concurrency_card(&histogram, &theme)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(320.0))
                        .child(balance_card(view, session, &theme)),
                ),
        )
        .into_any_element()
}

/// Occupancy lanes: one row per CPU (or thread), colored by busy fraction,
/// sharing the master timeline's brush.
fn lanes_canvas(entity: Entity<ShellView>, theme: Theme, view: LanesView) -> impl IntoElement {
    let rows = view.heatmap.lanes.len().min(MAX_LANES);
    let hidden = view.heatmap.lanes.len() - rows;
    let height = (rows + usize::from(hidden > 0)) as f32 * LANE_H + AXIS_H + 4.0;
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.set_cursor_style(CursorStyle::Crosshair, &hitbox);
            window.paint_quad(fill(bounds, theme.viz.surface));

            let frame = PlotFrame::new(bounds, GUTTER);
            let buckets = view.heatmap.buckets.max(1);
            let bar_w = frame.width() / buckets as f32;
            for (row, lane) in view.heatmap.lanes.iter().take(rows).enumerate() {
                let y = frame.top() + row as f32 * LANE_H;
                let average = lane
                    .buckets
                    .iter()
                    .map(|bucket| bucket.utilization)
                    .sum::<f64>()
                    / buckets as f64;
                let label = shape_label(
                    &truncate_label(&lane.label, 9),
                    10.0,
                    theme.viz.ink_2,
                    window,
                );
                let _ = label.paint(
                    point(bounds.left() + px(6.0), px(y + 3.0)),
                    px(12.0),
                    window,
                    cx,
                );
                let share = shape_label(
                    &format!("{:.0}%", average * 100.0),
                    10.0,
                    theme.viz.muted,
                    window,
                );
                let _ = share.paint(
                    point(bounds.left() + px(46.0), px(y + 3.0)),
                    px(12.0),
                    window,
                    cx,
                );

                for (bucket, value) in lane.buckets.iter().enumerate() {
                    if value.utilization <= 0.01 {
                        continue;
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            point(px(frame.left() + bucket as f32 * bar_w), px(y + 2.0)),
                            size(px(bar_w.ceil().max(1.0)), px(LANE_H - 4.0)),
                        ),
                        heat(value.utilization as f32),
                    ));
                }
            }

            let mut lanes_bottom = rows as f32 * LANE_H;
            if hidden > 0 {
                let note = shape_label(
                    &format!("+{hidden} more CPUs not shown"),
                    9.0,
                    theme.viz.muted,
                    window,
                );
                let _ = note.paint(
                    point(
                        bounds.left() + px(6.0),
                        px(frame.top() + lanes_bottom + 3.0),
                    ),
                    px(12.0),
                    window,
                    cx,
                );
                lanes_bottom += LANE_H;
            }
            let axis_y = frame.top() + lanes_bottom + 2.0;
            frame.paint_time_axis(axis_y, view.duration, &theme, window, cx);
            if let Some(range) = view.preview.or(view.selection) {
                frame.paint_selection(
                    range,
                    view.duration,
                    frame.top(),
                    lanes_bottom,
                    &theme,
                    window,
                );
            }
            super::timeline::brush(&entity, frame, &hitbox, view.duration, window);
        },
    )
    .flex_none()
    .w_full()
    .h(px(height))
}

fn concurrency_card(histogram: &ConcurrencyHistogram, theme: &Theme) -> gpui::AnyElement {
    let cpus = histogram.slots.len().saturating_sub(1);
    let peak = histogram
        .slots
        .iter()
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1e-9);
    let serial: f64 = histogram.slots.iter().skip(1).take(2).sum();

    ui::viz_card("concurrency histogram")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(format!(
                    "avg {:.1} of {cpus} CPUs busy over {}",
                    histogram.average_busy,
                    format_duration_seconds(histogram.total_seconds)
                )),
        )
        .child(div().flex().items_end().gap(px(4.0)).h(px(120.0)).children(
            histogram.slots.iter().enumerate().map(|(busy, seconds)| {
                let share = (seconds / peak) as f32;
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.muted_foreground)
                            .child(if *seconds > 0.0 {
                                format_duration_seconds(*seconds)
                            } else {
                                String::new()
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .max_w(px(36.0))
                            .h(px((share * 92.0).max(if *seconds > 0.0 {
                                2.0
                            } else {
                                0.0
                            })))
                            .rounded_t(px(3.0))
                            .bg(match busy {
                                0 => theme.viz.grid,
                                1..=2 => theme.viz.series[1],
                                _ => theme.viz.series[0],
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(theme.muted_foreground)
                            .child(busy.to_string()),
                    )
            }),
        ))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child("simultaneously busy CPUs → elapsed time at that level"),
        )
        .when(cpus > 2 && serial > 0.0, |card| {
            let wasted = serial * (cpus as f64 - 1.5);
            card.child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} in the 1–2 core band — about {} of {cpus}-core capacity idle there",
                        format_duration_seconds(serial),
                        format_duration_seconds(wasted)
                    )),
            )
        })
        .into_any_element()
}

fn balance_card(view: &ShellView, session: &Arc<ShellSession>, theme: &Theme) -> gpui::AnyElement {
    let Some(balance) = view.balance.latest() else {
        return ui::viz_card("thread balance")
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted_foreground)
                    .child(if view.balance.is_computing() {
                        "measuring…"
                    } else {
                        "no threads in the current scope"
                    }),
            )
            .into_any_element();
    };

    let mut rows: Vec<&ThreadBalanceRow> = balance.rows.iter().collect();
    rows.sort_by(|left, right| right.sync_fraction.total_cmp(&left.sync_fraction));
    let worst_sync = rows
        .first()
        .map(|row| row.sync_fraction)
        .unwrap_or(0.0)
        .max(1e-9);
    let label_for = |thread_id: u32| {
        session
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .map(|thread| thread.label.clone())
            .unwrap_or_else(|| format!("tid {thread_id}"))
    };

    let header = |label: &'static str, width: Option<f32>, right: bool| {
        let cell = div()
            .text_size(px(10.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.muted_foreground)
            .when(right, |el| el.text_right())
            .child(label);
        match width {
            Some(width) => cell.w(px(width)).flex_none(),
            None => cell.flex_1().min_w(px(0.0)),
        }
    };

    ui::viz_card("thread balance")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(if balance.has_cpu_ids {
                    "busy · waiting at sync · CPU migrations"
                } else {
                    "busy · waiting at sync (no CPU ids recorded)"
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .pb(px(2.0))
                .border_b_1()
                .border_color(theme.border)
                .child(header("Thread", None, false))
                .child(header("Busy", Some(48.0), true))
                .child(header("In sync", Some(56.0), true))
                .child(header("", Some(90.0), false))
                .child(header("Migrations", Some(70.0), true)),
        )
        .children(rows.into_iter().map(|row| {
            let worst = row.sync_fraction >= worst_sync && row.sync_fraction > 0.08;
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .py(px(3.0))
                .text_size(px(11.0))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w(px(0.0))
                        .items_center()
                        .gap(px(4.0))
                        .child(div().truncate().child(label_for(row.thread_id)))
                        .when(worst, |el| {
                            el.child(badge("waits most").tint(theme.viz.status_serious))
                        }),
                )
                .child(
                    div()
                        .w(px(48.0))
                        .flex_none()
                        .text_right()
                        .child(format!("{:.0}%", row.busy_fraction * 100.0)),
                )
                .child(
                    div()
                        .w(px(56.0))
                        .flex_none()
                        .text_right()
                        .child(format!("{:.1}%", row.sync_fraction * 100.0)),
                )
                .child(div().w(px(90.0)).flex_none().child(
                    ui::meter((row.sync_fraction / worst_sync) as f32).color(if worst {
                        theme.viz.status_serious
                    } else {
                        theme.viz.series[0]
                    }),
                ))
                .child(
                    div()
                        .w(px(70.0))
                        .flex_none()
                        .text_right()
                        .text_color(theme.muted_foreground)
                        .child(if balance.has_cpu_ids {
                            row.migrations.to_string()
                        } else {
                            "—".to_owned()
                        }),
                )
        }))
        .into_any_element()
}
