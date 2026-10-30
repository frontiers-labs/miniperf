//! Region tracks and the call-path table share one selection with profile views.
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

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
    ui::{self, ActiveTheme, Column, Icon, Theme, empty_state, icon, table_cell, table_row},
};

const ROW_H: f32 = 20.0;

struct RegionLane {
    label: String,
    row: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionHover {
    Invocation(RegionSpanId),
    Density(usize),
}

struct RegionItem {
    left: f32,
    right: f32,
    hover: RegionHover,
    label: Option<String>,
}

type RegionRaster = Vec<Vec<RegionItem>>;

pub(super) struct RegionTimeline {
    lanes: Vec<RegionLane>,
    rows: Vec<Vec<RegionSpanId>>,
    raster: RefCell<Option<(f32, Arc<RegionRaster>)>>,
}

impl RegionTimeline {
    pub(super) fn new(session: &ShellSession) -> Self {
        let mut depths = BTreeMap::<(u32, u32), usize>::new();
        for span in &session.regions.spans {
            depths
                .entry((span.process_id, span.thread_id))
                .and_modify(|depth| *depth = (*depth).max(span.depth + 1))
                .or_insert(span.depth + 1);
        }
        let mut lanes = Vec::new();
        let mut lane_rows = BTreeMap::new();
        let mut rows = Vec::new();
        for ((pid, tid), depth) in depths {
            let row = rows.len();
            lanes.push(RegionLane {
                label: format!("{pid}:{tid}"),
                row,
            });
            lane_rows.insert((pid, tid), row);
            rows.resize_with(row + depth, Vec::new);
        }
        for (index, span) in session.regions.spans.iter().enumerate() {
            let row = lane_rows[&(span.process_id, span.thread_id)] + span.depth;
            rows[row].push(RegionSpanId(index));
        }
        Self {
            lanes,
            rows,
            raster: RefCell::new(None),
        }
    }

    fn raster(&self, session: &ShellSession, width: f32) -> Arc<RegionRaster> {
        let mut cached = self.raster.borrow_mut();
        if let Some((cached_width, raster)) = &*cached
            && *cached_width == width
        {
            return raster.clone();
        }
        let range = session
            .full_range
            .expect("region timeline has a recording range");
        let duration = (range.end_ns - range.start_ns).max(1) as f64;
        let columns = width.ceil() as usize;
        let rows: RegionRaster = self
            .rows
            .iter()
            .map(|spans| {
                let mut bins = BTreeMap::<usize, RegionItem>::new();
                for &id in spans {
                    let span = &session.regions.spans[id.0];
                    let left = ((span.range.start_ns.saturating_sub(range.start_ns) as f64
                        / duration) as f32
                        * width)
                        .min(width);
                    let right = ((span.range.end_ns.saturating_sub(range.start_ns) as f64
                        / duration) as f32
                        * width)
                        .min(width);
                    let column = (left as usize).min(columns - 1);
                    let visible = right - left >= 1.0;
                    if let Some(item) = bins.get_mut(&column) {
                        if matches!(item.hover, RegionHover::Invocation(_)) {
                            continue;
                        }
                        if !visible {
                            if let RegionHover::Density(count) = &mut item.hover {
                                *count += 1;
                            }
                            continue;
                        }
                    }
                    let label = (right - left > 28.0).then(|| {
                        let node = &session.regions.nodes[span.node.0];
                        let name = if span.boundary == SpanBoundary::Complete {
                            node.name.clone()
                        } else {
                            format!("{} · incomplete", node.name)
                        };
                        truncate_label(&name, ((right - left - 6.0) / 6.0) as usize)
                    });
                    bins.insert(
                        column,
                        RegionItem {
                            left: if visible { left } else { column as f32 },
                            right: if visible {
                                right
                            } else {
                                (column as f32 + 1.0).min(width)
                            },
                            hover: if visible {
                                RegionHover::Invocation(id)
                            } else {
                                RegionHover::Density(1)
                            },
                            label,
                        },
                    );
                }
                bins.into_values().collect()
            })
            .collect();
        let raster = Arc::new(rows);
        *cached = Some((width, raster.clone()));
        raster
    }
}

fn hit_test(raster: &RegionRaster, x: f32, y: f32) -> Option<RegionHover> {
    if x < 0.0 || y < 0.0 || y % ROW_H < 1.0 || y % ROW_H >= ROW_H - 1.0 {
        return None;
    }
    let row = raster.get((y / ROW_H) as usize)?;
    // Exact span bounds win where a density pixel overlaps its neighbor.
    row.iter()
        .find(|item| {
            matches!(item.hover, RegionHover::Invocation(_)) && x >= item.left && x < item.right
        })
        .or_else(|| row.iter().find(|item| x >= item.left && x < item.right))
        .map(|item| item.hover)
}

fn canvas_view(
    entity: Entity<ShellView>,
    theme: Theme,
    view: TimelineView,
    selection: Option<RegionSelection>,
    timeline: Rc<RegionTimeline>,
) -> impl IntoElement {
    let height = timeline.rows.len() as f32 * ROW_H + timeline::AXIS_H;
    let prepaint_timeline = timeline.clone();
    let prepaint_session = view.session.clone();
    canvas(
        move |bounds, window, _| {
            let frame = PlotFrame::new(bounds, timeline::GUTTER);
            let raster = prepaint_timeline.raster(&prepaint_session, frame.width());
            (window.insert_hitbox(bounds, HitboxBehavior::Normal), raster)
        },
        move |bounds, (hitbox, raster), window, cx| {
            window.paint_quad(fill(bounds, theme.viz.surface));
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            let frame = PlotFrame::new(bounds, timeline::GUTTER);
            let Some(range) = view.session.full_range else {
                return;
            };
            let duration = (range.end_ns - range.start_ns).max(1) as f64;
            for lane in &timeline.lanes {
                let line = shape_label(
                    &truncate_label(&lane.label, 14),
                    10.0,
                    theme.viz.ink_2,
                    window,
                );
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
            for (row, items) in raster.iter().enumerate() {
                let y = frame.top() + row as f32 * ROW_H;
                for item in items {
                    let x = frame.left() + item.left;
                    let rect = Bounds::new(
                        point(px(x), px(y + 1.0)),
                        size(px(item.right - item.left), px(ROW_H - 2.0)),
                    );
                    let (color, alpha) = match item.hover {
                        RegionHover::Invocation(id) => {
                            let span = &view.session.regions.spans[id.0];
                            let selected = selection == Some(RegionSelection::Node(span.node))
                                || selection == Some(RegionSelection::Invocation(id));
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
                            (color, alpha)
                        }
                        RegionHover::Density(_) => (theme.viz.muted, 0.65),
                    };
                    window.paint_quad(fill(rect, color.opacity(alpha)));
                    if let Some(label) = &item.label {
                        let line = shape_label(label, 10.0, theme.viz.ink, window);
                        let _ = line.paint(point(px(x + 3.0), px(y + 3.0)), px(ROW_H), window, cx);
                    }
                }
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
            let down_raster = raster.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !down_hitbox.is_hovered(window)
                {
                    return;
                }
                if let Some(RegionHover::Invocation(id)) = hit_test(
                    &down_raster,
                    f32::from(event.position.x) - frame.left(),
                    f32::from(event.position.y) - frame.top(),
                ) {
                    down_entity.update(cx, |this, cx| {
                        this.select_region(Some(RegionSelection::Invocation(id)), cx)
                    });
                }
            });
            let hover_entity = entity.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let hovered = hitbox
                    .is_hovered(window)
                    .then(|| {
                        hit_test(
                            &raster,
                            f32::from(event.position.x) - frame.left(),
                            f32::from(event.position.y) - frame.top(),
                        )
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

    pub(super) fn render_region_timeline(
        &self,
        session: &Arc<ShellSession>,
        view: TimelineView,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let hover = if let Some(RegionHover::Invocation(id)) = self.region_hover {
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
        } else if let Some(RegionHover::Density(count)) = self.region_hover {
            format!(
                "{count} subpixel invocations share this pixel. Select their region in Events to scope them."
            )
        } else {
            "Click a span to scope one invocation. Select a region in Events to scope all its invocations.".to_string()
        };
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
                self.region_timeline
                    .clone()
                    .expect("loaded session has a region timeline"),
            ))
            .child(
                div()
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(hover),
            )
            .into_any_element()
    }

    pub(super) fn render_events(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(error) = &session.regions.error {
            return empty_state(
                Icon::Layers,
                format!("Could not read named regions: {error}"),
            )
            .into_any_element();
        }
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
        let mut row_elements = Vec::new();
        for (id, depth) in rows {
            let node = &session.regions.nodes[id.0];
            let mut count = 0u64;
            let mut incomplete = 0u64;
            let mut inclusive = 0u64;
            let mut own = 0u64;
            let mut full_duration = 0u64;
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
                    full_duration += span.range.end_ns - span.range.start_ns;
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
                "{}\n{}\nInclusive and Self are clipped to the selected time. Mean uses the full duration of completed invocations that overlap the selected time and threads. Incomplete spans are excluded.",
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
                    format!("{:.3} ms", full_duration as f64 / count as f64 / 1e6)
                } else {
                    "—".to_string()
                }));
            row_elements.push(row);
        }
        div()
            .id("events-view")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(6.0))
                    .child(ui::section_caption(
                        "named regions · select a call path to scope all invocations",
                        cx,
                    )),
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
        if !session.regions.spans.is_empty() {
            let timeline = self
                .region_timeline
                .as_ref()
                .expect("loaded session has a region timeline");
            let cached = timeline.raster.borrow();
            let (width, raster) = cached
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("region timeline was not rendered"))?;
            println!(
                "tour: region_render spans={} rows={} columns={} items={}",
                session.regions.spans.len(),
                raster.len(),
                width.ceil() as usize,
                raster.iter().map(Vec::len).sum::<usize>()
            );
        }
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
            let analysis =
                crate::profile_analysis::FunctionAnalysis::build(&session.profile, &resolved);
            let samples = analysis.total_samples;
            let balance = crate::profile_analysis::ThreadBalance::build(&session.profile, &resolved)
                .map(|balance| {
                    balance.rows.into_iter().map(|row| {
                        serde_json::json!({ "thread_id": row.thread_id, "busy_fraction": row.busy_fraction })
                    }).collect::<Vec<_>>()
                }).unwrap_or_default();
            let range = filter.time.map(
                |range| serde_json::json!({ "start_ns": range.start_ns, "end_ns": range.end_ns }),
            );
            let invocation = invocation.map(|id| {
                let span = &session.regions.spans[id.0];
                serde_json::json!({ "instance": span.instance, "pid": span.process_id, "tid": span.thread_id })
            });
            Ok(
                serde_json::json!({ "kind": kind, "range": range, "disabled_threads": filter.disabled_threads, "invocation": invocation, "samples": samples, "balance": balance }),
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
                "balance": base["balance"],
                "recording_range": session.full_range.map(|range| serde_json::json!({ "start_ns": range.start_ns, "end_ns": range.end_ns })),
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
