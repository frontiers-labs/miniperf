//! Memory view: what the address trace says about traffic, locality and
//! footprint — stat tiles, bandwidth/residency over time, the miss-ratio
//! curve, stride and line-use histograms, and the working-set table.

use std::sync::Arc;

use gpui::{Context, FontWeight, Hsla, canvas, div, fill, prelude::*, px};

use super::ShellView;
use super::session::ShellSession;
use crate::charts::{PlotFrame, paint_area_series, shape_label};
use crate::memory::{MemoryData, MemorySummary};
use crate::snapshot::{format_bytes, format_count};
use crate::ui::{self, ActiveTheme, Icon, Theme, empty_state};

const TRACK_H: f32 = 56.0;
const CURVE_H: f32 = 120.0;
const BARS_H: f32 = 110.0;

pub fn render(session: &Arc<ShellSession>, cx: &mut Context<ShellView>) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(memory) = session.memory.as_ref() else {
        return empty_state(
            Icon::MemoryStick,
            "No memory observations in this recording",
        )
        .into_any_element();
    };

    div()
        .id("memory")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(tiles(memory, &theme)),
        )
        .when(!memory.timeline.is_empty(), |el| {
            el.child(timeline_card(memory, &theme))
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .when(!memory.miss_ratio.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w(px(300.0))
                            .child(miss_ratio_card(memory, &theme)),
                    )
                })
                .when(!memory.strides.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w(px(300.0))
                            .child(strides_card(memory, &theme)),
                    )
                })
                .when(!memory.spatial.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w(px(300.0))
                            .child(spatial_card(memory, &theme)),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .when(!memory.working_set.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w(px(300.0))
                            .child(working_set_card(memory, &theme)),
                    )
                })
                .when_some(memory.summary.as_ref(), |el, summary| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w(px(300.0))
                            .child(verdict_card(memory, summary, &theme)),
                    )
                }),
        )
        .into_any_element()
}

fn tiles(memory: &MemoryData, theme: &Theme) -> Vec<gpui::AnyElement> {
    let Some(summary) = memory.summary.as_ref() else {
        return Vec::new();
    };
    let mut tiles = Vec::new();

    if let Some(achieved) = summary.achieved_gbytes_per_second {
        let utilization = summary.bandwidth_utilization.unwrap_or(0.0);
        let sub = match summary.peak_gbytes_per_second {
            Some(peak) => format!("peak {peak:.1} GB/s · {:.0}% of roof", utilization * 100.0),
            None => format!("{:.0}% of roof", utilization * 100.0),
        };
        tiles.push(tile(
            "DRAM bandwidth",
            format!("{achieved:.1} GB/s avg"),
            Some(sub),
            Some(utilization as f32),
            theme,
        ));
    }
    if let Some(rss) = summary.peak_rss_bytes {
        let sub = summary
            .peak_allocated_bytes
            .map(|allocated| format!("live allocations peak {}", format_bytes(allocated as f64)));
        tiles.push(tile("Peak RSS", format_bytes(rss as f64), sub, None, theme));
    }
    tiles.push(tile(
        "Touched footprint",
        format_bytes(summary.accessed_footprint_bytes as f64),
        summary
            .cold_fraction
            .map(|cold| format!("{:.0}% of references are cold (first touch)", cold * 100.0)),
        None,
        theme,
    ));
    if let Some(utilization) = line_utilization(memory) {
        tiles.push(tile(
            "Cache-line use",
            format!("{:.0}%", utilization * 100.0),
            Some(format!(
                "of each {} B line actually read",
                summary.line_size
            )),
            Some(utilization as f32),
            theme,
        ));
    }
    tiles.push(tile(
        "References",
        format_count(summary.reference_count as f64),
        Some(format!(
            "{} loaded · {} stored",
            format_bytes(summary.architectural_load_bytes as f64),
            format_bytes(summary.architectural_store_bytes as f64)
        )),
        None,
        theme,
    ));

    tiles
}

fn tile(
    label: &'static str,
    value: String,
    sub: Option<String>,
    meter: Option<f32>,
    theme: &Theme,
) -> gpui::AnyElement {
    div()
        .flex_1()
        .min_w(px(170.0))
        .child(
            ui::viz_card(label)
                .child(
                    div()
                        .truncate()
                        .text_size(px(18.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(value),
                )
                .when_some(meter, |card, value| {
                    card.child(ui::meter(value).color(if value > 0.75 {
                        theme.viz.status_serious
                    } else {
                        theme.viz.series[0]
                    }))
                })
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

/// Mean fraction of a cache line the program actually reads, from the spatial
/// utilization histogram (buckets are percentages weighted by line count).
fn line_utilization(memory: &MemoryData) -> Option<f64> {
    let total: u64 = memory.spatial.iter().map(|point| point.count).sum();
    (total > 0).then(|| {
        memory
            .spatial
            .iter()
            .map(|point| point.bucket as f64 * point.count as f64)
            .sum::<f64>()
            / total as f64
            / 100.0
    })
}

fn timeline_card(memory: &MemoryData, theme: &Theme) -> gpui::AnyElement {
    let summary = memory.summary.as_ref();
    let note = summary.map(|summary| {
        format!(
            "source: {} · scope: {}",
            summary.bandwidth_source.replace('_', " "),
            summary.bandwidth_scope.replace('_', " ")
        )
    });

    let start = memory
        .timeline
        .first()
        .map(|point| point.timestamp_ns)
        .unwrap_or(0);
    let end = memory
        .timeline
        .last()
        .map(|point| point.timestamp_ns)
        .unwrap_or(start);
    let duration = (end.saturating_sub(start) as f64 / 1e9).max(1e-9);
    let seconds: Vec<f64> = memory
        .timeline
        .iter()
        .map(|point| point.timestamp_ns.saturating_sub(start) as f64 / 1e9)
        .collect();

    let tracks: Vec<(&'static str, Hsla, Vec<Option<f64>>, &'static str)> = vec![
        (
            "DRAM read",
            theme.viz.series[0],
            memory
                .timeline
                .iter()
                .map(|point| point.read_gbytes_per_second)
                .collect(),
            "GB/s",
        ),
        (
            "DRAM write",
            theme.viz.series[1],
            memory
                .timeline
                .iter()
                .map(|point| point.write_gbytes_per_second)
                .collect(),
            "GB/s",
        ),
        (
            "RSS",
            theme.viz.series[2],
            memory
                .timeline
                .iter()
                .map(|point| point.rss_bytes.map(|bytes| bytes as f64))
                .collect(),
            "bytes",
        ),
    ];

    let mut card = ui::viz_card("bandwidth & residency over time");
    if let Some(note) = note {
        card = card.action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(note),
        );
    }
    for (label, color, values, unit) in tracks {
        if !values.iter().any(Option::is_some) {
            continue;
        }
        card = card.child(track_canvas(
            label,
            unit,
            color,
            theme.clone(),
            seconds.clone(),
            values,
            duration,
        ));
    }
    card.into_any_element()
}

pub(super) fn track_canvas(
    label: &'static str,
    unit: &'static str,
    color: Hsla,
    theme: Theme,
    seconds: Vec<f64>,
    values: Vec<Option<f64>>,
    duration: f64,
) -> impl IntoElement {
    let max = values
        .iter()
        .flatten()
        .fold(0f64, |max, value| max.max(*value));
    canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            window.paint_quad(fill(bounds, theme.viz.surface));
            let frame = PlotFrame::new(bounds, 92.0);
            let top = frame.top();

            let line = shape_label(label, 10.0, theme.viz.ink_2, window);
            let _ = line.paint(point_at(bounds.left(), top + 4.0), px(12.0), window, cx);
            let peak = shape_label(
                &format!("peak {}", format_track_value(max, unit)),
                10.0,
                theme.viz.muted,
                window,
            );
            let _ = peak.paint(point_at(bounds.left(), top + 18.0), px(12.0), window, cx);

            let points: Vec<(f32, Option<f64>)> = seconds
                .iter()
                .zip(&values)
                .map(|(second, value)| (frame.x_for(*second, duration), *value))
                .collect();
            paint_area_series(top + 4.0, frame.height() - 8.0, &points, max, color, window);
        },
    )
    .flex_none()
    .w_full()
    .h(px(TRACK_H))
}

fn point_at(left: gpui::Pixels, y: f32) -> gpui::Point<gpui::Pixels> {
    gpui::point(left + px(6.0), px(y))
}

fn format_track_value(value: f64, unit: &str) -> String {
    match unit {
        "bytes" => format_bytes(value),
        _ => format!("{value:.1} {unit}"),
    }
}

/// Miss ratio against cache size, log2 on x — the curve that says whether the
/// working set is a capacity problem or a locality problem.
fn miss_ratio_card(memory: &MemoryData, theme: &Theme) -> gpui::AnyElement {
    let points: Vec<(f64, f64)> = memory
        .miss_ratio
        .iter()
        .filter(|point| point.cache_bytes > 0)
        .map(|point| ((point.cache_bytes as f64).log2(), point.miss_ratio))
        .collect();
    let marks: Vec<(f64, String)> = memory
        .calibration_levels
        .iter()
        .filter(|level| level.capacity_bytes > 0)
        .map(|level| {
            (
                (level.capacity_bytes as f64).log2(),
                level.level.to_uppercase(),
            )
        })
        .collect();
    let (first, last) = points
        .iter()
        .fold((f64::MAX, f64::MIN), |(first, last), point| {
            (first.min(point.0), last.max(point.0))
        });
    let axis = (
        format_bytes(2f64.powf(first)),
        format_bytes(2f64.powf(last)),
    );
    let theme = theme.clone();

    ui::viz_card("miss ratio vs cache size")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child("from the address trace"),
        )
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    window.paint_quad(fill(bounds, theme.viz.surface));
                    let frame = PlotFrame::new(bounds, 8.0);
                    let span = (last - first).max(1e-9);
                    let height = frame.height() - 18.0;
                    let x_for =
                        |value: f64| frame.left() + ((value - first) / span) as f32 * frame.width();

                    for (position, label) in &marks {
                        if *position < first || *position > last {
                            continue;
                        }
                        let x = x_for(*position);
                        window.paint_quad(fill(
                            gpui::Bounds::new(
                                gpui::point(px(x), px(frame.top())),
                                gpui::size(px(1.0), px(height)),
                            ),
                            theme.viz.grid,
                        ));
                        let line = shape_label(label, 9.0, theme.viz.muted, window);
                        let _ = line.paint(
                            gpui::point(px(x + 2.0), px(frame.top() + 2.0)),
                            px(10.0),
                            window,
                            cx,
                        );
                    }

                    let mapped: Vec<(f32, Option<f64>)> = points
                        .iter()
                        .map(|(position, ratio)| (x_for(*position), Some(*ratio)))
                        .collect();
                    paint_area_series(
                        frame.top(),
                        height,
                        &mapped,
                        1.0,
                        theme.viz.series[0],
                        window,
                    );

                    let low = shape_label(&axis.0, 9.0, theme.viz.muted, window);
                    let _ = low.paint(
                        gpui::point(px(frame.left()), px(frame.top() + height + 4.0)),
                        px(10.0),
                        window,
                        cx,
                    );
                    let high = shape_label(&axis.1, 9.0, theme.viz.muted, window);
                    let _ = high.paint(
                        gpui::point(
                            px(frame.left() + frame.width() - 44.0),
                            px(frame.top() + height + 4.0),
                        ),
                        px(10.0),
                        window,
                        cx,
                    );
                },
            )
            .w_full()
            .h(px(CURVE_H)),
        )
        .into_any_element()
}

fn strides_card(memory: &MemoryData, theme: &Theme) -> gpui::AnyElement {
    let total: u64 = memory.strides.iter().map(|point| point.count).sum();
    let far: f64 = memory
        .strides
        .iter()
        .filter(|point| point.bucket.abs() >= 6)
        .map(|point| point.count as f64)
        .sum::<f64>()
        / total.max(1) as f64;
    let bars = memory
        .strides
        .iter()
        .map(|point| {
            (
                stride_label(point.bucket),
                point.count as f64 / total.max(1) as f64,
                if point.bucket.abs() >= 6 {
                    theme.viz.series[1]
                } else {
                    theme.viz.series[0]
                },
            )
        })
        .collect();

    ui::viz_card("access strides")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child("lines between consecutive accesses"),
        )
        .child(bars_row(bars, theme))
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.muted_foreground)
                .child(format!(
                    "{:.0}% of accesses jump 64 lines or more — the gather path",
                    far * 100.0
                )),
        )
        .into_any_element()
}

fn spatial_card(memory: &MemoryData, theme: &Theme) -> gpui::AnyElement {
    let total: u64 = memory.spatial.iter().map(|point| point.count).sum();
    let bars = memory
        .spatial
        .iter()
        .map(|point| {
            (
                format!("{}%", point.bucket),
                point.count as f64 / total.max(1) as f64,
                theme.viz.series[0],
            )
        })
        .collect();

    ui::viz_card("cache-line use")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child("share of each fetched line that is read"),
        )
        .child(bars_row(bars, theme))
        .into_any_element()
}

fn bars_row(bars: Vec<(String, f64, Hsla)>, theme: &Theme) -> impl IntoElement + use<> {
    let peak = bars
        .iter()
        .map(|(_, share, _)| *share)
        .fold(0.0_f64, f64::max)
        .max(1e-9);
    div()
        .flex()
        .items_end()
        .gap(px(3.0))
        .h(px(BARS_H))
        .children(bars.into_iter().map(|(label, share, color)| {
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
                        .child(if share >= 0.01 {
                            format!("{:.0}%", share * 100.0)
                        } else {
                            String::new()
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .max_w(px(28.0))
                        .h(px(((share / peak) as f32 * 78.0).max(if share > 0.0 {
                            2.0
                        } else {
                            0.0
                        })))
                        .rounded_t(px(3.0))
                        .bg(color),
                )
                .child(
                    div()
                        .text_size(px(8.5))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
        }))
}

fn stride_label(stride: i64) -> String {
    match stride {
        0 => "same".to_owned(),
        stride if stride.abs() <= 5 => {
            format!(
                "{}{}L",
                if stride > 0 { "+" } else { "−" },
                1 << stride.abs()
            )
        }
        stride => format!("{}2^{}", if stride > 0 { "+" } else { "−" }, stride.abs()),
    }
}

fn working_set_card(memory: &MemoryData, theme: &Theme) -> gpui::AnyElement {
    let cell = |text: String, right: bool| {
        div()
            .flex_1()
            .min_w(px(0.0))
            .when(right, |el| el.text_right())
            .child(text)
    };

    ui::viz_card("working set by window")
        .action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child("bytes needed to cover a window of references"),
        )
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .pb(px(2.0))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(cell("Window (refs)".to_owned(), false))
                .child(cell("mean".to_owned(), true))
                .child(cell("p95".to_owned(), true))
                .child(cell("max".to_owned(), true)),
        )
        .children(memory.working_set.iter().map(|point| {
            div()
                .flex()
                .gap(px(8.0))
                .py(px(2.0))
                .text_size(px(11.0))
                .child(cell(format_count(point.window_references as f64), false))
                .child(cell(format_bytes(point.mean_bytes), true))
                .child(cell(format_bytes(point.p95_bytes as f64), true))
                .child(cell(format_bytes(point.max_bytes as f64), true))
        }))
        .into_any_element()
}

/// The one-paragraph read of the panels above, driven by the numbers rather
/// than by a canned story.
fn verdict_card(memory: &MemoryData, summary: &MemorySummary, theme: &Theme) -> gpui::AnyElement {
    let utilization = summary.bandwidth_utilization.unwrap_or(0.0);
    let line_use = line_utilization(memory).unwrap_or(1.0);
    let headline = if utilization >= 0.6 {
        "Bandwidth-limited."
    } else if line_use < 0.5 {
        "Locality-limited."
    } else {
        "No single memory bottleneck."
    };
    let modeled = summary
        .modeled_dram_read_bytes
        .saturating_add(summary.modeled_dram_write_bytes) as f64;
    let architectural = summary
        .architectural_load_bytes
        .saturating_add(summary.architectural_store_bytes) as f64;
    let amplification = (architectural > 0.0).then(|| modeled / architectural);

    ui::viz_card("verdict")
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.muted_foreground)
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(headline),
                )
                .child(format!(
                    "DRAM traffic reaches {:.0}% of the calibrated roof and each fetched line is \
                     {:.0}% used.",
                    utilization * 100.0,
                    line_use * 100.0
                ))
                .when_some(amplification, |el, amplification| {
                    el.child(format!(
                        "Modeled DRAM traffic is {amplification:.1}× the bytes the algorithm asks \
                         for."
                    ))
                })
                .when(!summary.quality.is_empty(), |el| {
                    el.child(format!("Analysis quality: {}.", summary.quality))
                }),
        )
        .into_any_element()
}
