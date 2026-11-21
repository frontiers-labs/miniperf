//! Flame graph canvas: one row per stack depth, width proportional to the
//! selected weight, colored by module kind. Click selects, double-click zooms
//! into the node, and symbol-filter matches stay lit while the rest dims.

use std::sync::Arc;

use gpui::{
    Bounds, CursorStyle, DispatchPhase, Entity, HitboxBehavior, MouseDownEvent, MouseMoveEvent,
    Window, canvas, fill, point, prelude::*, px, size,
};

use super::ShellView;
use super::session::ShellSession;
use crate::charts::truncate_label;
use crate::profile_analysis::IcicleLayout;
use crate::ui::Theme;

pub const ROW_H: f32 = 17.0;

/// Everything one flame paint needs, cloned per frame.
#[derive(Clone)]
pub struct FlameView {
    pub session: Arc<ShellSession>,
    pub layout: Arc<IcicleLayout>,
    pub symbol_query: String,
    pub selected_frame: Option<usize>,
}

/// Where the tooltip sits and which node it describes.
#[derive(Clone, Copy, PartialEq)]
pub struct FlameHover {
    pub node_id: usize,
    pub x: f32,
    pub y: f32,
}

impl FlameView {
    pub fn height(&self) -> f32 {
        (self.layout.max_depth + 1) as f32 * ROW_H
    }

    fn hit_test(
        &self,
        bounds: &Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
    ) -> Option<usize> {
        let width = f32::from(bounds.size.width).max(1.0);
        let x = (f32::from(position.x) - f32::from(bounds.left())) as f64 / width as f64;
        let depth = ((f32::from(position.y) - f32::from(bounds.top())) / ROW_H).floor();
        if depth < 0.0 {
            return None;
        }
        let depth = depth as usize;
        self.layout
            .frames
            .iter()
            .find(|frame| frame.depth == depth && x >= frame.x && x <= frame.x + frame.width)
            .map(|frame| frame.node_id)
    }
}

pub fn flame_canvas(entity: Entity<ShellView>, theme: Theme, view: FlameView) -> impl IntoElement {
    let height = view.height();
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            window.paint_quad(fill(bounds, theme.background));

            let width = f32::from(bounds.size.width);
            let left = f32::from(bounds.left());
            let top = f32::from(bounds.top());
            let query = view.symbol_query.trim().to_lowercase();

            for frame in &view.layout.frames {
                let x = left + (frame.x as f32) * width;
                let w = ((frame.width as f32) * width - 1.0).max(0.5);
                let y = top + frame.depth as f32 * ROW_H;

                let profile_frame = frame
                    .frame_id
                    .and_then(|id| view.session.profile.frames.get(id));
                let matches = query.is_empty()
                    || profile_frame.map_or_else(
                        || frame.label.to_lowercase().contains(&query),
                        |frame| {
                            frame.matches_symbol(&query)
                                || frame
                                    .module
                                    .as_ref()
                                    .is_some_and(|module| module.to_lowercase().contains(&query))
                        },
                    );
                let base = match frame.frame_id {
                    Some(frame_id) => match view.session.module_label(frame_id) {
                        Some((_, kind)) => theme.module_kind_color(kind),
                        None => theme.viz.series[0],
                    },
                    None => theme.muted_foreground,
                };
                let selected = frame
                    .frame_id
                    .is_some_and(|frame_id| view.selected_frame == Some(frame_id));
                let color = if matches { base } else { theme.muted };
                let alpha = if matches { 0.85 } else { 1.0 };

                let rect = Bounds::new(point(px(x), px(y)), size(px(w), px(ROW_H - 1.0)));
                window.paint_quad(fill(rect, color.opacity(alpha)));
                if selected {
                    window.paint_quad(gpui::outline(rect, theme.viz.ink, gpui::BorderStyle::Solid));
                }

                if w > 28.0 {
                    let max_chars = ((w - 8.0) / 6.6) as usize;
                    let label_color = if matches {
                        gpui::white()
                    } else {
                        theme.muted_foreground
                    };
                    let label = truncate_label(
                        profile_frame.map_or(frame.label.as_str(), |frame| &frame.friendly_name),
                        max_chars,
                    );
                    let mut run = window.text_style().to_run(label.len());
                    run.font.family = theme.font_mono.clone();
                    run.color = label_color;
                    let line =
                        window
                            .text_system()
                            .shape_line(label.into(), px(11.0), &[run], None);
                    let _ = line.paint(point(px(x + 4.0), px(y)), px(16.0), window, cx);
                }
            }

            register_mouse(&entity, bounds, &hitbox, &view, window);
        },
    )
    .w_full()
    .h(px(height))
}

fn register_mouse(
    entity: &Entity<ShellView>,
    bounds: Bounds<gpui::Pixels>,
    hitbox: &gpui::Hitbox,
    view: &FlameView,
    window: &mut Window,
) {
    let move_hitbox = hitbox.clone();
    let move_view = view.clone();
    let move_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let hovered = move_hitbox
            .is_hovered(window)
            .then(|| move_view.hit_test(&bounds, event.position))
            .flatten();
        move_entity.update(cx, |this, cx| {
            if this.flame_hover.map(|hover| hover.node_id) != hovered {
                this.flame_hover = hovered.map(|node_id| FlameHover {
                    node_id,
                    x: f32::from(event.position.x) - f32::from(bounds.left()),
                    y: f32::from(event.position.y) - f32::from(bounds.top()),
                });
                cx.notify();
            }
        });
    });

    let down_hitbox = hitbox.clone();
    let down_view = view.clone();
    let down_entity = entity.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !down_hitbox.is_hovered(window) {
            return;
        }
        let Some(node_id) = down_view.hit_test(&bounds, event.position) else {
            return;
        };
        let frame_id = down_view
            .layout
            .frames
            .iter()
            .find(|frame| frame.node_id == node_id)
            .and_then(|frame| frame.frame_id);
        down_entity.update(cx, |this, cx| {
            if event.click_count >= 2 {
                this.zoom_flame(node_id, cx);
            } else {
                this.select_frame(frame_id, cx);
            }
        });
    });
}
