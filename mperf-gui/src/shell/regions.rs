//! Region tracks and the call-path table share one selection with profile views.
use std::{collections::BTreeMap, sync::Arc};

use gpui::{
    Bounds, Context, CursorStyle, DispatchPhase, Entity, HitboxBehavior, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, canvas, div, fill, point, prelude::*, px, size,
};

use super::{
    ShellView,
    session::{ShellSession, format_count},
    timeline::{self, TimelineView},
};
use crate::{
    charts::{PlotFrame, shape_label, truncate_label},
    profile::TimeRange,
    regions::{RegionNodeId, RegionSelection, RegionSpanId, SpanBoundary},
    ui::{self, ActiveTheme, Column, Icon, Theme, icon, table_cell, table_row},
};

const ROW_H: f32 = 20.0;

#[derive(Clone)]
struct RegionLane {
    process_id: u32,
    thread_id: u32,
    row: usize,
    rows: usize,
}

fn lanes(session: &ShellSession) -> Vec<RegionLane> {
    let mut depths = BTreeMap::<(u32, u32), usize>::new();
    for span in &session.regions.spans {
        depths
            .entry((span.process_id, span.thread_id))
            .and_modify(|depth| *depth = (*depth).max(span.depth + 1))
            .or_insert(span.depth + 1);
    }
    let mut row = 0;
    depths
        .into_iter()
        .map(|((process_id, thread_id), rows)| {
            let lane = RegionLane {
                process_id,
                thread_id,
                row,
                rows,
            };
            row += rows;
            lane
        })
        .collect()
}

pub fn canvas_view(
    entity: Entity<ShellView>,
    theme: Theme,
    view: TimelineView,
    selection: Option<RegionSelection>,
) -> impl IntoElement {
    let lanes = lanes(&view.session);
    let height =
        lanes.iter().map(|lane| lane.rows).sum::<usize>() as f32 * ROW_H + timeline::AXIS_H;
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.paint_quad(fill(bounds, theme.viz.surface));
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            let frame = PlotFrame::new(bounds, timeline::GUTTER);
            let Some(range) = view.session.full_range else {
                return;
            };
            let duration = (range.end_ns - range.start_ns).max(1) as f64;
            for lane in &lanes {
                let label = format!("{}:{}", lane.process_id, lane.thread_id);
                let line = shape_label(&truncate_label(&label, 14), 10.0, theme.viz.ink_2, window);
                let _ = line.paint(
                    point(
                        bounds.left() + px(6.0),
                        px(frame.top() + lane.row as f32 * ROW_H + 3.0),
                    ),
                    px(ROW_H),
                    window,
                    cx,
                );
            }
            let mut rectangles = Vec::new();
            for (index, span) in view.session.regions.spans.iter().enumerate() {
                let lane = lanes
                    .iter()
                    .find(|lane| {
                        lane.process_id == span.process_id && lane.thread_id == span.thread_id
                    })
                    .unwrap();
                let x = frame.left()
                    + (span.range.start_ns.saturating_sub(range.start_ns) as f64 / duration) as f32
                        * frame.width();
                let right = frame.left()
                    + (span.range.end_ns.saturating_sub(range.start_ns) as f64 / duration) as f32
                        * frame.width();
                let width = (right - x)
                    .max(1.0)
                    .min((frame.left() + frame.width() - x).max(1.0));
                let y = frame.top() + (lane.row + span.depth) as f32 * ROW_H;
                let rect = Bounds::new(point(px(x), px(y + 1.0)), size(px(width), px(ROW_H - 2.0)));
                let selected = selection == Some(RegionSelection::Node(span.node))
                    || selection == Some(RegionSelection::Invocation(RegionSpanId(index)));
                let alpha = if view.disabled_threads.contains(&span.thread_id) {
                    0.18
                } else if selected {
                    1.0
                } else {
                    0.65
                };
                let color = if span.boundary == SpanBoundary::Complete {
                    theme.viz.series[span.node.0 % theme.viz.series.len()]
                } else {
                    theme.viz.muted
                };
                window.paint_quad(fill(rect, color.opacity(alpha)));
                if width > 28.0 {
                    let node = &view.session.regions.nodes[span.node.0];
                    let name = if span.boundary == SpanBoundary::Complete {
                        node.name.clone()
                    } else {
                        format!("{} · incomplete", node.name)
                    };
                    let line = shape_label(
                        &truncate_label(&name, ((width - 6.0) / 6.0) as usize),
                        10.0,
                        theme.viz.ink,
                        window,
                    );
                    let _ = line.paint(point(px(x + 3.0), px(y + 3.0)), px(ROW_H), window, cx);
                }
                rectangles.push((rect, RegionSpanId(index)));
            }
            frame.paint_time_axis(
                frame.top() + height - timeline::AXIS_H,
                duration / 1e9,
                &theme,
                window,
                cx,
            );
            if let Some(selection) = view.preview.or(view.selection) {
                frame.paint_selection(
                    selection,
                    duration / 1e9,
                    frame.top(),
                    height - timeline::AXIS_H,
                    &theme,
                    window,
                );
            }
            let down_entity = entity.clone();
            let down_hitbox = hitbox.clone();
            let down_rectangles = rectangles.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !down_hitbox.is_hovered(window)
                {
                    return;
                }
                if let Some((_, id)) = down_rectangles
                    .iter()
                    .rev()
                    .find(|(rect, _)| rect.contains(&event.position))
                {
                    down_entity.update(cx, |this, cx| {
                        this.select_region(Some(RegionSelection::Invocation(*id)), cx)
                    });
                }
            });
            let hover_entity = entity.clone();
            let hover_hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let hovered = hover_hitbox
                    .is_hovered(window)
                    .then(|| {
                        rectangles
                            .iter()
                            .rev()
                            .find(|(rect, _)| rect.contains(&event.position))
                            .map(|(_, id)| *id)
                    })
                    .flatten();
                hover_entity.update(cx, |this, cx| {
                    if this.region_hover != hovered {
                        this.region_hover = hovered;
                        cx.notify();
                    }
                });
            });
        },
    )
    .w_full()
    .h(px(height))
    .flex_none()
}

fn overlap(range: TimeRange, time: Option<TimeRange>) -> u64 {
    let time = time.unwrap_or(range);
    range
        .end_ns
        .min(time.end_ns)
        .saturating_sub(range.start_ns.max(time.start_ns))
}

impl ShellView {
    pub(super) fn select_region(
        &mut self,
        selection: Option<RegionSelection>,
        cx: &mut Context<Self>,
    ) {
        self.filter.region = if self.filter.region == selection {
            None
        } else {
            selection
        };
        self.refresh_analysis(cx);
        cx.notify();
    }

    pub(super) fn render_regions(
        &self,
        session: &Arc<ShellSession>,
        view: TimelineView,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let columns = [
            Column::new("Region"),
            Column::new("Count").width(70.0).right(),
            Column::new("Incomplete").width(80.0).right(),
            Column::new("Inclusive").width(95.0).right(),
            Column::new("Self").width(95.0).right(),
            Column::new("Mean").width(95.0).right(),
        ];
        let mut rows = Vec::new();
        let mut pending: Vec<_> = session
            .regions
            .roots
            .iter()
            .rev()
            .map(|&id| (id, 0))
            .collect();
        while let Some((id, depth)) = pending.pop() {
            rows.push((id, depth));
            if !self.collapsed_regions.contains(&id) {
                pending.extend(
                    session.regions.nodes[id.0]
                        .children
                        .iter()
                        .rev()
                        .map(|&child| (child, depth + 1)),
                );
            }
        }
        let hover = if let Some(id) = self.region_hover {
            let span = &session.regions.spans[id.0];
            let node = &session.regions.nodes[span.node.0];
            let status = match span.boundary {
                SpanBoundary::Complete => "complete",
                SpanBoundary::MissingBegin => "missing begin, no sample attribution",
                SpanBoundary::MissingEnd => "missing end, no sample attribution",
                SpanBoundary::CrossThread => "cross-thread end, no sample attribution",
            };
            format!(
                "{} · {} · pid {} tid {} · {:.3} ms · {status}",
                node.name,
                node.context,
                span.process_id,
                span.thread_id,
                (span.range.end_ns - span.range.start_ns) as f64 / 1e6
            )
        } else {
            "Click a span to scope one invocation. Select a region below to scope all its invocations.".to_string()
        };
        let mut row_elements = Vec::new();
        for (id, depth) in rows {
            let node = &session.regions.nodes[id.0];
            let mut count = 0u64;
            let mut incomplete = 0u64;
            let mut inclusive = 0u64;
            let mut own = 0u64;
            for span_id in &node.spans {
                let span = &session.regions.spans[span_id.0];
                let visible = self.filter.time.is_none_or(|time| {
                    if span.range.start_ns == span.range.end_ns {
                        span.range.start_ns >= time.start_ns && span.range.start_ns < time.end_ns
                    } else {
                        overlap(span.range, Some(time)) > 0
                    }
                });
                if self.filter.disabled_threads.contains(&span.thread_id) || !visible {
                    continue;
                }
                if span.boundary == SpanBoundary::Complete {
                    count += 1;
                    inclusive += overlap(span.range, self.filter.time);
                    own += span
                        .self_ranges
                        .iter()
                        .map(|&range| overlap(range, self.filter.time))
                        .sum::<u64>();
                } else {
                    incomplete += 1;
                }
            }
            let collapsed = self.collapsed_regions.contains(&id);
            let collapse_entity = cx.entity();
            let select_entity = cx.entity();
            let title = format!(
                "{}\n{}\nTimings include completed spans in the selected time and threads. Incomplete spans are excluded.",
                node.name, node.context
            );
            let row = table_row(self.filter.region == Some(RegionSelection::Node(id)), cx)
                .id(("region", id.0))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    select_entity.update(cx, |this, cx| {
                        this.select_region(Some(RegionSelection::Node(id)), cx)
                    });
                })
                .child(
                    table_cell(&columns[0])
                        .pl(px(8.0 + depth as f32 * 14.0))
                        .child(
                            div()
                                .id(("region-collapse", id.0))
                                .w(px(20.0))
                                .flex_none()
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    collapse_entity.update(cx, |this, cx| {
                                        if !this.collapsed_regions.remove(&id) {
                                            this.collapsed_regions.insert(id);
                                        }
                                        cx.notify();
                                    });
                                })
                                .when(!node.children.is_empty(), |el| {
                                    el.child(
                                        icon(if collapsed {
                                            Icon::ChevronRight
                                        } else {
                                            Icon::ChevronDown
                                        })
                                        .size(px(12.0)),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .id(("region-name", id.0))
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .cursor_pointer()
                                .tooltip(ui::tooltip::text_tooltip(title))
                                .child(node.name.clone()),
                        ),
                )
                .child(table_cell(&columns[1]).child(format_count(count as f64)))
                .child(table_cell(&columns[2]).child(incomplete.to_string()))
                .child(table_cell(&columns[3]).child(format!("{:.3} ms", inclusive as f64 / 1e6)))
                .child(table_cell(&columns[4]).child(format!("{:.3} ms", own as f64 / 1e6)))
                .child(table_cell(&columns[5]).child(if count > 0 {
                    format!("{:.3} ms", inclusive as f64 / count as f64 / 1e6)
                } else {
                    "—".to_string()
                }));
            row_elements.push(row);
        }
        div()
            .flex()
            .flex_col()
            .flex_none()
            .child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .child(ui::section_caption("named regions · nested by thread", cx)),
            )
            .child(canvas_view(
                cx.entity(),
                theme.clone(),
                view,
                self.filter.region,
            ))
            .child(
                div()
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(hover),
            )
            .child(ui::table_header(&columns, None, cx))
            .children(row_elements)
            .into_any_element()
    }
}

impl ShellView {
    /// Reports and selects real loaded region nodes for the viewer's command-line tour.
    pub fn region_tour_summary(&self) -> anyhow::Result<usize> {
        let session = self
            .session
            .as_ref()
            .expect("tour requires a loaded session");
        if let Some(error) = &session.regions.error {
            anyhow::bail!("{error}");
        }
        let completed: u64 = session.regions.nodes.iter().map(|node| node.count).sum();
        let incomplete: u64 = session
            .regions
            .nodes
            .iter()
            .map(|node| node.incomplete)
            .sum();
        println!(
            "tour: regions nodes={} completed={completed} incomplete={incomplete}",
            session.regions.nodes.len()
        );
        Ok(session.regions.nodes.len())
    }

    pub fn tour_region(&mut self, index: usize, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let session = self
            .session
            .clone()
            .expect("tour requires a loaded session");
        let id = RegionNodeId(index);
        let selection = RegionSelection::Node(id);
        self.select_region(Some(selection), cx);
        let report_scope = |filter: &super::session::GlobalFilter,
                            kind: &str,
                            invocation: Option<RegionSpanId>|
         -> anyhow::Result<serde_json::Value> {
            let resolved = filter.resolve(&session);
            let samples = session
                .profile
                .samples
                .iter()
                .filter(|sample| resolved.matches(sample))
                .count() as u64;
            let analysis =
                crate::profile_analysis::FunctionAnalysis::build(&session.profile, &resolved);
            anyhow::ensure!(
                analysis.total_samples == samples,
                "region scope disagrees with hotspot sample count"
            );
            let range = filter.time.map(
                |range| serde_json::json!({ "start_ns": range.start_ns, "end_ns": range.end_ns }),
            );
            let invocation = invocation.map(|id| {
                let span = &session.regions.spans[id.0];
                serde_json::json!({ "instance": span.instance, "pid": span.process_id, "tid": span.thread_id })
            });
            Ok(
                serde_json::json!({ "kind": kind, "range": range, "disabled_threads": filter.disabled_threads, "invocation": invocation, "samples": samples }),
            )
        };
        let base = report_scope(&self.filter, "node", None)?;
        let filter = self.filter.resolve(&session);
        let intervals = filter
            .regions
            .as_ref()
            .expect("region selection resolves intervals");
        let mut scopes = Vec::new();
        if let Some(range) = session.full_range {
            let duration = (range.end_ns - range.start_ns) as f64 / 1e9;
            self.commit_time_filter(Some((duration * 0.25, duration * 0.75)), cx);
            scopes.push(report_scope(&self.filter, "time", None)?);
            self.commit_time_filter(None, cx);
        }
        let node = &session.regions.nodes[index];
        if let Some(id) = node.spans.first() {
            let thread = session.regions.spans[id.0].thread_id;
            self.filter.disabled_threads.insert(thread);
            self.refresh_analysis(cx);
            scopes.push(report_scope(&self.filter, "thread", None)?);
            self.filter.disabled_threads.remove(&thread);
            self.refresh_analysis(cx);
        }
        if let Some(&id) = node
            .spans
            .iter()
            .find(|id| session.regions.spans[id.0].boundary == SpanBoundary::Complete)
        {
            self.select_region(Some(RegionSelection::Invocation(id)), cx);
            scopes.push(report_scope(&self.filter, "invocation", Some(id))?);
            self.select_region(Some(selection), cx);
        }
        let node = &session.regions.nodes[index];
        let mut path = vec![node.name.clone()];
        let mut parent = node.parent;
        while let Some(id) = parent {
            let node = &session.regions.nodes[id.0];
            path.push(node.name.clone());
            parent = node.parent;
        }
        path.reverse();
        println!(
            "tour: region {}",
            serde_json::json!({
                "path": path, "context": node.context, "count": node.count, "incomplete": node.incomplete,
                "inclusive_ns": node.inclusive_ns, "self_ns": node.self_ns,
                "samples": base["samples"], "intervals": intervals.by_thread.values().map(Vec::len).sum::<usize>(),
                "threads": intervals.by_thread.keys().collect::<Vec<_>>(), "scopes": scopes
            })
        );
        Ok(())
    }

    pub fn finish_region_tour(&mut self, cx: &mut Context<Self>) {
        self.filter.region = None;
        self.refresh_analysis(cx);
        cx.notify();
    }
}
