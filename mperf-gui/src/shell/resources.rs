//! Recorded counters and individual thread samples on one shared time axis.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use gpui::{
    App, Bounds, Context, CursorStyle, DispatchPhase, Entity, FocusHandle, FontWeight,
    HitboxBehavior, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder,
    Pixels, SharedString, Window, canvas, div, fill, point, prelude::*, px, size,
};

use super::ShellView;
use super::session::ShellSession;
use crate::charts::{paint_area_series, shape_label, truncate_label};
use crate::snapshot::{
    ResourceUse, Severity, SnapshotChart, SnapshotData, SnapshotFinding, SummaryMetric, UseCategory,
};
use crate::ui::{
    self, ActiveTheme, ButtonSize, ButtonVariant, Icon, Theme, badge, button, empty_state,
};

const SPARK_H: f32 = 32.0;
const GUTTER: f32 = 176.0;
const RULER_H: f32 = 36.0;
const GROUP_H: f32 = 32.0;
const COUNTER_H: f32 = 57.0;
const THREAD_H: f32 = 37.0;

fn time_range(session: &ShellSession) -> Option<crate::profile::TimeRange> {
    session
        .resource_tracks
        .as_ref()
        .map(|tracks| tracks.range)
        .or(session.full_range)
}

fn selection_seconds(shell: &ShellView) -> Option<(f64, f64)> {
    let origin = time_range(shell.session.as_ref()?)?.start_ns;
    let range = shell.filter.time?;
    Some((
        range.start_ns.saturating_sub(origin) as f64 / 1e9,
        range.end_ns.saturating_sub(origin) as f64 / 1e9,
    ))
}

fn commit_range(shell: &mut ShellView, range: Option<(f64, f64)>, cx: &mut Context<ShellView>) {
    let Some(session) = &shell.session else {
        return;
    };
    let origin = time_range(session).map_or(0, |range| range.start_ns);
    let full_origin = session.full_range.map_or(origin, |range| range.start_ns);
    let delta = (origin as i128 - full_origin as i128) as f64 / 1e9;
    shell.commit_time_filter(range.map(|(start, end)| (start + delta, end + delta)), cx);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    Counters,
    Threads,
}

#[derive(Clone, Copy)]
struct Drag {
    start: f64,
    x: f32,
    sample: Option<usize>,
    preview: Option<(f64, f64)>,
}

/// Local navigation and exact sample identity. Time selections use the shell's
/// half-open global filter so every native analysis observes the same range.
#[derive(Default)]
pub struct TimelineState {
    viewport: Option<(f64, f64)>,
    collapsed: BTreeSet<Group>,
    hover: Option<f64>,
    selected_sample: Option<usize>,
    drag: Option<Drag>,
    focus: Option<FocusHandle>,
}

impl TimelineState {
    fn viewport(&self, duration: f64) -> (f64, f64) {
        self.viewport.unwrap_or((0.0, duration))
    }

    fn fit(&mut self, start: f64, width: f64, duration: f64) {
        let width = width.clamp(duration.min(1e-6), duration);
        let start = start.clamp(0.0, duration - width);
        self.viewport = Some((start, start + width));
    }

    fn pan(&mut self, direction: f64, duration: f64) {
        let (start, end) = self.viewport(duration);
        self.fit(
            start + (end - start) * direction * 0.2,
            end - start,
            duration,
        );
    }

    fn zoom(&mut self, factor: f64, duration: f64, selection: Option<(f64, f64)>) {
        let (start, end) = self.viewport(duration);
        let (left, right) = selection.unwrap_or((start, end));
        let width = (end - start) * factor;
        self.fit((left + right - width) / 2.0, width, duration);
    }
}

#[derive(Clone, Copy)]
enum CounterSource {
    Samples,
    Tma(usize),
    Sum {
        track: usize,
        metric: usize,
    },
    Ipc {
        track: usize,
        cycles: usize,
        instructions: usize,
    },
}

struct CounterRow {
    label: SharedString,
    source: CounterSource,
    color: Hsla,
}

impl CounterRow {
    fn bins(&self, session: &ShellSession) -> Vec<(u64, u64, Option<f64>)> {
        if let CounterSource::Tma(index) = self.source {
            return session.tma.as_ref().map_or_else(Vec::new, |tma| {
                tma.intervals
                    .iter()
                    .map(|interval| (interval.start_ns, interval.end_ns, interval.values[index]))
                    .collect()
            });
        }
        let Some(tracks) = &session.resource_tracks else {
            return Vec::new();
        };
        let values: Vec<_> = match self.source {
            CounterSource::Samples => tracks
                .sample_counts
                .iter()
                .map(|count| Some(*count as f64))
                .collect(),
            CounterSource::Sum { track, .. } | CounterSource::Ipc { track, .. } => {
                tracks.tracks[track].values.clone()
            }
            CounterSource::Tma(_) => unreachable!(),
        };
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let start = tracks
                    .range
                    .start_ns
                    .saturating_add(index as u64 * tracks.bin_width_ns);
                let end = start
                    .saturating_add(tracks.bin_width_ns)
                    .min(tracks.range.end_ns);
                (start, end, value)
            })
            .collect()
    }

    fn value(
        &self,
        session: &ShellSession,
        selection: Option<(f64, f64)>,
        time: f64,
    ) -> Option<f64> {
        let timestamp = time_range(session)?
            .start_ns
            .saturating_add((time * 1e9) as u64);
        if let CounterSource::Tma(index) = self.source {
            let tma = session.tma.as_ref()?;
            if tma.intervals.is_empty() {
                return tma.rows[tma.level1[index]].value;
            }
            let position = tma
                .intervals
                .partition_point(|interval| interval.start_ns <= timestamp);
            let interval = tma.intervals.get(position.checked_sub(1)?)?;
            return (timestamp < interval.end_ns)
                .then_some(interval.values[index])
                .flatten();
        }
        let tracks = session.resource_tracks.as_ref()?;
        if let Some((start, end)) = selection {
            let origin = time_range(session)?.start_ns;
            let start_ns = origin.saturating_add((start * 1e9) as u64);
            let end_ns = origin.saturating_add((end * 1e9) as u64);
            let samples =
                session.profile.samples.iter().filter(|sample| {
                    sample.timestamp_ns >= start_ns && sample.timestamp_ns < end_ns
                });
            let sum = |metric: usize| {
                let mut values = samples
                    .clone()
                    .filter_map(|sample| sample.counters[metric])
                    .filter(|value| value.is_finite())
                    .peekable();
                values.peek()?;
                Some(values.sum::<f64>())
            };
            return match self.source {
                CounterSource::Tma(_) => unreachable!(),
                CounterSource::Samples => Some(samples.count() as f64),
                CounterSource::Sum { metric, .. } => sum(metric),
                CounterSource::Ipc {
                    cycles,
                    instructions,
                    ..
                } => {
                    let cycles = sum(cycles)?;
                    (cycles > 0.0)
                        .then(|| sum(instructions).map(|instructions| instructions / cycles))
                        .flatten()
                }
            };
        }
        if timestamp < tracks.range.start_ns || timestamp >= tracks.range.end_ns {
            return None;
        }
        let bin = ((timestamp - tracks.range.start_ns) / tracks.bin_width_ns) as usize;
        match self.source {
            CounterSource::Tma(_) => unreachable!(),
            CounterSource::Samples => tracks.sample_counts.get(bin).map(|count| *count as f64),
            CounterSource::Sum { track, .. } | CounterSource::Ipc { track, .. } => {
                tracks.tracks[track].values.get(bin).copied().flatten()
            }
        }
    }

    fn format(&self, value: Option<f64>) -> String {
        value.map_or_else(
            || "-".into(),
            |value| match self.source {
                CounterSource::Ipc { .. } => format!("{value:.2}"),
                CounterSource::Tma(_) => format!("{:.1}%", value * 100.0),
                _ => super::session::format_count(value),
            },
        )
    }
}

fn counter_rows(session: &ShellSession, theme: &Theme) -> Vec<CounterRow> {
    let mut rows = vec![CounterRow {
        label: "Samples".into(),
        source: CounterSource::Samples,
        color: theme.resource_series[0],
    }];
    let Some(tracks) = &session.resource_tracks else {
        return rows;
    };
    let metric = |names: &[&str]| {
        session.profile.counter_metrics.iter().position(|metric| {
            let key: String = metric
                .key
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .flat_map(char::to_lowercase)
                .collect();
            names.iter().any(|name| key == *name)
        })
    };
    let cycles = metric(&["cycles", "pmucycles"]);
    let instructions = session.instructions_metric;
    for (label, metric, color) in [("Cycles", cycles, 1), ("Instructions", instructions, 2)] {
        if let Some(metric) = metric
            && let Some(track) = tracks
                .tracks
                .iter()
                .position(|track| track.key == session.profile.counter_metrics[metric].key)
        {
            rows.push(CounterRow {
                label: label.into(),
                source: CounterSource::Sum { track, metric },
                color: theme.resource_series[color],
            });
        }
    }
    if let (Some(cycles), Some(instructions), Some(track)) = (
        cycles,
        instructions,
        tracks
            .tracks
            .iter()
            .position(|track| track.key == "derived.ipc"),
    ) {
        rows.push(CounterRow {
            label: "IPC".into(),
            source: CounterSource::Ipc {
                track,
                cycles,
                instructions,
            },
            color: theme.resource_series[3],
        });
    }
    rows
}

enum TimelineRow {
    Ruler,
    Group(Group),
    Counter(CounterRow),
    Thread(usize, usize),
}

impl TimelineRow {
    fn height(&self) -> f32 {
        match self {
            Self::Ruler => RULER_H,
            Self::Group(_) => GROUP_H,
            Self::Counter(_) => COUNTER_H,
            Self::Thread(..) => THREAD_H,
        }
    }
}

pub fn render(
    shell: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let state = &shell.resource_timeline;
    let duration = time_range(session)
        .map_or(session.duration_seconds(), |range| {
            range.end_ns.saturating_sub(range.start_ns) as f64 / 1e9
        })
        .max(1e-9);
    let viewport = state.viewport(duration);
    let selection = state
        .drag
        .and_then(|drag| drag.preview)
        .or_else(|| selection_seconds(shell));
    let toolbar_button = |id: &'static str, icon: Icon, disabled| {
        button(id)
            .icon(icon)
            .size(ButtonSize::IconSm)
            .variant(ButtonVariant::Outline)
            .disabled(disabled)
    };
    let toolbar = div()
        .flex()
        .items_center()
        .flex_none()
        .h(px(41.0))
        .px(px(8.0))
        .gap(px(4.0))
        .border_b_1()
        .border_color(theme.border)
        .child(
            toolbar_button("resource-pan-left", Icon::MoveLeft, viewport.0 <= 0.0).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.resource_timeline.pan(-1.0, duration);
                    cx.notify();
                }),
            ),
        )
        .child(
            toolbar_button(
                "resource-pan-right",
                Icon::MoveRight,
                viewport.1 >= duration,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.resource_timeline.pan(1.0, duration);
                cx.notify();
            })),
        )
        .child(
            toolbar_button(
                "resource-zoom-in",
                Icon::ZoomIn,
                viewport.1 - viewport.0 <= 1e-6,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let selection = selection_seconds(this);
                this.resource_timeline.zoom(0.5, duration, selection);
                cx.notify();
            })),
        )
        .child(
            toolbar_button(
                "resource-zoom-out",
                Icon::ZoomOut,
                viewport.1 - viewport.0 >= duration,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let selection = selection_seconds(this);
                this.resource_timeline.zoom(2.0, duration, selection);
                cx.notify();
            })),
        )
        .child(
            button("resource-zoom-selection")
                .label("Zoom to selection")
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Outline)
                .disabled(selection.is_none())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some((start, end)) = selection_seconds(this) {
                        this.resource_timeline.fit(start, end - start, duration);
                        cx.notify();
                    }
                })),
        )
        .child(
            button("resource-reset")
                .label("Reset")
                .icon(Icon::RotateCcw)
                .size(ButtonSize::Sm)
                .variant(ButtonVariant::Ghost)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.resource_timeline.viewport = None;
                    this.resource_timeline.hover = None;
                    this.resource_timeline.drag = None;
                    commit_range(this, None, cx);
                })),
        );
    let mut content = div()
        .id("resources")
        .size_full()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .children(super::topdown::hierarchy(session, None, cx))
        .child(toolbar);
    if session.profile.samples.is_empty() && session.tma.is_none() {
        return content
            .child(empty_state(
                Icon::Gauge,
                "No timestamped samples in this recording",
            ))
            .into_any_element();
    }
    if let Some((start, end)) = selection {
        content = content.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(12.0))
                .h(px(33.0))
                .px(px(12.0))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(12.0))
                .child(format!(
                    "{} to {} · {}",
                    format_ms(start),
                    format_ms(end),
                    format_ms(end - start)
                ))
                .child(
                    button("resource-clear-range")
                        .label("Clear selection")
                        .size(ButtonSize::Xs)
                        .variant(ButtonVariant::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.resource_timeline.drag = None;
                            commit_range(this, None, cx);
                        })),
                ),
        );
    }
    let mut counters = counter_rows(session, &theme);
    if let Some(tma) = &session.tma {
        counters.extend(
            tma.level1_rows()
                .enumerate()
                .filter(|&(index, row)| {
                    row.value.is_some()
                        || tma
                            .intervals
                            .iter()
                            .any(|interval| interval.values[index].is_some())
                })
                .map(|(index, row)| CounterRow {
                    label: match row.name.as_str() {
                        "retiring" => "Retiring".into(),
                        "fe_bound" => "Frontend bound".into(),
                        "be_bound" => "Backend bound".into(),
                        "bad_speculation" => "Bad speculation".into(),
                        name => name.replace('_', " ").into(),
                    },
                    source: CounterSource::Tma(index),
                    color: theme.tma_color(super::tma_category(&row.name)),
                }),
        );
    }
    let counter_count = counters.len();
    let mut rows = vec![TimelineRow::Ruler, TimelineRow::Group(Group::Counters)];
    if !state.collapsed.contains(&Group::Counters) {
        rows.extend(counters.into_iter().map(TimelineRow::Counter));
    }
    rows.push(TimelineRow::Group(Group::Threads));
    if !state.collapsed.contains(&Group::Threads) {
        let mut threads: Vec<_> = (0..session.threads.len())
            .filter(|index| session.threads[*index].samples > 0)
            .collect();
        threads.sort_by_key(|index| session.threads[*index].thread_id);
        rows.extend(
            threads
                .into_iter()
                .enumerate()
                .map(|(color, index)| TimelineRow::Thread(index, color)),
        );
    }
    let focus = state.focus.clone().unwrap_or_else(|| cx.focus_handle());
    let key_focus = focus.clone();
    content = content.child(
        div()
            .id("resource-timeline")
            .flex_none()
            .track_focus(&focus)
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if !key_focus.is_focused(window) {
                        return;
                    }
                    let selection = selection_seconds(this);
                    match event.keystroke.key.as_str() {
                        "left" => this.resource_timeline.pan(-1.0, duration),
                        "right" => this.resource_timeline.pan(1.0, duration),
                        "+" | "=" => this.resource_timeline.zoom(0.5, duration, selection),
                        "-" => this.resource_timeline.zoom(2.0, duration, selection),
                        "escape" => {
                            this.resource_timeline.drag = None;
                            commit_range(this, None, cx);
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(timeline_canvas(
                cx.entity(),
                session.clone(),
                theme.clone(),
                rows,
                viewport,
                selection,
                state.hover,
                state.selected_sample,
                focus,
                state.collapsed.clone(),
                counter_count,
            )),
    );
    if let Some(sample) = state
        .selected_sample
        .and_then(|index| session.profile.samples.get(index))
    {
        let name = sample
            .stack
            .last()
            .and_then(|id| session.profile.frames.get(*id))
            .map_or("Unknown frame", |frame| frame.friendly_name.as_str());
        let thread = session
            .threads
            .iter()
            .find(|thread| thread.thread_id == sample.thread_id)
            .map_or_else(
                || format!("TID {}", sample.thread_id),
                |thread| thread.label.clone(),
            );
        let cpu = sample
            .cpu
            .map_or_else(|| "CPU unavailable".into(), |cpu| format!("CPU {cpu}"));
        let time = time_range(session).map_or(0.0, |full| {
            sample.timestamp_ns.saturating_sub(full.start_ns) as f64 / 1e9
        });
        content = content.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .flex_wrap()
                .gap(px(16.0))
                .px(px(12.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(12.0))
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(name.to_owned()),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("{thread} · {cpu}")),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format_ms(time)),
                )
                .child(
                    button("resource-clear-sample")
                        .icon(Icon::X)
                        .size(ButtonSize::IconXs)
                        .variant(ButtonVariant::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.resource_timeline.selected_sample = None;
                            cx.notify();
                        })),
                ),
        );
    }
    content.into_any_element()
}

fn format_ms(seconds: f64) -> String {
    format!("{:.3} ms", seconds * 1e3)
}

#[allow(clippy::too_many_arguments)]
fn timeline_canvas(
    entity: Entity<ShellView>,
    session: Arc<ShellSession>,
    theme: Theme,
    rows: Vec<TimelineRow>,
    viewport: (f64, f64),
    selection: Option<(f64, f64)>,
    hover: Option<f64>,
    selected: Option<usize>,
    focus: FocusHandle,
    collapsed: BTreeSet<Group>,
    counter_count: usize,
) -> impl IntoElement {
    let height = rows.iter().map(TimelineRow::height).sum::<f32>();
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.set_cursor_style(CursorStyle::Crosshair, &hitbox);
            let left = f32::from(bounds.left());
            let top = f32::from(bounds.top());
            let plot_left = left + GUTTER;
            let plot_width = (f32::from(bounds.size.width) - GUTTER).max(1.0);
            let x_at = |time: f64| {
                plot_left + ((time - viewport.0) / (viewport.1 - viewport.0)) as f32 * plot_width
            };
            let origin = time_range(&session).map_or(0, |range| range.start_ns);
            let ticks = time_ticks(viewport);
            let mut y = top;
            let mut groups = Vec::new();
            let mut samples = Vec::new();
            let mut thread_samples = BTreeMap::<u32, Vec<usize>>::new();
            for (index, sample) in session.profile.samples.iter().enumerate() {
                thread_samples
                    .entry(sample.thread_id)
                    .or_default()
                    .push(index);
            }
            for row in &rows {
                let h = row.height();
                window.paint_quad(fill(
                    Bounds::new(point(px(left), px(y)), size(bounds.size.width, px(h))),
                    theme.background,
                ));
                match *row {
                    TimelineRow::Ruler => {
                        for (index, &time) in ticks.iter().enumerate() {
                            let step_ms = ticks
                                .get(1)
                                .map_or((viewport.1 - viewport.0) * 1e3, |next| {
                                    (next - ticks[0]) * 1e3
                                });
                            let precision = (-step_ms.log10().floor()).max(0.0) as usize;
                            let label = format!("{:.*} ms", precision.min(6), time * 1e3);
                            let line = shape_label(&label, 10.0, theme.muted_foreground, window);
                            let width = f32::from(line.width);
                            let x = if index == 0 {
                                x_at(time)
                            } else if index + 1 == ticks.len() {
                                x_at(time) - width
                            } else {
                                x_at(time) - width / 2.0
                            };
                            let _ = line.paint(
                                point(px(x.max(plot_left)), px(y + 13.0)),
                                px(14.0),
                                window,
                                cx,
                            );
                        }
                    }
                    TimelineRow::Group(group) => {
                        window.paint_quad(fill(
                            Bounds::new(point(px(left), px(y)), size(bounds.size.width, px(h))),
                            theme.muted.opacity(0.5),
                        ));
                        let folded = collapsed.contains(&group);
                        let mut chevron = PathBuilder::stroke(px(1.2));
                        let points = if folded {
                            [(16.0, 11.0), (20.0, 15.0), (16.0, 19.0)]
                        } else {
                            [(16.0, 13.0), (20.0, 17.0), (24.0, 13.0)]
                        };
                        chevron.move_to(point(px(left + points[0].0), px(y + points[0].1)));
                        for (x, offset) in &points[1..] {
                            chevron.line_to(point(px(left + x), px(y + offset)));
                        }
                        if let Ok(path) = chevron.build() {
                            window.paint_path(path, theme.foreground);
                        }
                        let (label, count) = match group {
                            Group::Counters => ("Counters", counter_count),
                            Group::Threads => (
                                "Threads",
                                session
                                    .threads
                                    .iter()
                                    .filter(|thread| thread.samples > 0)
                                    .count(),
                            ),
                        };
                        window.with_text_style(
                            Some(gpui::TextStyleRefinement {
                                font_weight: Some(FontWeight::MEDIUM),
                                ..Default::default()
                            }),
                            |window| {
                                let line = shape_label(label, 12.0, theme.foreground, window);
                                let width = f32::from(line.width);
                                let _ = line.paint(
                                    point(px(left + 32.0), px(y + 8.0)),
                                    px(16.0),
                                    window,
                                    cx,
                                );
                                paint_text(
                                    &count.to_string(),
                                    left + 42.0 + width,
                                    y + 8.0,
                                    12.0,
                                    theme.muted_foreground,
                                    window,
                                    cx,
                                );
                            },
                        );
                        groups.push((group, y, y + h));
                    }
                    TimelineRow::Counter(ref counter) => {
                        let color = counter.color;
                        paint_row_label(
                            &counter.label,
                            &counter.format(counter.value(
                                &session,
                                selection,
                                hover.unwrap_or(viewport.0),
                            )),
                            left,
                            y,
                            h,
                            color,
                            &theme,
                            window,
                            cx,
                        );
                        paint_counter(
                            counter.bins(&session),
                            matches!(counter.source, CounterSource::Tma(_)),
                            origin,
                            viewport,
                            plot_left,
                            plot_width,
                            y,
                            color,
                            window,
                        );
                    }
                    TimelineRow::Thread(index, color_index) => {
                        let thread = &session.threads[index];
                        let color = theme.resource_series[[0, 4, 5, 3, 1, 2][color_index % 6]];
                        window.paint_quad(fill(
                            Bounds::new(point(px(plot_left), px(y)), size(px(plot_width), px(h))),
                            theme.muted.opacity(0.2),
                        ));
                        let indices = thread_samples
                            .get(&thread.thread_id)
                            .map_or(&[][..], Vec::as_slice);
                        let cpus: BTreeSet<_> = indices
                            .iter()
                            .filter_map(|&index| session.profile.samples[index].cpu)
                            .collect();
                        let subtitle = if cpus.is_empty() {
                            "CPU unavailable".into()
                        } else {
                            format!(
                                "CPU {}",
                                cpus.iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        };
                        paint_row_label(
                            &format!("TID {}", thread.thread_id),
                            &subtitle,
                            left,
                            y,
                            h,
                            color,
                            &theme,
                            window,
                            cx,
                        );
                        for &sample_index in indices {
                            let sample = &session.profile.samples[sample_index];
                            let time = sample.timestamp_ns.saturating_sub(origin) as f64 / 1e9;
                            if time < viewport.0 || time >= viewport.1 {
                                continue;
                            }
                            let x = x_at(time);
                            let mark = Bounds::new(
                                point(px((x - 1.5).max(plot_left)), px(y + 6.0)),
                                size(
                                    px(3.0_f32
                                        .min(plot_left + plot_width - (x - 1.5).max(plot_left))),
                                    px(24.0),
                                ),
                            );
                            window.paint_quad(fill(mark, color));
                            if selected == Some(sample_index) {
                                window.paint_quad(
                                    fill(mark, color)
                                        .border_widths(px(1.0))
                                        .border_color(theme.foreground),
                                );
                            }
                            samples.push((sample_index, x, y + 6.0, y + 30.0));
                        }
                    }
                }
                if matches!(row, TimelineRow::Counter(_) | TimelineRow::Thread(..)) {
                    for &tick in &ticks {
                        paint_rect(x_at(tick), y, 1.0, h, theme.border.opacity(0.6), window);
                    }
                    if let Some((start, end)) = selection {
                        let x0 = x_at(start).clamp(plot_left, plot_left + plot_width);
                        let x1 = x_at(end).clamp(plot_left, plot_left + plot_width);
                        if x1 > x0 {
                            paint_rect(x0, y, x1 - x0, h, theme.foreground.opacity(0.1), window);
                        }
                        for time in [start, end] {
                            let x = x_at(time);
                            if x >= plot_left && x <= plot_left + plot_width {
                                paint_rect(x, y, 1.0, h, theme.foreground.opacity(0.5), window);
                            }
                        }
                    }
                    if let Some(time) = hover {
                        let x = x_at(time);
                        if x >= plot_left && x <= plot_left + plot_width {
                            paint_rect(x, y, 1.0, h, theme.foreground.opacity(0.5), window);
                        }
                    }
                }
                if !matches!(row, TimelineRow::Group(_)) {
                    paint_rect(plot_left - 1.0, y, 1.0, h, theme.border, window);
                }
                paint_rect(
                    left,
                    y + h - 1.0,
                    f32::from(bounds.size.width),
                    1.0,
                    theme.border,
                    window,
                );
                y += h;
            }
            register_timeline_mouse(
                &entity,
                &hitbox,
                bounds,
                viewport,
                focus.clone(),
                groups,
                samples,
                window,
            );
        },
    )
    .flex_none()
    .w_full()
    .h(px(height))
}

fn time_ticks((start, end): (f64, f64)) -> Vec<f64> {
    let magnitude = 10.0_f64.powf(((end - start) / 6.0).log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .find(|step| step * magnitude >= (end - start) / 6.0)
        .unwrap_or(10.0)
        * magnitude;
    let first = (start / step).ceil() * step;
    (0..=10)
        .map(|index| first + index as f64 * step)
        .take_while(|tick| *tick <= end)
        .collect()
}

fn paint_text(
    text: &str,
    x: f32,
    y: f32,
    font_size: f32,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let line = shape_label(text, font_size, color, window);
    let _ = line.paint(point(px(x), px(y)), px(font_size + 4.0), window, cx);
}

#[allow(clippy::too_many_arguments)]
fn paint_row_label(
    label: &str,
    value: &str,
    left: f32,
    top: f32,
    height: f32,
    color: Hsla,
    theme: &Theme,
    window: &mut Window,
    cx: &mut App,
) {
    let name_y = top + if height == THREAD_H { 1.0 } else { 11.0 };
    window.paint_quad(
        fill(
            Bounds::new(
                point(px(left + 12.0), px(name_y + 8.0)),
                size(px(6.0), px(6.0)),
            ),
            color,
        )
        .corner_radii(px(3.0)),
    );
    paint_text(
        &truncate_label(label, 22),
        left + 24.0,
        name_y,
        12.0,
        theme.foreground,
        window,
        cx,
    );
    paint_text(
        &truncate_label(value, 27),
        left + 12.0,
        name_y + 19.0,
        11.0,
        theme.muted_foreground,
        window,
        cx,
    );
}

fn paint_rect(x: f32, y: f32, width: f32, height: f32, color: Hsla, window: &mut Window) {
    window.paint_quad(fill(
        Bounds::new(point(px(x), px(y)), size(px(width), px(height))),
        color,
    ));
}

#[allow(clippy::too_many_arguments)]
fn paint_counter(
    bins: Vec<(u64, u64, Option<f64>)>,
    percentage: bool,
    origin: u64,
    viewport: (f64, f64),
    left: f32,
    width: f32,
    top: f32,
    color: Hsla,
    window: &mut Window,
) {
    let max = if percentage {
        1.0
    } else {
        bins.iter()
            .filter_map(|(_, _, value)| *value)
            .fold(0.0_f64, f64::max)
    };
    if max <= 0.0 {
        return;
    }
    let x_at = |time| left + ((time - viewport.0) / (viewport.1 - viewport.0)) as f32 * width;
    let mut points = Vec::new();
    let mut previous_end = 0;
    for (start_ns, end_ns, value) in bins {
        let start = start_ns.saturating_sub(origin) as f64 / 1e9;
        let end = end_ns.saturating_sub(origin) as f64 / 1e9;
        if end <= viewport.0 || start >= viewport.1 {
            continue;
        }
        if start_ns > previous_end {
            points.push((x_at(start.max(viewport.0)), None));
        }
        previous_end = end_ns;
        points.push((x_at(start.max(viewport.0)), value));
        points.push((x_at(end.min(viewport.1)), value));
    }
    let baseline = top + 56.0;
    let mut area = None;
    let mut stroke = None;
    let mut last_x = left;
    for (x, value) in points
        .into_iter()
        .chain(std::iter::once((left + width, None)))
    {
        if let Some(value) = value {
            let fraction = (value / max).clamp(0.0, 1.0) as f32;
            let y = if percentage {
                baseline - fraction * (baseline - top - 4.0)
            } else {
                top + (44.0 - fraction * 40.0) * 56.0 / 48.0
            };
            let p = point(px(x), px(y));
            if area.is_none() {
                let mut path = PathBuilder::fill();
                path.move_to(point(px(x), px(baseline)));
                path.line_to(p);
                area = Some(path);
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(point(px(x), px(baseline)));
                path.line_to(p);
                stroke = Some(path);
            } else {
                if let Some(path) = &mut area {
                    path.line_to(p);
                }
                if let Some(path) = &mut stroke {
                    path.line_to(p);
                }
            }
            last_x = x;
        } else {
            if let Some(mut path) = area.take() {
                path.line_to(point(px(last_x), px(baseline)));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, color.opacity(0.5));
                }
            }
            if let Some(mut path) = stroke.take() {
                path.line_to(point(px(last_x), px(baseline)));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn register_timeline_mouse(
    entity: &Entity<ShellView>,
    hitbox: &gpui::Hitbox,
    bounds: Bounds<Pixels>,
    viewport: (f64, f64),
    focus: FocusHandle,
    groups: Vec<(Group, f32, f32)>,
    samples: Vec<(usize, f32, f32, f32)>,
    window: &mut Window,
) {
    let left = f32::from(bounds.left()) + GUTTER;
    let width = (f32::from(bounds.size.width) - GUTTER).max(1.0);
    let time_at = move |x: f32| {
        viewport.0 + ((x - left) / width).clamp(0.0, 1.0) as f64 * (viewport.1 - viewport.0)
    };
    let down_hitbox = hitbox.clone();
    let down_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble
            || event.button != MouseButton::Left
            || !down_hitbox.is_hovered(window)
        {
            return;
        }
        let x = f32::from(event.position.x);
        let y = f32::from(event.position.y);
        window.focus(&focus);
        down_entity.update(cx, |this, cx| {
            this.resource_timeline.focus = Some(focus.clone());
            if let Some((group, _, _)) = groups
                .iter()
                .find(|(_, top, bottom)| y >= *top && y < *bottom)
            {
                if !this.resource_timeline.collapsed.remove(group) {
                    this.resource_timeline.collapsed.insert(*group);
                }
            } else if x >= left {
                let sample = samples
                    .iter()
                    .rev()
                    .filter(|(_, sample_x, top, bottom)| {
                        (sample_x - x).abs() <= 2.0 && y >= *top && y < *bottom
                    })
                    .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
                    .map(|sample| sample.0);
                this.resource_timeline.drag = Some(Drag {
                    start: time_at(x),
                    x,
                    sample,
                    preview: None,
                });
            }
            cx.notify();
        });
        cx.stop_propagation();
    });
    let move_hitbox = hitbox.clone();
    let move_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let x = f32::from(event.position.x);
        let hover = (move_hitbox.is_hovered(window) && x >= left).then(|| time_at(x));
        move_entity.update(cx, |this, cx| {
            let state = &mut this.resource_timeline;
            let mut changed = state.hover != hover;
            state.hover = hover;
            if let Some(drag) = &mut state.drag
                && (x - drag.x).abs() > 3.0
            {
                let time = time_at(x);
                drag.preview =
                    (time != drag.start).then_some((time.min(drag.start), time.max(drag.start)));
                changed = true;
            }
            if changed {
                cx.notify();
            }
        });
    });
    let up_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
        if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
            return;
        }
        up_entity.update(cx, |this, cx| {
            let Some(drag) = this.resource_timeline.drag.take() else {
                return;
            };
            if (f32::from(event.position.x) - drag.x).abs() <= 3.0 {
                if let Some(sample) = drag.sample {
                    this.resource_timeline.selected_sample = Some(sample);
                    cx.notify();
                } else {
                    commit_range(this, None, cx);
                }
            } else {
                commit_range(this, drag.preview, cx);
            }
        });
    });
}

pub fn render_snapshot(
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(snapshot) = session.snapshot.as_ref() else {
        return empty_state(Icon::Gauge, "No resource snapshot in this recording")
            .into_any_element();
    };

    div()
        .id("resources")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(8.0))
        .child(
            div().flex().flex_wrap().gap(px(8.0)).children(
                snapshot
                    .resources
                    .iter()
                    .map(|resource| resource_card(resource, &theme, cx)),
            ),
        )
        .when(!snapshot.findings.is_empty(), |el| {
            el.child(ui::section_caption("findings · ranked", cx))
                .child(
                    div().flex().flex_col().gap(px(6.0)).children(
                        snapshot
                            .findings
                            .iter()
                            .map(|finding| finding_card(finding, &theme)),
                    ),
                )
        })
        .when(!snapshot.collectors.is_empty(), |el| {
            el.child(collectors_card(snapshot, &theme))
        })
        .into_any_element()
}

fn resource_card(
    resource: &ResourceUse,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let mut card = ui::viz_card(resource.resource.to_uppercase());
    if let Some(headline) = resource.headline.clone() {
        card = card.action(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(headline),
        );
    }

    for (index, category) in UseCategory::ALL.into_iter().enumerate() {
        let metrics = &resource.summaries[index];
        if metrics.is_empty() {
            continue;
        }
        card = card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(ui::section_caption(category.title(), cx))
                .children(
                    metrics
                        .iter()
                        .map(|metric| metric_row(metric, category, theme)),
                ),
        );
    }

    if let Some(chart) = spark_chart(resource) {
        card = card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{} · {}",
                            chart.metric.replace('_', " "),
                            chart.unit
                        )),
                )
                .child(sparkline(chart, theme.viz.series[0], theme)),
        );
    }

    div()
        .flex_1()
        .min_w(px(240.0))
        .child(card)
        .into_any_element()
}

fn metric_row(metric: &SummaryMetric, category: UseCategory, theme: &Theme) -> gpui::AnyElement {
    let fraction = metric.fraction.unwrap_or(0.0) as f32;
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(6.0))
                .text_size(px(10.5))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(match &metric.entity {
                            Some(entity) => {
                                format!("{} · {entity}", metric.metric.replace('_', " "))
                            }
                            None => metric.metric.replace('_', " "),
                        }),
                )
                .child(div().flex_none().child(metric.value.clone()))
                .when(!metric.scope.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(9.0))
                            .text_color(theme.muted_foreground)
                            .child(metric.scope.replace('_', " ")),
                    )
                }),
        )
        .when(metric.fraction.is_some(), |el| {
            el.child(ui::meter(fraction).color(meter_color(category, fraction, theme)))
        })
        .into_any_element()
}

/// Utilization reads as load, saturation and errors read as damage — so they
/// escalate through the status ramp at very different thresholds.
fn meter_color(category: UseCategory, fraction: f32, theme: &Theme) -> Hsla {
    match category {
        UseCategory::Errors => {
            if fraction > 0.0 {
                theme.viz.status_critical
            } else {
                theme.viz.status_good
            }
        }
        UseCategory::Saturation => match fraction {
            value if value >= 0.30 => theme.viz.status_critical,
            value if value >= 0.06 => theme.viz.status_serious,
            _ => theme.viz.status_good,
        },
        UseCategory::Utilization => match fraction {
            value if value >= 0.85 => theme.viz.status_serious,
            value if value >= 0.70 => theme.viz.status_warn,
            _ => theme.viz.series[0],
        },
    }
}

/// The card's one sparkline: engine occupancy for a device that reports it,
/// else its clock; otherwise the busiest utilization series, falling back to
/// whatever the resource does have.
fn spark_chart(resource: &ResourceUse) -> Option<&SnapshotChart> {
    if matches!(resource.resource.as_str(), "gpu" | "npu") {
        let named = |metric: &str| {
            resource
                .charts
                .iter()
                .find(|chart| chart.metric == metric && chart.max_value > 0.0)
        };
        if let Some(chart) = named("busy").or_else(|| named("frequency")) {
            return Some(chart);
        }
    }
    resource
        .charts
        .iter()
        .filter(|chart| chart.category == UseCategory::Utilization)
        .chain(resource.charts.iter())
        .find(|chart| chart.max_value > 0.0)
}

fn sparkline(chart: &SnapshotChart, color: Hsla, theme: &Theme) -> impl IntoElement + use<> {
    let surface = theme.viz.surface;
    let max = chart.max_value;
    let series: Vec<Vec<(f64, f64)>> = chart
        .series
        .iter()
        .map(|series| series.points.clone())
        .collect();
    let (first, last) = series
        .iter()
        .flatten()
        .fold((f64::MAX, f64::MIN), |(first, last), point| {
            (first.min(point.0), last.max(point.0))
        });

    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(fill(bounds, surface));
            let span = (last - first).max(1e-9);
            let left = f32::from(bounds.left());
            let width = f32::from(bounds.size.width);
            for points in &series {
                let mapped: Vec<(f32, Option<f64>)> = points
                    .iter()
                    .map(|(time, value)| {
                        (left + ((time - first) / span) as f32 * width, Some(*value))
                    })
                    .collect();
                paint_area_series(
                    f32::from(bounds.top()) + 1.0,
                    f32::from(bounds.size.height) - 2.0,
                    &mapped,
                    max,
                    color,
                    window,
                );
            }
        },
    )
    .w_full()
    .h(px(SPARK_H))
}

fn finding_card(finding: &SnapshotFinding, theme: &Theme) -> gpui::AnyElement {
    let detail = |label: &'static str, text: String| {
        div()
            .flex()
            .gap(px(4.0))
            .text_size(px(11.0))
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .child(label),
            )
            .child(div().flex_1().min_w(px(0.0)).child(text))
    };

    ui::viz_panel()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    badge(finding.severity.label().to_uppercase())
                        .tint(severity_color(finding.severity, theme)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::MEDIUM)
                        .child(finding.finding.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.muted_foreground)
                        .child(finding.resource.to_uppercase()),
                ),
        )
        .child(detail("Evidence:", finding.evidence.clone()))
        .child(detail("Try:", finding.recommendation.clone()))
        .when(!finding.quality.is_empty(), |card| {
            card.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.muted_foreground)
                    .child(format!("quality: {}", finding.quality)),
            )
        })
        .into_any_element()
}

pub(super) fn severity_color(severity: Severity, theme: &Theme) -> Hsla {
    match severity {
        Severity::High => theme.viz.status_critical,
        Severity::Medium => theme.viz.status_serious,
        Severity::Info => theme.viz.series[0],
    }
}

fn collectors_card(snapshot: &SnapshotData, theme: &Theme) -> gpui::AnyElement {
    ui::viz_card("collectors")
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .children(snapshot.collectors.iter().map(|collector| {
                    let color = match collector.status.as_str() {
                        "ok" => theme.viz.status_good,
                        "partial" => theme.viz.status_warn,
                        _ => theme.viz.status_critical,
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .rounded(px(3.0))
                        .bg(theme.muted)
                        .px(px(6.0))
                        .py(px(2.0))
                        .text_size(px(10.0))
                        .child(div().size(px(6.0)).rounded_full().bg(color))
                        .child(collector.name.clone())
                        .when(!collector.message.is_empty(), |el| {
                            el.child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(collector.message.clone()),
                            )
                        })
                })),
        )
        .into_any_element()
}
