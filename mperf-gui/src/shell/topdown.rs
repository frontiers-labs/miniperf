//! Shared Top-Down hierarchy for Resources and the function inspector.

use gpui::{FontWeight, div, prelude::*, px};

use super::clock_badge;
use super::session::ShellSession;
use super::{ShellView, tma::TmaData, tma_category};
use crate::profile::ProfileFrame;
use crate::ui::{self, ActiveTheme, Theme, badge, info_tooltip};

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

pub(super) fn hierarchy(
    session: &ShellSession,
    frame: Option<&ProfileFrame>,
    cx: &mut Context<ShellView>,
) -> Option<gpui::AnyElement> {
    let tma = session.tma.as_ref()?;
    let theme = cx.theme().clone();
    let function_values = frame.and_then(|frame| tma.function_hierarchy_values(frame));
    let value = |index: usize| match frame {
        Some(_) => function_values.and_then(|values| values[index]),
        None => tma.rows[index].value,
    };
    let rows = hierarchy_rows(tma, value, &theme, cx);
    if frame.is_none() && !tma.has_hierarchy() && tma.error.is_none() {
        return None;
    }

    Some(
        div()
            .id("topdown-hierarchy")
            .flex_none()
            .w_full()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .child(ui::section_caption("Top-Down hierarchy", cx))
                    .when(frame.is_none(), |el| {
                        el.children(clock_badge(session, &theme))
                    }),
            )
            .when_some(
                tma.error.as_ref().filter(|_| frame.is_none()).cloned(),
                |el, error| {
                    el.child(
                        div()
                            .px(px(8.0))
                            .pb(px(6.0))
                            .text_size(px(11.0))
                            .text_color(theme.destructive)
                            .child(error),
                    )
                },
            )
            .when(frame.is_some() && rows.is_empty(), |el| {
                el.child(
                    div()
                        .px(px(8.0))
                        .pb(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.muted_foreground)
                        .child("No TMA values for this function"),
                )
            })
            .children(rows)
            .into_any_element(),
    )
}

fn hierarchy_rows(
    tma: &TmaData,
    values: impl Fn(usize) -> Option<f64>,
    theme: &Theme,
    cx: &mut Context<ShellView>,
) -> Vec<gpui::AnyElement> {
    let dominant = tma.dominant_path(&values);
    tma.rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let value = values(index)?;
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
                            .max_w(gpui::relative(0.66))
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
