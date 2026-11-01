use super::{ShellView, ViewId, session::ShellSession};
use crate::{
    snapshot::{format_bytes, format_count},
    ui::{self, ActiveTheme},
};
use gpui::{Context, div, prelude::*, px};
use std::sync::Arc;

pub fn render(
    state: &ShellView,
    session: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let data = session.allocations.as_ref().unwrap();
    if let Some(error) = &data.error {
        return div()
            .p(px(12.))
            .child(format!("Allocation data could not load: {error}"))
            .into_any_element();
    }
    let selected = state.allocation_stack;
    let sites = state.analysis.allocation_sites.latest();
    let mut content = div()
        .id("allocations")
        .size_full()
        .overflow_y_scroll()
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(div().text_size(px(18.)).child("Allocations"))
        .child(
            div()
                .text_color(if data.complete {
                    theme.foreground
                } else {
                    theme.destructive
                })
                .child(if data.complete {
                    "Complete observed root-process operations"
                } else {
                    "Incomplete capture. Counts, live bytes and peaks describe received operations only."
                })
        )
        .child(
            div()
                .text_size(px(12.))
                .child(format!(
                    "Whole recording: {} allocations · {} requested · {} live at end · {} observed peak · {}/{} stack-attributed",
                    format_count(data.count as f64),
                    format_bytes(data.bytes as f64),
                    format_bytes(data.live as f64),
                    format_bytes(data.peak as f64),
                    format_count(data.attributed as f64),
                    format_count(data.count as f64)
                ))
        )
        .child(
            div()
                .text_color(theme.muted_foreground)
                .text_size(px(11.))
                .child("Requested bytes are not RSS. Live at end is not a leak claim. Realloc internal peaks are uncertain. The table selects allocations born in the selected time range and named region, with thread, module and symbol filters. Live bytes refer to that cohort at the selection end. The chart shows the whole recording.")
        );
    if let (Some(first), Some(last)) = (data.timeline.first(), data.timeline.last()) {
        let seconds = data
            .timeline
            .iter()
            .map(|point| (point.0 - first.0) as f64 / 1e9)
            .collect();
        let values = data.timeline.iter().map(|point| Some(point.1)).collect();
        content = content.child(
            ui::viz_card("Observed live requested bytes, bucket maxima").child(
                super::memory::track_canvas(
                    "Live bytes",
                    "bytes",
                    theme.viz.series[0],
                    theme.clone(),
                    seconds,
                    values,
                    ((last.0 - first.0) as f64 / 1e9).max(f64::EPSILON),
                ),
            ),
        );
    }
    let columns = [
        ui::Column::new("Call site"),
        ui::Column::new("Count").width(80.).right(),
        ui::Column::new("Requested").width(110.).right(),
        ui::Column::new("Live at end").width(110.).right(),
        ui::Column::new("Observed peak").width(110.).right(),
        ui::Column::new("Mean lifetime").width(110.).right(),
    ];
    let mut table = div()
        .flex()
        .flex_col()
        .text_size(px(11.))
        .child(ui::table_header(&columns, None, cx));
    for (stack, site) in sites.into_iter().flat_map(|sites| sites.iter()).take(200) {
        let stack = *stack;
        let lifetime = site
            .ended_lifetime
            .checked_div(site.ended_count)
            .map_or_else(|| "Unknown".into(), |value| format!("{value} ns"));
        table = table.child(
            ui::table_row(selected == Some(stack), cx)
                .id(("allocation-site", stack))
                .child(ui::table_cell(&columns[0]).child(site.title.clone()))
                .child(ui::table_cell(&columns[1]).child(format_count(site.count as f64)))
                .child(ui::table_cell(&columns[2]).child(format_bytes(site.requested as f64)))
                .child(ui::table_cell(&columns[3]).child(format_bytes(site.live as f64)))
                .child(ui::table_cell(&columns[4]).child(format_bytes(site.peak as f64)))
                .child(ui::table_cell(&columns[5]).child(lifetime))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.allocation_stack = Some(stack);
                    cx.notify();
                })),
        );
    }
    let title = if state.analysis.allocation_sites_stale() {
        "Call sites by requested bytes, first 200 · updating"
    } else {
        "Call sites by requested bytes, first 200"
    };
    content = content.child(ui::viz_card(title).child(table));
    if let Some(stack) = selected {
        let mut frames = div().flex().flex_col().gap(px(4.));
        if let Some(ids) = data.stacks.get(&stack) {
            for &id in ids {
                let frame = &session.profile.frames[id];
                frames = frames.child(
                    div()
                        .id(("allocation-frame", id))
                        .cursor_pointer()
                        .child(format!(
                            "{}  {}:{}",
                            frame.friendly_name,
                            frame.file.clone().unwrap_or_default(),
                            frame.line.unwrap_or(0)
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_source_tab(id, cx))),
                );
            }
        } else {
            frames = frames
                .child("Stack attribution was not selected or could not unwind this allocation.");
        }
        content = content.child(
            ui::viz_card("Allocation call stack; select a frame to inspect full name and source")
                .child(frames),
        );
    }
    content.into_any_element()
}
impl ShellView {
    /// Wait for the same filtered summaries the allocation table renders.
    pub fn allocation_tour_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.allocations.is_some())
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
        !self.analysis.allocation_sites_stale()
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
            .analysis
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
