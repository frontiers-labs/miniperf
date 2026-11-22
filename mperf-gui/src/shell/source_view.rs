use std::{collections::BTreeMap, ops::Range};

use gpui::{HighlightStyle, Hsla, StyledText, rgb};

use super::*;

#[derive(Clone, Copy)]
enum AssemblyRow {
    Mapped(usize),
    Unmapped(usize),
    Gap(usize),
}

#[derive(Default)]
struct CodeRow {
    source_line: Option<usize>,
    linked_line: Option<usize>,
    assembly: Option<AssemblyRow>,
}

fn correlated_rows(tab: &SourceTab) -> Vec<CodeRow> {
    let instructions = tab
        .asm
        .as_ref()
        .map_or(&[][..], |listing| listing.instructions.as_slice());
    let Some(document) = tab
        .document
        .as_ref()
        .filter(|document| !document.lines.is_empty())
    else {
        return (0..instructions.len().max(1))
            .map(|index| CodeRow {
                assembly: (index < instructions.len()).then_some(AssemblyRow::Unmapped(index)),
                ..CodeRow::default()
            })
            .collect();
    };

    let line_count = document.lines.len();
    let mut grouped = BTreeMap::<usize, Vec<AssemblyRow>>::new();
    let mut last_line = None;
    let mut gap_start = 0;
    let append_gap = |group: &mut Vec<AssemblyRow>, range: Range<usize>| {
        if !range.is_empty() {
            group.push(AssemblyRow::Gap(range.len()));
            group.extend(range.map(AssemblyRow::Unmapped));
        }
    };
    for (index, instruction) in instructions.iter().enumerate() {
        let Some(line) = instruction
            .line_in(&document.path)
            .filter(|line| *line <= line_count)
        else {
            continue;
        };
        append_gap(
            grouped.entry(last_line.unwrap_or(line)).or_default(),
            gap_start..index,
        );
        grouped
            .entry(line)
            .or_default()
            .push(AssemblyRow::Mapped(index));
        last_line = Some(line);
        gap_start = index + 1;
    }
    append_gap(
        grouped
            .entry(last_line.unwrap_or_else(|| document.focus_line.clamp(1, line_count)))
            .or_default(),
        gap_start..instructions.len(),
    );

    let mut rows = Vec::new();
    for line in 1..=line_count {
        let assembly = grouped.get(&line).map(Vec::as_slice).unwrap_or_default();
        let first_mapped = assembly
            .iter()
            .position(|row| matches!(row, AssemblyRow::Mapped(_)));
        if first_mapped.is_none() {
            rows.push(CodeRow {
                source_line: Some(line),
                linked_line: Some(line),
                assembly: None,
            });
        }
        rows.extend(assembly.iter().enumerate().map(|(index, &row)| CodeRow {
            source_line: (first_mapped == Some(index)).then_some(line),
            linked_line: matches!(row, AssemblyRow::Mapped(_)).then_some(line),
            assembly: Some(row),
        }));
    }
    rows
}

pub(super) fn scroll_to_function(tab: &SourceTab) {
    let rows = correlated_rows(tab);
    let hottest = tab.asm.as_ref().and_then(|listing| {
        listing
            .line_samples
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(line, _)| *line)
    });
    let target = hottest.or_else(|| tab.document.as_ref().map(|document| document.focus_line));
    let index = rows
        .iter()
        .position(|row| row.source_line == target)
        .unwrap_or(0);
    tab.scroll
        .scroll_to_item(index.saturating_sub(8), ScrollStrategy::Top);
}

fn count_cell(samples: u64, maximum: u64, theme: &Theme) -> gpui::Div {
    div().w(px(48.0)).h(px(16.0)).flex_none().rounded(px(2.0)).px(px(4.0))
        .text_size(px(10.0)).text_right().text_color(theme.muted_foreground)
        .when(samples > 0, |el| el.bg(theme.chart_series[0].opacity(0.08 + samples as f32 / maximum.max(1) as f32 * 0.2))
            .child(samples.to_string()))
}

fn syntax_spans(
    text: &str,
    assembly: bool,
    dark: bool,
    block_comment: &mut bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let palette: [Hsla; 7] = if dark {
        [
            0x9ba9b7, 0xe49ed1, 0xa0d2a1, 0x8fc7ff, 0xe7bc8e, 0xc0cad4, 0xc2b0f0,
        ]
    } else {
        [
            0x68717d, 0x9c257d, 0x247248, 0x165da9, 0x925019, 0x52616b, 0x6d48a8,
        ]
    }
    .map(|color| rgb(color).into());
    let mut spans: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut offset = 0;
    let mut first_word = true;
    while offset < text.len() {
        let tail = &text[offset..];
        let start = offset;
        let character = tail.chars().next().unwrap_or_default();
        let category;
        if *block_comment {
            let length = tail.find("*/").map_or(tail.len(), |end| end + 2);
            *block_comment = !tail.contains("*/");
            offset += length;
            category = Some(0);
        } else if tail.starts_with("//") || (assembly && character == '#') {
            offset = text.len();
            category = Some(0);
        } else if let Some(comment) = tail.strip_prefix("/*") {
            let end = comment.find("*/").map(|end| end + 4);
            *block_comment = end.is_none();
            offset += end.unwrap_or(tail.len());
            category = Some(0);
        } else if character == '"' || (character == '\'' && tail[1..].contains('\'')) {
            offset += character.len_utf8();
            let mut escaped = false;
            for next in text[offset..].chars() {
                offset += next.len_utf8();
                if next == character && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
            }
            category = Some(2);
        } else if character.is_ascii_digit() || (assembly && matches!(character, '%' | '$')) {
            offset += character.len_utf8();
            for next in text[offset..].chars() {
                if !next.is_ascii_alphanumeric() && !matches!(next, '_' | '.') {
                    break;
                }
                offset += next.len_utf8();
            }
            category = Some(4);
        } else if character.is_alphabetic() || character == '_' {
            offset += character.len_utf8();
            for next in text[offset..].chars() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                offset += next.len_utf8();
            }
            let word = &text[start..offset];
            category = if (assembly && first_word)
                || matches!(
                    word,
                    "fn" | "let"
                        | "mut"
                        | "pub"
                        | "use"
                        | "mod"
                        | "impl"
                        | "struct"
                        | "enum"
                        | "trait"
                        | "match"
                        | "if"
                        | "else"
                        | "for"
                        | "while"
                        | "loop"
                        | "return"
                        | "const"
                        | "static"
                        | "unsafe"
                        | "extern"
                        | "as"
                        | "in"
                        | "where"
                        | "move"
                        | "async"
                        | "await"
                        | "int"
                        | "void"
                        | "char"
                        | "float"
                        | "double"
                        | "long"
                        | "short"
                        | "unsigned"
                        | "signed"
                        | "class"
                        | "namespace"
                        | "template"
                        | "typename"
                        | "auto"
                        | "bool"
                        | "switch"
                        | "case"
                        | "break"
                        | "continue"
                        | "include"
                        | "define"
                        | "import"
                        | "export"
                        | "function"
                        | "var"
                        | "new"
                        | "type"
                        | "interface"
                        | "default"
                        | "this"
                ) {
                Some(1)
            } else if matches!(word, "true" | "false" | "NULL" | "nullptr" | "None") {
                Some(4)
            } else if text[offset..].trim_start().starts_with('(')
                || word.chars().next().is_some_and(char::is_uppercase)
            {
                Some(3)
            } else {
                None
            };
            first_word = false;
        } else {
            offset += character.len_utf8();
            category = if character.is_ascii_punctuation() {
                Some(5)
            } else {
                None
            };
        }
        if let Some(category) = category {
            spans.push((start..offset, palette[category].into()));
        }
    }
    spans
}

fn syntax(text: String, assembly: bool, dark: bool, block_comment: &mut bool) -> StyledText {
    let spans = syntax_spans(&text, assembly, dark, block_comment);
    StyledText::new(text).with_highlights(spans)
}

impl ShellView {
    pub(super) fn render_source_tab(
        &self,
        source_ix: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(tab) = self.source_tabs.get(source_ix) else {
            return div().into_any_element();
        };
        let Some(session) = self.session.clone() else {
            return div().into_any_element();
        };
        let frame = &session.profile.frames[tab.frame_id];
        let functions: Vec<_> = session
            .profile
            .frames
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                if let Some(file) = &frame.file {
                    candidate.file.as_ref() == Some(file)
                } else {
                    candidate.name == frame.name && candidate.module == frame.module
                }
            })
            .map(|(id, frame)| (id, frame.friendly_name.clone()))
            .collect();
        let function_ids: Vec<_> = functions.iter().map(|(id, _)| *id).collect();
        let line_count = tab
            .document
            .as_ref()
            .map_or(0, |document| document.lines.len());
        let inspector_visible = if self.wide_inspector {
            self.inspector_open
        } else {
            self.narrow_inspector_open
        };
        let pane_header = |title: &'static str| {
            div()
                .h(px(36.0))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .text_size(px(12.0))
                .border_b_1()
                .border_color(theme.border)
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
        };
        let assembly_header = pane_header("Assembly")
            .when(functions.len() > 1, |el| {
                el.child(
                    div().flex_1().min_w(px(0.0)).child(
                        dropdown_menu(
                            ("source-function", source_ix),
                            div()
                                .flex()
                                .items_center()
                                .flex_1()
                                .min_w(px(0.0))
                                .h(px(24.0))
                                .px(px(4.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(theme.input)
                                .cursor_pointer()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .child(frame.friendly_name.clone()),
                                )
                                .child(icon(Icon::ChevronDown).size(px(12.0))),
                            tab.function_menu_open,
                        )
                        .items(
                            functions
                                .into_iter()
                                .map(|(id, name)| {
                                    DropdownItem::new(name).checked(id == tab.frame_id)
                                })
                                .collect(),
                        )
                        .on_toggle(cx.processor(move |this, open: bool, _, cx| {
                            if let Some(tab) = this.source_tabs.get_mut(source_ix) {
                                tab.function_menu_open = open;
                            }
                            cx.notify();
                        }))
                        .on_select(cx.processor(
                            move |this, index: usize, _, cx| {
                                if let Some(frame_id) = function_ids.get(index) {
                                    this.open_source_tab(*frame_id, cx);
                                }
                            },
                        )),
                    ),
                )
            })
            .when(tab.asm.is_some(), |el| {
                el.child(
                    div()
                        .ml_auto()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{} samples",
                            tab.asm.as_ref().map_or(0, |listing| listing.total_samples)
                        )),
                )
            })
            .child(
                button(("source-inspector", source_ix))
                    .icon(if inspector_visible {
                        Icon::PanelRightClose
                    } else {
                        Icon::PanelRightOpen
                    })
                    .radius(4.0)
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::IconXs)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.wide_inspector {
                            this.inspector_open = !this.inspector_open;
                        } else {
                            this.narrow_inspector_open = !this.narrow_inspector_open;
                        }
                        cx.notify();
                    })),
            );
        let headers = div()
            .flex()
            .flex_none()
            .child(
                pane_header("Source")
                    .flex_1()
                    .min_w(px(0.0))
                    .border_r_1()
                    .border_color(theme.border)
                    .when(line_count > 0, |el| {
                        el.child(
                            div()
                                .ml_auto()
                                .text_color(theme.muted_foreground)
                                .child(format!("{line_count} lines")),
                        )
                    }),
            )
            .child(assembly_header.flex_1().min_w(px(0.0)));
        let rows = Arc::new(correlated_rows(tab));
        let lines = Arc::new(
            tab.document
                .as_ref()
                .map(|document| document.lines.clone())
                .unwrap_or_default(),
        );
        let mut block_comment = false;
        let source_spans: Arc<Vec<_>> = Arc::new(
            lines
                .iter()
                .map(|line| syntax_spans(line, false, theme.dark, &mut block_comment))
                .collect(),
        );
        let source_error = tab
            .document
            .as_ref()
            .and_then(|document| document.error.clone())
            .unwrap_or_else(|| format!("Source unavailable for {}.", frame.friendly_name));
        let asm_error = tab.asm_error.clone().unwrap_or_else(|| {
            if tab.asm.is_some() {
                format!("No native assembly recorded for {}.", frame.friendly_name)
            } else {
                "Loading disassembly…".to_owned()
            }
        });
        let listing = tab.asm.clone();
        let active_line = tab.active_line();
        let selected = tab.pinned_line;
        let entity = cx.entity();
        let scroll = tab.scroll.clone();
        let source_horizontal = tab.source_horizontal.clone();
        let assembly_horizontal = tab.assembly_horizontal.clone();
        let list = uniform_list(
            ("source-assembly", source_ix),
            rows.len(),
            move |range, window, cx| {
                let theme = cx.theme().clone();
                let measure = |text: &str| {
                    let mut run = window.text_style().to_run(text.len());
                    run.font.family = theme.font_mono.clone();
                    run.font.weight = FontWeight::NORMAL;
                    window
                        .text_system()
                        .shape_line(text.to_owned().into(), px(11.0), &[run], None)
                        .width
                };
                let source_width = lines
                    .iter()
                    .map(|line| measure(line))
                    .max()
                    .unwrap_or(px(0.0))
                    + px(112.0);
                let assembly_width = listing
                    .as_ref()
                    .map(|listing| {
                        listing
                            .instructions
                            .iter()
                            .map(|instruction| measure(&instruction.text))
                            .max()
                            .unwrap_or(px(0.0))
                    })
                    .unwrap_or(px(0.0))
                    + px(160.0);
                range
                    .map(|index| {
                        let row = &rows[index];
                        let line = row.linked_line;
                        let linked = line.is_some() && line == active_line;
                        let instruction = row.assembly.and_then(|assembly| match assembly {
                            AssemblyRow::Mapped(index) | AssemblyRow::Unmapped(index) => listing
                                .as_ref()
                                .and_then(|listing| listing.instructions.get(index)),
                            AssemblyRow::Gap(_) => None,
                        });
                        let hover = entity.clone();
                        let click = entity.clone();
                        let samples = row
                            .source_line
                            .and_then(|line| {
                                listing
                                    .as_ref()
                                    .and_then(|listing| listing.line_samples.get(&line).copied())
                            })
                            .unwrap_or(0);
                        let mut source = div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .border_r_1()
                            .border_color(theme.border)
                            .overflow_hidden();
                        if let Some(line) = row.source_line {
                            source = source
                                .child(
                                    div()
                                        .w(px(36.0))
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_right()
                                        .text_color(theme.muted_foreground)
                                        .child(line.to_string()),
                                )
                                .child(count_cell(
                                    samples,
                                    listing
                                        .as_ref()
                                        .map_or(0, |listing| listing.max_line_samples),
                                    &theme,
                                ))
                                .child(
                                    div().whitespace_nowrap().child(
                                        StyledText::new(lines[line - 1].clone())
                                            .with_highlights(source_spans[line - 1].clone()),
                                    ),
                                );
                        } else if index == 0 && lines.is_empty() {
                            source = source.child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(source_error.clone()),
                            );
                        }
                        let mut assembly = div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .overflow_hidden();
                        if let Some(instruction) = instruction {
                            assembly = assembly
                                .child(
                                    div()
                                        .w(px(80.0))
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_right()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{:x}", instruction.address)),
                                )
                                .child(count_cell(
                                    instruction.samples,
                                    listing
                                        .as_ref()
                                        .map_or(0, |listing| listing.max_instruction_samples),
                                    &theme,
                                ))
                                .child(div().whitespace_nowrap().child(syntax(
                                    instruction.text.clone(),
                                    true,
                                    theme.dark,
                                    &mut false,
                                )));
                        } else if let Some(AssemblyRow::Gap(count)) = row.assembly {
                            assembly = assembly
                                .bg(theme.muted.opacity(0.35))
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child(format!("{count} without a source line in this file"));
                        } else if index == 0
                            && listing
                                .as_ref()
                                .is_none_or(|listing| listing.instructions.is_empty())
                        {
                            assembly = assembly
                                .text_color(theme.muted_foreground)
                                .child(asm_error.clone());
                        }
                        div()
                            .id(("code-row", index))
                            .flex()
                            .w_full()
                            .h(px(18.0))
                            .text_size(px(11.0))
                            .font_family(theme.font_mono.clone())
                            .when_some(instruction, |el, instruction| {
                                let total = listing
                                    .as_ref()
                                    .map_or(0, |listing| listing.total_llc_misses);
                                let mut details = format!(
                                    "{} samples, {} LLC misses, {:.1}% of function LLC misses",
                                    instruction.samples,
                                    instruction.llc_misses,
                                    instruction.llc_misses as f64 / total.max(1) as f64 * 100.0
                                );
                                if let Some(source) = instruction.source().filter(|source| {
                                    source.inline_callsite
                                        || matches!(row.assembly, Some(AssemblyRow::Unmapped(_)))
                                }) {
                                    let label = if source.inline_callsite {
                                        "Inlined at"
                                    } else {
                                        "Source at"
                                    };
                                    details.push_str(&format!(
                                        "\n{label} {}:{}",
                                        source.location.path.display(),
                                        source.location.line
                                    ));
                                }
                                el.tooltip(ui::tooltip::text_tooltip(details))
                            })
                            .when(linked, |el| el.bg(theme.accent))
                            .when(line.is_some(), |el| {
                                el.cursor_pointer()
                                    .hover(|el| el.bg(theme.muted.opacity(0.5)))
                            })
                            .on_hover(move |hovered, _, cx| {
                                hover.update(cx, |this, cx| {
                                    if let Some(tab) = this.source_tabs.get_mut(source_ix) {
                                        let next = if *hovered { line } else { None };
                                        if tab.hovered_line != next {
                                            tab.hovered_line = next;
                                            cx.notify();
                                        }
                                    }
                                })
                            })
                            .when_some(line, |el, line| {
                                el.on_click(move |_, _, cx| {
                                    click.update(cx, |this, cx| {
                                        if let Some(tab) = this.source_tabs.get_mut(source_ix) {
                                            tab.pinned_line = if selected == Some(line) {
                                                None
                                            } else {
                                                Some(line)
                                            };
                                            cx.notify();
                                        }
                                    })
                                })
                            })
                            .child(
                                div()
                                    .id(("source-horizontal", index))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .h_full()
                                    .overflow_x_scroll()
                                    .track_scroll(&source_horizontal)
                                    .map(|mut pane| {
                                        pane.style().restrict_scroll_to_axis = Some(true);
                                        pane
                                    })
                                    .child(
                                        source
                                            .flex_none()
                                            .w(source_width)
                                            .min_w(gpui::relative(1.0)),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("assembly-horizontal", index))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .h_full()
                                    .overflow_x_scroll()
                                    .track_scroll(&assembly_horizontal)
                                    .map(|mut pane| {
                                        pane.style().restrict_scroll_to_axis = Some(true);
                                        pane
                                    })
                                    .child(
                                        assembly
                                            .flex_none()
                                            .w(assembly_width)
                                            .min_w(gpui::relative(1.0)),
                                    ),
                            )
                    })
                    .collect()
            },
        )
        .track_scroll(scroll)
        .flex_1()
        .min_h(px(0.0));
        div()
            .relative()
            .size_full()
            .flex()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child(headers)
                    .child(list),
            )
            .when(inspector_visible, |el| {
                let inspector = self.render_function_inspector(tab.frame_id, &session, cx);
                if self.wide_inspector {
                    el.child(inspector)
                } else {
                    el.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .bg(theme.background)
                            .child(inspector),
                    )
                }
            })
            .into_any_element()
    }

    pub(super) fn render_function_inspector(
        &self,
        frame_id: usize,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let hotspots = self.active_view() == Some(ViewId::Hotspots);
        let frame = &session.profile.frames[frame_id];
        let details = self
            .analysis
            .latest()
            .and_then(|analysis| analysis.details(frame_id));
        let panel = div()
            .id("function-inspector")
            .when(hotspots, |el| {
                el.w(gpui::relative(0.36)).min_w(px(260.0)).max_w(px(420.0))
            })
            .when(!hotspots, |el| el.w(px(272.0)))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("inspector-function-name")
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(frame.friendly_name.clone())
                                    .tooltip(ui::tooltip::text_tooltip(frame.name.clone())),
                            )
                            .when(hotspots, |el| {
                                el.child(
                                    button("close-function-inspector")
                                        .icon(Icon::X)
                                        .size(ButtonSize::IconXs)
                                        .variant(ButtonVariant::Ghost)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.select_frame(None, cx)
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(theme.muted_foreground)
                            .child(frame.module.clone().unwrap_or_default()),
                    ),
            );
        let panel = if hotspots {
            panel.children(topdown::hierarchy(session, Some(frame), cx))
        } else {
            panel
        };
        let Some(details) = details else {
            return panel.into_any_element();
        };
        let function = details.function;
        let stats = [
            ("Self samples", function.self_samples.to_string()),
            ("Inclusive samples", function.inclusive_samples.to_string()),
            ("Self %", format!("{:.1}%", function.self_fraction * 100.0)),
            (
                "Total %",
                format!("{:.1}%", function.inclusive_fraction * 100.0),
            ),
            (
                "Cycles",
                function
                    .metrics
                    .cycles
                    .map(format_count)
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            (
                "Instructions",
                function
                    .metrics
                    .instructions
                    .map(format_count)
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            (
                "IPC",
                function
                    .metrics
                    .ipc
                    .map(|ipc| format!("{ipc:.2}"))
                    .unwrap_or_else(|| "-".to_owned()),
            ),
        ];
        panel
            .when(!hotspots, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .px(px(12.0))
                        .py(px(12.0))
                        .gap_y(px(8.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .children(stats.into_iter().map(|(label, value)| {
                            div()
                                .w(gpui::relative(0.5))
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.muted_foreground)
                                        .child(label),
                                )
                                .child(div().mt(px(2.0)).text_size(px(12.0)).child(value))
                        })),
                )
            })
            .children(
                [("Callers", details.callers), ("Callees", details.callees)]
                    .into_iter()
                    .map(|(title, relations)| {
                        div()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .px(px(12.0))
                                    .bg(theme.muted.opacity(0.35))
                                    .text_size(px(12.0))
                                    .child(title)
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(theme.muted_foreground)
                                            .child("Samples"),
                                    ),
                            )
                            .when(relations.is_empty(), |el| {
                                el.child(
                                    div()
                                        .px(px(12.0))
                                        .py(px(12.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.muted_foreground)
                                        .child(format!("No sampled {}.", title.to_lowercase())),
                                )
                            })
                            .children(relations.into_iter().map(|relation| {
                                let frame_id = relation.frame_id;
                                div()
                                    .id((title, frame_id))
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .px(px(12.0))
                                    .border_t_1()
                                    .border_color(theme.border.opacity(0.6))
                                    .cursor_pointer()
                                    .tooltip(ui::tooltip::text_tooltip(
                                        session.profile.frames[frame_id].name.clone(),
                                    ))
                                    .hover(|el| el.bg(theme.muted.opacity(0.4)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .font_family(theme.font_mono.clone())
                                            .text_size(px(11.0))
                                            .child(
                                                session.profile.frames[frame_id]
                                                    .friendly_name
                                                    .clone(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.0))
                                            .child(relation.samples.to_string()),
                                    )
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, _, cx| {
                                            if hotspots && event.click_count() == 1 {
                                                this.select_frame(Some(frame_id), cx);
                                            } else {
                                                this.open_source_tab(frame_id, cx);
                                            }
                                        },
                                    ))
                            }))
                    }),
            )
            .into_any_element()
    }
}
