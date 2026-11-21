use super::{ShellView, ViewId, session::ShellSession};
use crate::{
    allocations::{AllocationData, Boundary},
    profile::ProfileFrame,
    profile_analysis::SampleFilter,
    snapshot::format_count,
    ui::{self, ActiveTheme},
};
use gpui::{Context, div, prelude::*, px};
use std::{collections::HashMap, sync::Arc};

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let exponent = if bytes < 1024.0 {
        0
    } else {
        (bytes.log(1024.0).floor() as usize).min(3)
    };
    let value = format!("{:.2}", bytes / 1024.0f64.powi(exponent as i32));
    format!(
        "{} {}",
        value.trim_end_matches('0').trim_end_matches('.'),
        UNITS[exponent]
    )
}

#[derive(Default)]
pub(super) struct Site {
    title: String,
    count: u64,
    requested: u64,
    live: u64,
    ended_lifetime: u64,
    ended_count: u64,
    peak: u64,
}

pub(super) fn allocation_sites(
    data: &AllocationData,
    frames: &[ProfileFrame],
    filter: &SampleFilter,
) -> Vec<(u64, Site)> {
    let cutoff = filter.range.map(|range| range.end_ns).unwrap_or(u64::MAX);
    let mut sites = HashMap::<u64, (Site, Vec<(Boundary, i64)>)>::new();
    for lifetime in &data.lifetimes {
        let stack = data.stacks.get(&lifetime.stack);
        if filter.range.is_some_and(|range| {
            lifetime.start.time < range.start_ns || lifetime.start.time >= range.end_ns
        }) || filter
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&lifetime.tid))
            || filter.regions.as_ref().is_some_and(|regions| {
                !regions.contains(lifetime.pid, lifetime.tid, lifetime.start.time)
            })
            || filter.frame_ids.as_ref().is_some_and(|selected| {
                !stack.is_some_and(|frames| frames.iter().any(|id| selected.contains(id)))
            })
            || filter.leaf_frames.as_ref().is_some_and(|selected| {
                !data
                    .application_frames
                    .get(&lifetime.stack)
                    .is_some_and(|frame| selected.contains(frame))
            })
        {
            continue;
        }
        let (site, changes) = sites.entry(lifetime.stack).or_default();
        site.count += 1;
        site.requested += lifetime.bytes;
        changes.push((lifetime.start, lifetime.bytes as i64));
        if let Some(end) = lifetime.end.filter(|end| end.time <= cutoff) {
            site.ended_lifetime += end.time.saturating_sub(lifetime.start.time);
            site.ended_count += 1;
            changes.push((end, -(lifetime.bytes as i64)));
        } else {
            site.live += lifetime.bytes;
        }
    }
    let mut sites = sites
        .into_iter()
        .map(|(stack, (mut site, mut changes))| {
            changes.sort_unstable_by_key(|(boundary, _)| *boundary);
            let mut live = 0i64;
            let mut peak = 0i64;
            for (_, delta) in changes {
                live += delta;
                peak = peak.max(live);
            }
            site.peak = peak as u64;
            site.title = data.application_frames.get(&stack).map_or_else(
                || "[unknown stack]".into(),
                |frame| frames[*frame].friendly_name.clone(),
            );
            (stack, site)
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|(_, site)| std::cmp::Reverse(site.requested));
    sites
}

pub(super) struct ViewState {
    search: gpui::Entity<ui::TextInput>,
    sort: Sort,
    point: Option<usize>,
    wide: bool,
    chart_focus: gpui::FocusHandle,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Title,
    Count,
    Requested,
    Live,
    Peak,
    Lifetime,
}

#[derive(Clone, Copy)]
struct Sort {
    key: SortKey,
    descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Requested,
            descending: true,
        }
    }
}

impl ViewState {
    pub(super) fn new(cx: &mut Context<ShellView>) -> Self {
        let search = cx.new(|cx| {
            ui::TextInput::new("allocation-search", "Search call site, stack or file…", cx)
                .leading_icon(ui::Icon::Search)
                .height(32.)
                .text_size(12.)
                .width(256.)
        });
        let subscription = cx.subscribe(&search, |_, _, _: &ui::InputEvent, cx| cx.notify());
        Self {
            search,
            sort: Sort::default(),
            point: None,
            wide: false,
            chart_focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    pub(super) fn reset(&mut self, cx: &mut Context<ShellView>) {
        self.search.update(cx, |input, cx| input.clear(cx));
        self.sort = Sort::default();
        self.point = None;
    }
}

fn lifetime(site: &Site) -> String {
    site.ended_lifetime
        .checked_div(site.ended_count)
        .map_or_else(
            || "Unknown".into(),
            |ns| {
                if ns < 1_000 {
                    format!("{ns} ns")
                } else if ns < 1_000_000 {
                    format!("{:.2} µs", ns as f64 / 1_000.)
                } else {
                    format!("{:.3} ms", ns as f64 / 1_000_000.)
                }
            },
        )
}

fn sort_sites(left: &(u64, Site), right: &(u64, Site), sort: Sort) -> std::cmp::Ordering {
    let a = &left.1;
    let b = &right.1;
    let order = match sort.key {
        SortKey::Title => a.title.cmp(&b.title),
        SortKey::Count => a.count.cmp(&b.count),
        SortKey::Requested => a.requested.cmp(&b.requested),
        SortKey::Live => a.live.cmp(&b.live),
        SortKey::Peak => a.peak.cmp(&b.peak),
        SortKey::Lifetime => {
            let a = a.ended_lifetime.checked_div(a.ended_count);
            let b = b.ended_lifetime.checked_div(b.ended_count);
            match (a, b) {
                (Some(a), Some(b)) => a.cmp(&b),
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => return std::cmp::Ordering::Greater,
                (_, None) => return std::cmp::Ordering::Less,
            }
        }
    };
    (if sort.descending {
        order.reverse()
    } else {
        order
    })
    .then_with(|| left.0.cmp(&right.0))
}

pub fn render(
    state: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let Some(data) = session.allocations.as_ref() else {
        return ui::empty_state(ui::Icon::MemoryStick, "No allocations in this recording")
            .into_any_element();
    };
    if let Some(error) = &data.error {
        return div()
            .p(px(12.))
            .child(format!("Allocation data could not load: {error}"))
            .into_any_element();
    }
    let sites = state.allocation_sites.latest();
    let search = state
        .allocation_ui
        .search
        .read(cx)
        .text()
        .trim()
        .to_lowercase();
    let mut rows: Vec<_> = sites
        .into_iter()
        .flat_map(|sites| sites.iter())
        .filter(|(stack, site)| {
            search.is_empty()
                || site.title.to_lowercase().contains(&search)
                || data.stacks.get(stack).is_some_and(|ids| {
                    ids.iter().any(|id| {
                        let frame = &session.profile.frames[*id];
                        [
                            frame.name.as_str(),
                            frame.friendly_name.as_str(),
                            frame.file.as_deref().unwrap_or_default(),
                            frame.module.as_deref().unwrap_or_default(),
                        ]
                        .iter()
                        .any(|value| value.to_lowercase().contains(&search))
                    })
                })
        })
        .collect();
    rows.sort_by(|a, b| sort_sites(a, b, state.allocation_ui.sort));
    let selected = state
        .allocation_stack
        .or_else(|| sites.and_then(|sites| sites.first().map(|site| site.0)));
    let mut table = div().flex().flex_col().min_w(px(790.)).text_size(px(12.));
    let columns = [
        (SortKey::Title, "Call site", 0.34),
        (SortKey::Count, "Count", 0.08),
        (SortKey::Requested, "Requested", 0.12),
        (SortKey::Live, "Live at end", 0.12),
        (SortKey::Peak, "Observed peak", 0.14),
        (SortKey::Lifetime, "Mean ended lifetime", 0.20),
    ];
    let header = div()
        .flex()
        .h(px(37.))
        .flex_none()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.muted.opacity(0.35))
        .children(
            columns
                .iter()
                .enumerate()
                .map(|(index, (key, title, width))| {
                    let key = *key;
                    let active = state.allocation_ui.sort.key == key;
                    let icon = if !active {
                        ui::Icon::ArrowUpDown
                    } else if state.allocation_ui.sort.descending {
                        ui::Icon::ArrowDown
                    } else {
                        ui::Icon::ArrowUp
                    };
                    div()
                        .id(("allocation-sort", index))
                        .w(gpui::relative(*width))
                        .h_full()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(if index == 0 { 16. } else { 8. }))
                        .pr(px(8.))
                        .when(index > 0, |el| el.justify_end())
                        .text_size(px(11.))
                        .text_color(if active {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .cursor_pointer()
                        .child(*title)
                        .child(icon.render(
                            px(12.),
                            if active {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            },
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let sort = &mut this.allocation_ui.sort;
                            sort.descending = if sort.key == key {
                                !sort.descending
                            } else {
                                key != SortKey::Title
                            };
                            sort.key = key;
                            cx.notify();
                        }))
                }),
        );
    for (stack, site) in &rows {
        let stack = *stack;
        let values = [
            site.title.clone(),
            format_count(site.count as f64),
            format_bytes(site.requested as f64),
            format_bytes(site.live as f64),
            format_bytes(site.peak as f64),
            lifetime(site),
        ];
        table = table.child(
            ui::table_row(selected == Some(stack), cx)
                .id(("allocation-site", stack))
                .children(values.into_iter().enumerate().map(|(index, value)| {
                    div()
                        .w(gpui::relative(columns[index].2))
                        .min_w(px(0.))
                        .px(px(if index == 0 { 12. } else { 8. }))
                        .truncate()
                        .when(index == 0, |el| {
                            el.font_family(theme.font_mono.clone())
                                .font_weight(gpui::FontWeight::MEDIUM)
                        })
                        .when(index > 0, |el| el.text_right())
                        .child(if index == 0 {
                            ui::table_label(value).into_any_element()
                        } else {
                            div().child(value).into_any_element()
                        })
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.allocation_stack = Some(stack);
                    cx.notify();
                })),
        );
    }
    let table = div().flex().flex_col().min_w(px(790.)).child(header).child(
        div()
            .id("allocation-table-rows")
            .max_h(px(404.))
            .overflow_y_scroll()
            .child(table),
    );
    let mut detail = div()
        .id("allocation-callstack")
        .max_h(px(460.))
        .overflow_y_scroll()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .text_size(px(11.));
    let selected_site = selected
        .and_then(|stack| sites.and_then(|sites| sites.iter().find(|site| site.0 == stack)));
    let ids = selected.and_then(|stack| data.stacks.get(&stack));
    detail =
        detail.child(
            div()
                .p(px(12.))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("Allocation callstack"),
                )
                .child(
                    div()
                        .mt(px(4.))
                        .font_family(theme.font_mono.clone())
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(selected_site.map_or_else(
                            || "Select a call site".into(),
                            |site| site.1.title.clone(),
                        )),
                )
                .child(
                    div()
                        .mt(px(4.))
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "Captured leaf to root · {} frames",
                            ids.map_or(0, Vec::len)
                        )),
                ),
        );
    for (index, id) in ids.into_iter().flatten().copied().enumerate() {
        let frame = &session.profile.frames[id];
        detail = detail.child(
            div()
                .id(("allocation-frame", id))
                .flex()
                .gap(px(8.))
                .px(px(12.))
                .py(px(6.))
                .border_b_1()
                .border_color(theme.border.opacity(0.5))
                .child(
                    div()
                        .w(px(16.))
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(index.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .truncate()
                                .child(frame.friendly_name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(frame.file.as_ref().map_or_else(
                                    || "Source unavailable".into(),
                                    |file| {
                                        frame.line.map_or_else(
                                            || file.clone(),
                                            |line| format!("{file}:{line}"),
                                        )
                                    },
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(
                                    frame
                                        .module
                                        .clone()
                                        .unwrap_or_else(|| "Module unavailable".into()),
                                ),
                        ),
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open_source_tab(id, cx))),
        );
    }
    if ids.is_none_or(Vec::is_empty) {
        detail = detail.child(
            div()
                .p(px(12.))
                .text_color(theme.muted_foreground)
                .child("This allocation has no recorded stack."),
        );
    }
    let summary = |label: &'static str, value: String| {
        div()
            .flex()
            .gap(px(4.))
            .child(div().text_color(theme.muted_foreground).child(label))
            .child(value)
    };
    let point = state
        .allocation_ui
        .point
        .unwrap_or(data.timeline.len().saturating_sub(1));
    let inspected = data
        .timeline
        .get(point)
        .map(|value| {
            format!(
                "{:.3} ms · {} · {}",
                (value.0 - data.timeline.first().unwrap().0) as f64 / 1e6,
                format_bytes(value.1),
                if point + 1 == data.timeline.len() {
                    "final endpoint"
                } else {
                    "bucket maximum"
                }
            )
        })
        .unwrap_or_default();
    div().id("allocations").size_full().overflow_y_scroll().flex().flex_col()
        .child(div().h(px(45.)).flex_none().flex().items_center().gap(px(4.)).px(px(8.)).border_b_1().border_color(theme.border)
            .child(state.allocation_ui.search.clone())
            .child(ui::button("allocation-reset").label("Reset").icon(ui::Icon::RotateCcw).variant(ui::ButtonVariant::Ghost).size(ui::ButtonSize::Sm)
                .on_click(cx.listener(|this, _, _, cx| { this.allocation_ui.reset(cx); this.allocation_stack = None; cx.notify(); })))
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(format!("{} call sites{}", rows.len(), if state.allocation_sites.stale(state.analysis_key()) { " · updating" } else { "" }))))
        .child(div().flex_none().flex().flex_wrap().items_center().gap(px(12.)).px(px(12.)).py(px(6.)).border_b_1().border_color(theme.border).text_size(px(11.)).text_color(theme.muted_foreground)
            .child(div().font_weight(gpui::FontWeight::MEDIUM).text_color(if data.complete {theme.foreground} else {theme.destructive}).child(format!("{} · {} capture", session.command_name.as_deref().unwrap_or(&session.name), if data.complete {"complete"} else {"incomplete"})))
            .child("Separate allocation recording").child("Root process · libc-interposable operations"))
        .child(div().flex_none().flex().flex_wrap().items_center().gap(px(24.)).px(px(12.)).py(px(8.)).text_size(px(12.)).line_height(px(16.)).border_b_1().border_color(theme.border)
            .child(summary("Count", format_count(data.count as f64))).child(summary("Requested", format_bytes(data.bytes as f64)))
            .child(summary("Live at end", format_bytes(data.live as f64))).child(summary("Observed peak", format_bytes(data.peak as f64)))
            .child(summary("Stack attributed", format!("{} / {}", data.attributed, data.count))))
        .child(div().flex_none().border_b_1().border_color(theme.border)
            .child(div().flex().justify_between().gap(px(8.)).px(px(12.)).pt(px(8.)).text_size(px(12.)).line_height(px(16.)).child(div().font_weight(gpui::FontWeight::MEDIUM).child("Live requested bytes"))
                .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(inspected)))
            .child(div().id("allocation-chart-scroll").overflow_x_scroll().px(px(8.)).child(allocation_chart(state, data, &theme, cx)))
            .child(div().px(px(12.)).pb(px(8.)).text_size(px(10.)).line_height(px(15.)).text_color(theme.muted_foreground)
                .child("Whole capture · bucket maxima, plus final endpoint · mean lifetime includes ended allocations only · realloc internal peaks are uncertain")))
        .child(div().flex_none().flex().min_w(px(0.)).when(!state.allocation_ui.wide, |el| el.flex_col())
            .child(div().id("allocation-table-scroll").min_w(px(0.)).flex_1().overflow_x_scroll().child(table)
                .when(rows.is_empty(), |el| el.child(div().p(px(24.)).text_size(px(12.)).text_color(theme.muted_foreground).child("No matching call sites"))))
            .child(detail.when(state.allocation_ui.wide, |el| el.w(gpui::relative(0.3)).min_w(px(280.)).flex_none().border_l_1().border_color(theme.border))
                .when(!state.allocation_ui.wide, |el| el.border_t_1().border_color(theme.border))))
        .into_any_element()
}

fn allocation_chart(
    state: &ShellView,
    data: &crate::allocations::AllocationData,
    theme: &ui::Theme,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    use gpui::{Bounds, HitboxBehavior, PathBuilder, Point, canvas, fill, point, size};
    let timeline = data.timeline.clone();
    let theme = theme.clone();
    let selected = state
        .allocation_ui
        .point
        .unwrap_or(timeline.len().saturating_sub(1));
    let entity = cx.entity();
    let focus = state.allocation_ui.chart_focus.clone();
    div()
        .id("allocation-chart")
        .min_w(px(480.))
        .w_full()
        .track_focus(&focus)
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
            window.focus(&focus)
        })
        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
            let len = this
                .session
                .as_ref()
                .and_then(|session| session.allocations.as_ref())
                .map_or(0, |data| data.timeline.len());
            if len == 0 {
                return;
            }
            let current = this.allocation_ui.point.unwrap_or(len - 1);
            let next = match event.keystroke.key.as_str() {
                "left" => current.saturating_sub(1),
                "right" => (current + 1).min(len - 1),
                "home" => 0,
                "end" => len - 1,
                _ => return,
            };
            this.allocation_ui.point = Some(next);
            cx.notify();
        }))
        .child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, cx| {
                    let wide = f32::from(window.viewport_size().width) >= 1280.;
                    entity.update(cx, |this, cx| {
                        if this.allocation_ui.wide != wide {
                            this.allocation_ui.wide = wide;
                            cx.notify();
                        }
                    });
                    window.set_cursor_style(gpui::CursorStyle::Crosshair, &hitbox);
                    if timeline.is_empty() {
                        return;
                    }
                    let first = timeline.first().unwrap().0;
                    let duration = timeline.last().unwrap().0.saturating_sub(first).max(1) as f64;
                    let maximum = timeline.iter().map(|point| point.1).fold(1., f64::max);
                    let scale = (f32::from(bounds.size.width) / 1000.)
                        .min(f32::from(bounds.size.height) / 172.);
                    let origin = f32::from(bounds.left())
                        + (f32::from(bounds.size.width) - 1000. * scale) / 2.;
                    let left = origin + 65. * scale;
                    let right = origin + 976. * scale;
                    let top = f32::from(bounds.top()) + 14. * scale;
                    let bottom = f32::from(bounds.top()) + 137. * scale;
                    let x = |time: u64| {
                        left + (time.saturating_sub(first) as f64 / duration) as f32
                            * (right - left)
                    };
                    let y = |bytes: f64| bottom - (bytes / maximum) as f32 * (bottom - top);
                    let text = |text: String,
                                at: Point<gpui::Pixels>,
                                anchor: f32,
                                window: &mut gpui::Window,
                                cx: &mut gpui::App| {
                        let line = crate::charts::shape_label(
                            &text,
                            10. * scale,
                            theme.muted_foreground,
                            window,
                        );
                        let _ = line.paint(
                            point(at.x - line.width * anchor, at.y - line.ascent),
                            line.ascent + line.descent,
                            window,
                            cx,
                        );
                    };
                    for ratio in [0., 0.5, 1.] {
                        let at = y(maximum * ratio);
                        dashed_line(
                            point(px(left), px(at)),
                            point(px(right), px(at)),
                            theme.border,
                            window,
                        );
                        text(
                            format_bytes(maximum * ratio),
                            point(px(left - 8. * scale), px(at + 3. * scale)),
                            1.,
                            window,
                            cx,
                        );
                    }
                    let raw_step = duration / 1e6 / 5.;
                    let magnitude = 10.0f64.powf(raw_step.log10().floor());
                    let step = (raw_step / magnitude).round() * magnitude;
                    for tick in 0..=(duration / 1e6 / step).floor() as usize {
                        let time_ms = tick as f64 * step;
                        let at = left + (right - left) * (time_ms / (duration / 1e6)) as f32;
                        dashed_line(
                            point(px(at), px(top)),
                            point(px(at), px(bottom)),
                            theme.border,
                            window,
                        );
                        text(
                            format!("{} ms", time_ms),
                            point(px(at), px(bottom + 18. * scale)),
                            0.5,
                            window,
                            cx,
                        );
                    }
                    let mut stroke = PathBuilder::stroke(px(1.5));
                    let mut area = PathBuilder::fill();
                    area.move_to(point(px(x(first)), px(bottom)));
                    for (index, (time, bytes)) in timeline.iter().enumerate() {
                        let at = point(px(x(*time)), px(y(*bytes)));
                        if index == 0 {
                            stroke.move_to(at);
                            area.line_to(at);
                        } else {
                            let prev = point(px(x(*time)), px(y(timeline[index - 1].1)));
                            stroke.line_to(prev);
                            stroke.line_to(at);
                            area.line_to(prev);
                            area.line_to(at);
                        }
                    }
                    area.line_to(point(px(right), px(bottom)));
                    area.close();
                    if let Ok(path) = area.build() {
                        window.paint_path(path, theme.chart_series[1].opacity(0.12));
                    }
                    if let Ok(path) = stroke.build() {
                        window.paint_path(path, theme.chart_series[1]);
                    }
                    if let Some((time, bytes)) = timeline.get(selected) {
                        let at = point(px(x(*time)), px(y(*bytes)));
                        window.paint_quad(fill(
                            Bounds::new(point(at.x, px(top)), size(px(1.), px(bottom - top))),
                            theme.foreground.opacity(0.5),
                        ));
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(at.x - px(3.5), at.y - px(3.5)),
                                    size(px(7.), px(7.)),
                                ),
                                theme.foreground,
                            )
                            .corner_radii(px(4.))
                            .border_widths(px(1.5))
                            .border_color(theme.background),
                        );
                    }
                    let positions: Vec<_> = timeline
                        .iter()
                        .enumerate()
                        .map(|(index, point)| (index, x(point.0)))
                        .collect();
                    let entity = entity.clone();
                    let hitbox = hitbox.clone();
                    window.on_mouse_event(
                        move |event: &gpui::MouseDownEvent, phase, window, cx| {
                            if phase != gpui::DispatchPhase::Bubble
                                || event.button != gpui::MouseButton::Left
                                || !hitbox.is_hovered(window)
                            {
                                return;
                            }
                            let selected = positions
                                .iter()
                                .min_by(|a, b| {
                                    (a.1 - f32::from(event.position.x))
                                        .abs()
                                        .total_cmp(&(b.1 - f32::from(event.position.x)).abs())
                                })
                                .map(|point| point.0);
                            entity.update(cx, |this, cx| {
                                this.allocation_ui.point = selected;
                                cx.notify();
                            });
                        },
                    );
                },
            )
            .w_full()
            .max_h(px(192.))
            .map(|mut chart| {
                chart.style().aspect_ratio = Some(1000. / 172.);
                chart
            }),
        )
}

fn dashed_line(
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
    color: gpui::Hsla,
    window: &mut gpui::Window,
) {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    let length = dx.hypot(dy).max(1.);
    let mut at = 0.;
    while at < length {
        let end = (at + 3.).min(length);
        let mut path = gpui::PathBuilder::stroke(px(1.));
        path.move_to(gpui::point(
            from.x + px(dx * at / length),
            from.y + px(dy * at / length),
        ));
        path.line_to(gpui::point(
            from.x + px(dx * end / length),
            from.y + px(dy * end / length),
        ));
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
        at += 7.;
    }
}
impl ShellView {
    /// Wait for the same filtered summaries the allocation table renders.
    pub fn allocation_tour_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .session
            .as_ref()
            .is_none_or(|session| session.allocations.is_none())
        {
            return true;
        }
        if self.active_view() != Some(ViewId::Allocations) {
            let index = self
                .views()
                .iter()
                .position(|view| *view == ViewId::Allocations)
                .expect("allocation data offers the allocation view");
            self.set_active_tab(index, cx);
        }
        !self.allocation_sites.stale(self.analysis_key())
    }

    /// Exercises the allocation data and normal source-tab path on a real recording.
    pub fn tour_allocations(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let Some(session) = self.session.clone() else {
            return Ok(());
        };
        let Some(data) = &session.allocations else {
            return Ok(());
        };
        anyhow::ensure!(data.error.is_none(), "allocation data: {:?}", data.error);
        anyhow::ensure!(
            data.lifetimes.len() as u64 == data.count,
            "allocation row count differs from summary"
        );
        anyhow::ensure!(
            data.lifetimes
                .iter()
                .map(|lifetime| lifetime.bytes)
                .sum::<u64>()
                == data.bytes,
            "allocation requested bytes differ from summary"
        );
        println!(
            "tour: allocation_data {}",
            serde_json::json!({"count":data.count,"requested_bytes":data.bytes,"complete":data.complete,"stacks":data.stacks.len()})
        );
        let sites = self
            .allocation_sites
            .latest()
            .expect("tour waits for allocation summaries");
        for (stack, site) in sites.iter() {
            println!(
                "tour: allocation_site {}",
                serde_json::json!({
                    "stack_id": stack,
                    "count": site.count,
                    "requested_bytes": site.requested,
                    "live_bytes": site.live,
                    "peak_bytes": site.peak,
                    "call_site": site.title,
                })
            );
        }
        let source = data.stacks.iter().find_map(|(stack, ids)| {
            ids.iter()
                .find(|id| {
                    session.profile.frames[**id]
                        .file
                        .as_ref()
                        .is_some_and(|file| std::path::Path::new(file).is_file())
                })
                .map(|id| (*stack, *id))
        });
        if let Some((stack, id)) = source {
            self.allocation_stack = Some(stack);
            self.open_source_tab(id, cx);
            let document = self
                .source_tabs
                .iter()
                .find(|tab| tab.frame_id == id)
                .and_then(|tab| tab.document.as_ref())
                .ok_or_else(|| anyhow::anyhow!("allocation source tab has no document"))?;
            anyhow::ensure!(
                document.error.is_none() && !document.lines.is_empty(),
                "allocation source failed: {:?}",
                document.error
            );
            println!(
                "tour: allocation_source {}",
                serde_json::json!({"stack_id":stack,"frame_id":id,"source":document.path})
            );
        }
        Ok(())
    }
}
