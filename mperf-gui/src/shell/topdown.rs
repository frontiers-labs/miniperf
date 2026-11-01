//! Top-Down view: the vendor metric hierarchy, pipeline slots over time and
//! the per-function level-1 breakdown.

use std::sync::Arc;

use gpui::{CursorStyle, FontWeight, HitboxBehavior, canvas, div, fill, prelude::*, px};

use super::clock_badge;
use super::session::ShellSession;
use super::{ShellView, tma::TmaData, tma_category, tma_legend};
use crate::charts::{PlotFrame, paint_stacked_columns};
use crate::ui::{self, ActiveTheme, Icon, Theme, badge, empty_state, info_tooltip};

const INTERVALS_H: f32 = 96.0;
const AXIS_H: f32 = 16.0;

/// Level-1 rows as `(label, share, category)`, empty when the recording has no
/// TMA values.
pub fn level1(session: &ShellSession) -> Option<Vec<(String, f64, ui::TmaCategory)>> {
    let rows: Vec<_> = session
        .tma
        .as_ref()?
        .level1_rows()
        .filter_map(|row| Some((row.name.clone(), row.value?, tma_category(&row.name))))
        .collect();
    (!rows.is_empty()).then_some(rows)
}

pub fn render(
    view: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(tma) = session.tma.as_ref().filter(|tma| tma.has_hierarchy()) else {
        return empty_state(Icon::Layers, "No Top-Down metrics in this recording")
            .into_any_element();
    };

    div()
        .size_full()
        .flex()
        .overflow_hidden()
        .child(
            div()
                .id("topdown-hierarchy")
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .overflow_y_scroll()
                .border_r_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(8.0))
                        .py(px(6.0))
                        .child(ui::section_caption(
                            "top-down hierarchy · % of pipeline slots",
                            cx,
                        ))
                        .children(clock_badge(session, &theme)),
                )
                .when_some(tma.error.clone(), |el, error| {
                    el.child(
                        div()
                            .flex_none()
                            .px(px(8.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .text_color(theme.destructive)
                            .child(error),
                    )
                })
                .children(hierarchy_rows(tma, &theme, cx))
                .child(render_intervals(view, session, tma, cx)),
        )
        .child(render_functions(view, session, tma, cx))
        .into_any_element()
}

fn hierarchy_rows(
    tma: &TmaData,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> Vec<gpui::AnyElement> {
    let dominant = tma.dominant_path();
    tma.rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let value = row.value?;
            let category = tma_category(branch_of(tma, index));
            let on_path = dominant.contains(&index);
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(26.0))
                    .flex_none()
                    .pl(px((row.level.saturating_sub(1)) as f32 * 16.0 + 8.0))
                    .pr(px(8.0))
                    .border_b_1()
                    .border_color(theme.border.opacity(0.4))
                    .text_size(px(11.5))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .w(px(176.0))
                            .flex_none()
                            .when(on_path, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(div().truncate().min_w(px(0.0)).child(leaf_name(&row.name)))
                            .when(!row.description.is_empty(), |el| {
                                el.child(info_tooltip(
                                    ("tma-desc", index),
                                    row.description.clone(),
                                    cx,
                                ))
                            })
                            .when(on_path && row.level == 1, |el| {
                                el.child(badge("dominant").tint(theme.viz.status_serious))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h(px(10.0))
                            .rounded(px(2.0))
                            .bg(theme.viz.grid.opacity(0.5))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(value.clamp(0.0, 1.0) as f32))
                                    .rounded(px(2.0))
                                    .bg(theme.tma_color(category).opacity(if on_path {
                                        1.0
                                    } else {
                                        0.55
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w(px(48.0))
                            .flex_none()
                            .text_right()
                            .when(on_path, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(format!("{:.1}%", value * 100.0)),
                    )
                    .into_any_element(),
            )
        })
        .collect()
}

/// The level-1 ancestor of a row, which decides its color.
fn branch_of(tma: &TmaData, index: usize) -> &str {
    let name = tma.rows[index].name.as_str();
    name.split_once('.').map(|(root, _)| root).unwrap_or(name)
}

fn leaf_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).replace('_', " ")
}

/// Stacked level-1 shares per recorded interval, brushable like every other
/// time chart in the shell.
fn render_intervals(
    view: &ShellView,
    session: &Arc<ShellSession>,
    tma: &TmaData,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let legend: Vec<(String, f64, ui::TmaCategory)> = tma
        .level1_rows()
        .map(|row| (row.name.clone(), 0.0, tma_category(&row.name)))
        .collect();
    if tma.intervals.is_empty() {
        return div()
            .flex_none()
            .px(px(8.0))
            .py(px(8.0))
            .text_size(px(11.0))
            .text_color(theme.muted_foreground)
            .child("no per-interval TMA data in this recording")
            .into_any_element();
    }

    let colors: Vec<gpui::Hsla> = legend
        .iter()
        .map(|(_, _, category)| theme.tma_color(*category))
        .collect();
    let start_ns = session.full_range.map(|range| range.start_ns).unwrap_or(0);
    let duration = session.duration_seconds().max(1e-9);
    let mut columns: Vec<(f64, f64, Vec<f64>)> = Vec::with_capacity(tma.intervals.len());
    for (index, interval) in tma.intervals.iter().enumerate() {
        let start = interval.start_ns.saturating_sub(start_ns) as f64 / 1e9;
        let end = tma
            .intervals
            .get(index + 1)
            .map(|next| next.start_ns.saturating_sub(start_ns) as f64 / 1e9)
            .unwrap_or(duration);
        columns.push((start, end.max(start), interval.values.clone()));
    }

    let entity = cx.entity();
    let selection = view.selection_seconds();
    let preview = view.brush.preview;
    let chart = canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.set_cursor_style(CursorStyle::Crosshair, &hitbox);
            window.paint_quad(fill(bounds, theme.viz.surface));
            let frame = PlotFrame::new(bounds, 0.0);
            let top = frame.top() + 2.0;
            let height = INTERVALS_H - AXIS_H - 4.0;

            let pixels: Vec<(f32, f32, Vec<f64>)> = columns
                .iter()
                .map(|(start, end, values)| {
                    let x0 = frame.x_for(*start, duration);
                    let x1 = frame.x_for(*end, duration);
                    (x0, x1 - x0, values.clone())
                })
                .collect();
            paint_stacked_columns(top, height, &pixels, &colors, window);
            frame.paint_time_axis(top + height + 2.0, duration, &theme, window, cx);
            if let Some(range) = preview.or(selection) {
                frame.paint_selection(range, duration, top, height, &theme, window);
            }
            super::timeline::brush(&entity, frame, &hitbox, duration, window);
        },
    )
    .flex_none()
    .w_full()
    .h(px(INTERVALS_H));

    div()
        .flex()
        .flex_none()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(6.0))
                .child(ui::section_caption("pipeline slots over time", cx))
                .child(tma_legend(&legend, cx)),
        )
        .child(chart)
        .into_any_element()
}

fn render_functions(
    view: &ShellView,
    session: &Arc<ShellSession>,
    tma: &TmaData,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let colors: Vec<gpui::Hsla> = tma
        .level1_rows()
        .map(|row| theme.tma_color(tma_category(&row.name)))
        .collect();
    let legend: Vec<(String, f64, ui::TmaCategory)> = tma
        .level1_rows()
        .map(|row| (row.name.clone(), 0.0, tma_category(&row.name)))
        .collect();

    let rows: Vec<(usize, String, f64, Vec<f64>)> = view
        .analysis
        .latest()
        .map(|analysis| {
            analysis
                .functions
                .iter()
                .filter_map(|function| {
                    let shares = tma.functions.get(&function.label)?;
                    Some((
                        function.frame_id,
                        function.label.clone(),
                        function.self_fraction,
                        shares.clone(),
                    ))
                })
                .take(16)
                .collect()
        })
        .unwrap_or_default();

    div()
        .id("topdown-functions")
        .flex()
        .flex_col()
        .w(px(360.0))
        .flex_none()
        .h_full()
        .overflow_y_scroll()
        .child(
            div()
                .flex_none()
                .px(px(8.0))
                .py(px(6.0))
                .child(ui::section_caption(
                    "per-function breakdown · top hotspots",
                    cx,
                )),
        )
        .when(rows.is_empty(), |el| {
            el.child(
                div()
                    .px(px(8.0))
                    .text_size(px(11.0))
                    .text_color(theme.muted_foreground)
                    .child(
                        match session.tma.as_ref().map(|tma| tma.functions.is_empty()) {
                            Some(true) => "no per-function TMA table in this recording",
                            _ => "no hotspot matches the current filter",
                        },
                    ),
            )
        })
        .children(
            rows.into_iter()
                .map(|(frame_id, label, self_share, shares)| {
                    let selected = view.selected_frame == Some(frame_id);
                    let segments = shares
                        .iter()
                        .zip(&colors)
                        .map(|(share, color)| (*share as f32, *color))
                        .collect();
                    div()
                        .id(("tma-function", frame_id))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border.opacity(0.4))
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .when(selected, |el| el.bg(theme.accent))
                        .when(!selected, |el| el.hover(|s| s.bg(theme.muted.opacity(0.5))))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_frame(Some(frame_id), cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .child(label),
                        )
                        .child(
                            div()
                                .w(px(44.0))
                                .flex_none()
                                .text_right()
                                .text_color(theme.muted_foreground)
                                .child(format!("{:.1}%", self_share * 100.0)),
                        )
                        .child(
                            div()
                                .w(px(110.0))
                                .flex_none()
                                .child(ui::segment_bar(segments)),
                        )
                }),
        )
        .child(div().px(px(8.0)).py(px(8.0)).child(tma_legend(&legend, cx)))
        .into_any_element()
}
