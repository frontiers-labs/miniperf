use std::{cmp::Ordering, path::PathBuf, sync::Arc};

use gpui::{
    Context, Entity, FontWeight, IntoElement, PathPromptOptions, Subscription, div, prelude::*, px,
    relative, uniform_list,
};
use symbolize::type_layout::{ObjectLayouts, TypeLayout};

use super::{ShellView, session::ShellSession};
use crate::ui::{self, ActiveTheme, ButtonSize, ButtonVariant, Icon, button, icon};

#[derive(Default)]
enum Catalog {
    #[default]
    Idle,
    Loading {
        request: Arc<()>,
        path: PathBuf,
    },
    Ready(Arc<ObjectLayouts>),
    Unavailable(String),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SortKey {
    Name,
    #[default]
    Size,
    Alignment,
    Padding,
}

pub(super) struct State {
    catalog: Catalog,
    query: Option<Entity<ui::TextInput>>,
    subscription: Option<Subscription>,
    selected: Option<usize>,
    selected_field: Option<usize>,
    sort: SortKey,
    descending: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            catalog: Catalog::Idle,
            query: None,
            subscription: None,
            selected: None,
            selected_field: None,
            sort: SortKey::Size,
            descending: true,
        }
    }
}

pub(super) fn ensure(shell: &mut ShellView, cx: &mut Context<ShellView>) {
    if shell.structures.query.is_none() {
        let input = cx.new(|cx| {
            ui::TextInput::new("structure-search", "Search type, field or file…", cx)
                .leading_icon(Icon::Search)
                .height(32.0)
                .text_size(12.0)
                .width(256.0)
        });
        shell.structures.subscription =
            Some(cx.subscribe(&input, |_, _, _: &ui::InputEvent, cx| cx.notify()));
        shell.structures.query = Some(input);
    }
    if !matches!(shell.structures.catalog, Catalog::Idle) {
        return;
    }
    let binary = shell.session.as_ref().and_then(|session| {
        session
            .modules
            .iter()
            .filter(|module| module.kind == ui::ModuleKind::User)
            .flat_map(|module| module.frame_ids.iter())
            .filter_map(|id| session.profile.frames.get(*id)?.module.as_ref())
            .map(PathBuf::from)
            .find(|path| path.is_absolute() && path.is_file())
    });
    match binary {
        Some(path) => load(shell, path, cx),
        None => shell.structures.catalog = Catalog::Unavailable("The recorded binaries are not available locally. Open a binary with debug information to inspect its type layouts.".to_owned()),
    }
}

fn load(shell: &mut ShellView, path: PathBuf, cx: &mut Context<ShellView>) {
    let request = Arc::new(());
    shell.structures.catalog = Catalog::Loading {
        request: request.clone(),
        path: path.clone(),
    };
    cx.spawn(async move |this, cx| {
        let loaded = cx.background_executor().spawn(async move { symbolize::type_layout::load(&path) }).await;
        this.update(cx, |this, cx| {
            if !matches!(&this.structures.catalog, Catalog::Loading { request: current, .. } if Arc::ptr_eq(current, &request)) { return; }
            this.structures.selected = loaded.as_ref().ok().and_then(|catalog| (!catalog.types.is_empty()).then_some(0));
            this.structures.selected_field = None;
            this.structures.catalog = match loaded {
                Ok(catalog) => Catalog::Ready(Arc::new(catalog)),
                Err(error) => Catalog::Unavailable(error.to_string()),
            };
            cx.notify();
        }).ok();
    }).detach();
    cx.notify();
}

fn open_binary(cx: &mut Context<ShellView>) {
    let selected = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Open Binary".into()),
    });
    cx.spawn(async move |this, cx| {
        if let Ok(Ok(Some(paths))) = selected.await
            && let Some(path) = paths.into_iter().next()
        {
            this.update(cx, |this, cx| load(this, path, cx)).ok();
        }
    })
    .detach();
}

fn padding(layout: &TypeLayout) -> Option<u64> {
    Some(layout.padding()?.iter().map(|(_, size)| size).sum())
}

fn bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value} B"))
}

fn compare(a: &TypeLayout, b: &TypeLayout, key: SortKey, descending: bool) -> Ordering {
    let ordering = if key == SortKey::Name {
        a.name.cmp(&b.name)
    } else {
        let value = |layout: &TypeLayout| match key {
            SortKey::Size => Some(layout.size),
            SortKey::Alignment => layout.alignment,
            SortKey::Padding => padding(layout),
            SortKey::Name => unreachable!(),
        };
        match (value(a), value(b)) {
            (None, None) => return a.name.cmp(&b.name),
            (None, Some(_)) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Less,
            (Some(left), Some(right)) => left.cmp(&right),
        }
    };
    (if descending {
        ordering.reverse()
    } else {
        ordering
    })
    .then(a.name.cmp(&b.name))
}

pub(super) fn render(
    shell: &ShellView,
    _: &Arc<ShellSession>,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let catalog = match &shell.structures.catalog {
        Catalog::Ready(catalog) => Some(catalog.clone()),
        _ => None,
    };
    let count = catalog.as_ref().map_or(0, |catalog| catalog.types.len());
    let query = shell
        .structures
        .query
        .as_ref()
        .map_or_else(String::new, |input| {
            input.read(cx).text().trim().to_lowercase()
        });
    let mut rows: Vec<usize> = catalog.as_ref().map_or_else(Vec::new, |catalog| {
        catalog
            .types
            .iter()
            .enumerate()
            .filter(|(_, layout)| {
                query.is_empty()
                    || layout.name.to_lowercase().contains(&query)
                    || layout
                        .source
                        .as_ref()
                        .is_some_and(|path| path.to_string_lossy().to_lowercase().contains(&query))
                    || layout.fields.iter().any(|field| {
                        field.name.to_lowercase().contains(&query)
                            || field.type_name.to_lowercase().contains(&query)
                    })
            })
            .map(|(index, _)| index)
            .collect()
    });
    if let Some(catalog) = &catalog {
        rows.sort_by(|a, b| {
            compare(
                &catalog.types[*a],
                &catalog.types[*b],
                shell.structures.sort,
                shell.structures.descending,
            )
        });
    }
    let selected = catalog.as_ref().and_then(|catalog| {
        shell
            .structures
            .selected
            .and_then(|index| catalog.types.get(index))
    });
    let header = [
        (SortKey::Name, "Type"),
        (SortKey::Size, "Size"),
        (SortKey::Alignment, "Alignment"),
        (SortKey::Padding, "Padding"),
    ];
    let mut root = div()
        .id("structures-view")
        .size_full()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .text_size(px(12.0))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(px(45.0))
                .px(px(8.0))
                .gap(px(8.0))
                .border_b_1()
                .border_color(theme.border)
                .when_some(shell.structures.query.clone(), |el, input| el.child(input))
                .child(div().flex_1())
                .when(count == 0, |el| {
                    el.child(
                        button("open-type-binary")
                            .label("Open binary…")
                            .icon(Icon::FolderOpen)
                            .variant(ButtonVariant::Outline)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|_, _, _, cx| open_binary(cx))),
                    )
                })
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.muted_foreground)
                        .child(format!("{count} types · DWARF")),
                ),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .h(px(37.0))
                .bg(theme.muted.opacity(0.35))
                .border_b_1()
                .border_color(theme.border)
                .children(header.into_iter().map(|(key, label)| {
                    let active = shell.structures.sort == key;
                    let sort_icon = if active {
                        if shell.structures.descending {
                            Icon::ArrowDown
                        } else {
                            Icon::ArrowUp
                        }
                    } else {
                        Icon::ArrowUpDown
                    };
                    div()
                        .id(label)
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .h_full()
                        .px(px(12.0))
                        .text_color(if active {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .when(key == SortKey::Name, |el| el.w(relative(0.64)).flex_none())
                        .when(key != SortKey::Name, |el| el.flex_1().justify_end())
                        .child(label)
                        .child(icon(sort_icon).size(px(12.0)).color(theme.muted_foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.structures.descending = if this.structures.sort == key {
                                !this.structures.descending
                            } else {
                                key != SortKey::Name
                            };
                            this.structures.sort = key;
                            cx.notify();
                        }))
                })),
        );
    if let Some(catalog) = catalog.clone() {
        let height = rows.len().min(9) as f32 * 28.0;
        let selected_index = shell.structures.selected;
        root = root.child(
            uniform_list(
                "structure-types",
                rows.len(),
                cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|index| {
                            let id = rows[index];
                            let layout = &catalog.types[id];
                            ui::table_row(selected_index == Some(id), cx)
                                .id(id)
                                .text_size(px(12.0))
                                .child(
                                    div()
                                        .w(relative(0.64))
                                        .flex_none()
                                        .px(px(12.0))
                                        .font_family(cx.theme().font_mono.clone())
                                        .truncate()
                                        .child(layout.name.clone()),
                                )
                                .children(
                                    [
                                        bytes(Some(layout.size)),
                                        bytes(layout.alignment),
                                        bytes(padding(layout)),
                                    ]
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, value)| {
                                        div()
                                            .flex_1()
                                            .flex()
                                            .justify_end()
                                            .px(px(12.0))
                                            .when(index == 1, |el| {
                                                el.text_color(cx.theme().muted_foreground)
                                            })
                                            .child(value)
                                    }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.structures.selected = Some(id);
                                    this.structures.selected_field = None;
                                    cx.notify();
                                }))
                        })
                        .collect()
                }),
            )
            .h(px(height))
            .flex_none(),
        );
    }
    if let Some(layout) = selected {
        let object = catalog.as_ref().expect("selected layout has an object");
        let summary = format!(
            "{} · {} B · {} fields · {} padding",
            layout.language,
            layout.size,
            layout.fields.len(),
            bytes(padding(layout))
        );
        let module = object
            .path
            .file_name()
            .unwrap_or_else(|| object.path.as_os_str())
            .to_string_lossy();
        let build = object
            .build_id
            .as_ref()
            .map(|id| format!(" · build {}", &id[..id.len().min(8)]))
            .unwrap_or_default();
        root = root
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(20.0))
                            .gap_y(px(4.0))
                            .child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(layout.name.clone()),
                            )
                            .child(div().text_color(theme.muted_foreground).child(summary)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(16.0))
                            .mt(px(4.0))
                            .text_size(px(11.0))
                            .text_color(theme.muted_foreground)
                            .when_some(layout.source.as_ref(), |el, path| {
                                el.child(path.display().to_string())
                            })
                            .child(format!("{module} · {}{build}", object.architecture))
                            .when(layout.alignment.is_none(), |el| {
                                el.child("Alignment unavailable in DWARF")
                            }),
                    ),
            )
            .child(render_layout(shell, layout, cx));
    } else {
        let message = match &shell.structures.catalog {
            Catalog::Idle => "Loading type layouts…".to_owned(),
            Catalog::Loading { path, .. } => format!("Reading {}…", path.display()),
            Catalog::Ready(catalog) if catalog.types.is_empty() => {
                format!("No physical type layouts in {}", catalog.path.display())
            }
            Catalog::Ready(_) => "No matching types".to_owned(),
            Catalog::Unavailable(message) => message.clone(),
        };
        root = root.child(
            div()
                .min_h(px(128.0))
                .px(px(24.0))
                .py(px(32.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(message),
        );
    }
    root.into_any_element()
}

fn render_layout(
    shell: &ShellView,
    layout: &TypeLayout,
    cx: &mut Context<ShellView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let selected_field = shell.structures.selected_field;
    let row_count = layout.size.div_ceil(64).min(256);
    let mut bytes_pane = div()
        .w(relative(0.42))
        .min_w(px(280.0))
        .flex_none()
        .border_r_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(12.0))
                .py(px(8.0))
                .font_weight(FontWeight::MEDIUM)
                .child("Byte layout")
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.muted_foreground)
                        .child("Padding"),
                ),
        )
        .child(
            div()
                .px(px(12.0))
                .pb(px(12.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children((0..row_count).map(|row| {
                    let start = row * 64;
                    let end = layout.size.min(start + 64);
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .w(px(48.0))
                                .flex_none()
                                .text_right()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(format!("{start}..{}", end - 1)),
                        )
                        .child(
                            div()
                                .relative()
                                .h(px(32.0))
                                .flex_1()
                                .min_w(px(0.0))
                                .border_1()
                                .border_color(theme.border.opacity(0.6))
                                .bg(theme.muted.opacity(0.2))
                                .children(layout.fields.iter().enumerate().filter_map(
                                    |(index, field)| {
                                        let offset = field.offset?;
                                        let size = field.size?;
                                        let begin = start.max(offset);
                                        let finish = end.min(offset.saturating_add(size));
                                        if finish <= begin {
                                            return None;
                                        }
                                        Some(
                                            div()
                                                .id(("field", index))
                                                .absolute()
                                                .top_0()
                                                .left(relative((begin - start) as f32 / 64.0))
                                                .w(relative((finish - begin) as f32 / 64.0))
                                                .h_full()
                                                .overflow_hidden()
                                                .border_r_1()
                                                .border_color(theme.background.opacity(0.6))
                                                .bg(ui::theme::mix(
                                                    theme.background,
                                                    theme.chart_series[1 + index % 4],
                                                    0.38,
                                                ))
                                                .text_size(px(10.0))
                                                .cursor_pointer()
                                                .when(selected_field == Some(index), |el| {
                                                    el.border_2().border_color(theme.foreground)
                                                })
                                                .when(finish - begin >= 8, |el| {
                                                    el.child(
                                                        div()
                                                            .px(px(4.0))
                                                            .h_full()
                                                            .flex()
                                                            .items_center()
                                                            .truncate()
                                                            .child(field.name.clone()),
                                                    )
                                                })
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.structures.selected_field = Some(index);
                                                    cx.notify();
                                                })),
                                        )
                                    },
                                )),
                        )
                })),
        );
    if layout.size.div_ceil(64) > row_count {
        bytes_pane = bytes_pane.child(
            div()
                .px(px(12.0))
                .pb(px(8.0))
                .text_size(px(11.0))
                .text_color(theme.muted_foreground)
                .child("Byte layout shows the first 16 KiB"),
        );
    }
    let fields_pane = div()
        .flex_1()
        .min_w(px(0.0))
        .child(
            div()
                .px(px(12.0))
                .py(px(8.0))
                .font_weight(FontWeight::MEDIUM)
                .child("Physical fields"),
        )
        .child(
            div()
                .flex()
                .h(px(28.0))
                .px(px(12.0))
                .items_center()
                .border_y_1()
                .border_color(theme.border)
                .bg(theme.muted.opacity(0.3))
                .text_size(px(11.0))
                .text_color(theme.muted_foreground)
                .child(div().flex_1().child("Field / type"))
                .child(div().w(px(65.0)).text_right().child("Offset"))
                .child(div().w(px(65.0)).text_right().child("Size")),
        )
        .children(layout.fields.iter().enumerate().map(|(index, field)| {
            ui::table_row(selected_field == Some(index), cx)
                .id(index)
                .h(px(40.0))
                .px(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .font_family(theme.font_mono.clone())
                        .child(div().truncate().child(field.name.clone()))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(field.type_name.clone()),
                        ),
                )
                .child(
                    div().w(px(65.0)).text_right().child(
                        field
                            .offset
                            .map_or_else(|| "-".to_owned(), |offset| offset.to_string()),
                    ),
                )
                .child(
                    div().w(px(65.0)).text_right().child(
                        field
                            .bit_size
                            .map_or_else(|| bytes(field.size), |bits| format!("{bits} bits")),
                    ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.structures.selected_field = Some(index);
                    cx.notify();
                }))
        }));
    div()
        .flex()
        .min_w(px(0.0))
        .child(bytes_pane)
        .child(fields_pane)
        .into_any_element()
}
