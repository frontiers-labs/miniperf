use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DocumentId {
    Profile,
    File(PathBuf),
    Function(usize),
}

pub(super) struct Workspace {
    pub documents: Vec<DocumentId>,
    pub active: Option<DocumentId>,
    pub view: ViewId,
    back: Vec<DocumentId>,
    forward: Vec<DocumentId>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            documents: vec![DocumentId::Profile],
            active: Some(DocumentId::Profile),
            view: ViewId::Resources,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl Workspace {
    pub fn open(&mut self, id: DocumentId) {
        if !self.documents.contains(&id) {
            self.documents.push(id.clone());
        }
        self.activate(id);
    }

    fn activate(&mut self, id: DocumentId) {
        if self.active.as_ref() == Some(&id) {
            return;
        }
        if let Some(previous) = self.active.replace(id) {
            self.back.push(previous);
        }
        self.forward.clear();
    }

    fn navigate(&mut self, forward: bool) {
        let (origin, destination) = if forward {
            (&mut self.forward, &mut self.back)
        } else {
            (&mut self.back, &mut self.forward)
        };
        if let Some(id) = origin.pop()
            && let Some(previous) = self.active.replace(id)
        {
            destination.push(previous);
        }
    }

    fn close(&mut self, id: &DocumentId) {
        let Some(index) = self.documents.iter().position(|entry| entry == id) else {
            return;
        };
        self.documents.remove(index);
        if self.active.as_ref() == Some(id) {
            self.active = self
                .documents
                .get(index.min(self.documents.len().saturating_sub(1)))
                .cloned();
        }
        self.back.retain(|entry| entry != id);
        self.forward.retain(|entry| entry != id);
        self.back.dedup();
        self.forward.dedup();
        while self.back.last() == self.active.as_ref() && !self.back.is_empty() {
            self.back.pop();
        }
        while self.forward.last() == self.active.as_ref() && !self.forward.is_empty() {
            self.forward.pop();
        }
    }
}

impl ViewId {
    const SIDEBAR: [Self; 6] = [
        Self::Resources,
        Self::Hotspots,
        Self::Flame,
        Self::Roofline,
        Self::Allocations,
        Self::Structures,
    ];
}

impl ShellView {
    pub(super) fn toggle_sidebar_action(
        &mut self,
        _: &ToggleSidebar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    pub(super) fn toggle_theme_action(
        &mut self,
        _: &ToggleTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_theme(window, cx);
    }

    pub(super) fn navigate_back_action(
        &mut self,
        _: &NavigateBack,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.navigate(false);
        self.ensure_derived(cx);
        cx.notify();
    }

    pub(super) fn navigate_forward_action(
        &mut self,
        _: &NavigateForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.navigate(true);
        self.ensure_derived(cx);
        cx.notify();
    }

    pub(super) fn close_document_action(
        &mut self,
        _: &CloseDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.workspace.active.clone() {
            self.workspace.close(&id);
            self.source_tabs.retain(|tab| tab.id != id);
            self.ensure_derived(cx);
            cx.notify();
        }
    }

    fn activate_document(&mut self, id: DocumentId, cx: &mut Context<Self>) {
        self.workspace.activate(id.clone());
        if let Some(tab) = self.source_tabs.iter().find(|tab| tab.id == id) {
            self.selected_frame = Some(tab.frame_id);
        }
        self.ensure_derived(cx);
        cx.notify();
    }

    pub(super) fn render_title_bar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        // Hyprland can select server decorations without drawing a title bar.
        let client_controls = cfg!(target_os = "linux")
            && (matches!(
                window.window_decorations(),
                gpui::Decorations::Client { .. }
            ) || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some());
        let window_controls = window.window_controls();
        let title = self.active_document_title().unwrap_or_default();
        let views = self.views();
        let recents_len = self.recent_results.len();
        let view_start = recents_len + 2;
        let mut items: Vec<DropdownItem> = self
            .recent_results
            .iter()
            .map(|path| {
                DropdownItem::new(
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("recording")
                        .to_owned(),
                )
                .checked(
                    self.session
                        .as_ref()
                        .is_some_and(|session| session.result_directory == *path),
                )
            })
            .collect();
        items.push(DropdownItem::new("Open recording…").separator_before());
        items.push(DropdownItem::new("New profile…"));
        items.extend(views.iter().enumerate().map(|(index, view)| {
            let item = DropdownItem::new(view.title()).checked(self.active_view() == Some(*view));
            if index == 0 {
                item.separator_before()
            } else {
                item
            }
        }));
        items.push(
            DropdownItem::new("Recording filters and tracks")
                .checked(self.native_controls_open)
                .separator_before(),
        );
        items.push(DropdownItem::new("Clear recording filters"));
        if self.active_view() != Some(ViewId::Hotspots) {
            items.push(DropdownItem::new("Selection details").checked(self.native_selection_open));
        }
        let controls_index = view_start + views.len();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .h(px(36.0))
            .pl(px(12.0))
            .pr(px(12.0))
            .bg(theme.sidebar)
            .border_b_1()
            .border_color(theme.border)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .when(cfg!(target_os = "macos"), |el| {
                el.child(
                    div().flex().items_center().gap(px(8.0)).children(
                        [0xff5f57, 0xfebc2e, 0x28c840].into_iter().enumerate().map(
                            |(index, color)| {
                                div()
                                    .id(("window-control", index))
                                    .size(px(10.0))
                                    .rounded_full()
                                    .bg(gpui::rgb(color))
                                    .cursor_pointer()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(move |_, window, _| match index {
                                        0 => window.remove_window(),
                                        1 => window.minimize_window(),
                                        _ => window.zoom_window(),
                                    })
                            },
                        ),
                    ),
                )
            })
            .child(
                dropdown_menu(
                    "recording-menu",
                    div()
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child("miniperf"),
                    self.recording_menu_open,
                )
                .min_width(256.0)
                .items(items)
                .on_toggle(cx.processor(|this, open: bool, _, cx| {
                    this.recording_menu_open = open;
                    cx.notify();
                }))
                .on_select(cx.processor(move |this, ix: usize, _, cx| {
                    if ix < recents_len {
                        this.open_recording(this.recent_results[ix].clone(), cx);
                    } else if ix == recents_len {
                        this.pick_recording(cx);
                    } else if ix == recents_len + 1 {
                        this.new_profile_open = true;
                    } else if ix < controls_index {
                        this.select_view(views[ix - view_start], cx);
                    } else if ix == controls_index {
                        this.native_controls_open = !this.native_controls_open;
                    } else if ix == controls_index + 1 {
                        this.clear_filters(cx);
                    } else {
                        this.native_selection_open = !this.native_selection_open;
                    }
                    cx.notify();
                })),
            )
            .when(!title.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.border)
                        .child("/"),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.muted_foreground)
                        .child(title),
                )
            })
            .when(self.loading.is_some(), |el| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.muted_foreground)
                        .child("Loading recording…"),
                )
            })
            .child(div().flex_1())
            .child(
                button("shell-theme-toggle")
                    .icon(if theme.dark { Icon::Sun } else { Icon::Moon })
                    .radius(4.0)
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::IconXs)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
            )
            .when(client_controls, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .when(window_controls.minimize, |el| {
                            el.child(
                                button("window-minimize")
                                    .icon(Icon::Minus)
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::IconXs)
                                    .on_click(|_, window, _| window.minimize_window()),
                            )
                        })
                        .when(window_controls.maximize, |el| {
                            el.child(
                                button("window-maximize")
                                    .icon(Icon::Square)
                                    .variant(ButtonVariant::Ghost)
                                    .size(ButtonSize::IconXs)
                                    .on_click(|_, window, _| window.zoom_window()),
                            )
                        })
                        .child(
                            button("window-close")
                                .icon(Icon::X)
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::IconXs)
                                .on_click(|_, window, _| window.remove_window()),
                        ),
                )
            })
    }

    fn active_document_title(&self) -> Option<String> {
        self.workspace.active.as_ref().and_then(|id| {
            if *id == DocumentId::Profile {
                Some(self.workspace.view.title().to_owned())
            } else {
                self.source_tabs
                    .iter()
                    .find(|tab| &tab.id == id)
                    .map(|tab| tab.title.clone())
            }
        })
    }

    pub(super) fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        div()
            .flex()
            .flex_none()
            .h(px(36.0))
            .min_w(px(0.0))
            .bg(theme.sidebar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(2.0))
                    .px(px(4.0))
                    .border_r_1()
                    .border_color(theme.border)
                    .child(
                        button("sidebar-toggle")
                            .icon(Icon::PanelLeft)
                            .icon_size(16.0)
                            .radius(4.0)
                            .size(ButtonSize::IconXs)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_open = !this.sidebar_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("document-back")
                            .icon(Icon::ArrowLeft)
                            .radius(4.0)
                            .size(ButtonSize::IconXs)
                            .variant(ButtonVariant::Ghost)
                            .disabled(self.workspace.back.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspace.navigate(false);
                                this.ensure_derived(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("document-forward")
                            .icon(Icon::ArrowRight)
                            .radius(4.0)
                            .size(ButtonSize::IconXs)
                            .variant(ButtonVariant::Ghost)
                            .disabled(self.workspace.forward.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspace.navigate(true);
                                this.ensure_derived(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("document-strip")
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_x_scroll()
                    .children(
                        self.workspace
                            .documents
                            .iter()
                            .enumerate()
                            .map(|(index, id)| {
                                let selected = self.workspace.active.as_ref() == Some(id);
                                let (title, glyph) = if *id == DocumentId::Profile {
                                    (
                                        self.workspace.view.title().to_owned(),
                                        self.workspace.view.icon(),
                                    )
                                } else {
                                    (
                                        self.source_tabs
                                            .iter()
                                            .find(|tab| &tab.id == id)
                                            .map(|tab| tab.title.clone())
                                            .unwrap_or_default(),
                                        Icon::FileCode,
                                    )
                                };
                                let target = id.clone();
                                let close = id.clone();
                                div()
                                    .relative()
                                    .id(("document", index))
                                    .focusable()
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, _, cx| {
                                            let count = this.workspace.documents.len();
                                            let current = this
                                                .workspace
                                                .documents
                                                .iter()
                                                .position(|id| {
                                                    Some(id) == this.workspace.active.as_ref()
                                                })
                                                .unwrap_or(0);
                                            let next = match event.keystroke.key.as_str() {
                                                "left" => (current + count - 1) % count,
                                                "right" => (current + 1) % count,
                                                "home" => 0,
                                                "end" => count - 1,
                                                _ => return,
                                            };
                                            this.activate_document(
                                                this.workspace.documents[next].clone(),
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .h_full()
                                    .pr(px(4.0))
                                    .border_r_1()
                                    .border_color(theme.border)
                                    .when(selected, |el| {
                                        el.bg(theme.background).child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .left_0()
                                                .right_0()
                                                .h(px(1.0))
                                                .bg(theme.muted_foreground),
                                        )
                                    })
                                    .child(
                                        div()
                                            .id(("document-select", index))
                                            .flex()
                                            .items_center()
                                            .h_full()
                                            .gap(px(7.0))
                                            .pl(px(12.0))
                                            .pr(px(6.0))
                                            .text_size(px(12.0))
                                            .text_color(if selected {
                                                theme.foreground
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .cursor_pointer()
                                            .child(
                                                icon(glyph)
                                                    .size(px(14.0))
                                                    .color(theme.muted_foreground),
                                            )
                                            .child(div().max_w(px(192.0)).truncate().child(
                                                if matches!(id, DocumentId::File(_)) {
                                                    title
                                                        .rsplit('/')
                                                        .next()
                                                        .unwrap_or(&title)
                                                        .to_owned()
                                                } else {
                                                    title
                                                },
                                            ))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.activate_document(target.clone(), cx)
                                            })),
                                    )
                                    .child(
                                        button(("document-close", index))
                                            .icon(Icon::X)
                                            .radius(4.0)
                                            .size(ButtonSize::IconXxs)
                                            .variant(ButtonVariant::Ghost)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.workspace.close(&close);
                                                this.source_tabs.retain(|tab| tab.id != close);
                                                this.ensure_derived(cx);
                                                cx.notify();
                                            })),
                                    )
                            }),
                    ),
            )
    }

    pub(super) fn render_sidebar(
        &self,
        session: &Arc<ShellSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let available_sidebar_views = ViewId::SIDEBAR
            .into_iter()
            .filter(|view| view.is_available(session));
        let files: BTreeSet<PathBuf> = session
            .profile
            .frames
            .iter()
            .filter_map(|frame| frame.file.as_ref().map(PathBuf::from))
            .collect();
        let common = files
            .iter()
            .next()
            .and_then(|path| path.parent())
            .map(|parent| {
                let mut root = parent.to_path_buf();
                while !files.iter().all(|path| path.starts_with(&root)) {
                    if !root.pop() {
                        break;
                    }
                }
                root
            })
            .unwrap_or_default();
        let mut directories = BTreeSet::new();
        let mut entries = Vec::new();
        for path in files {
            let relative = path.strip_prefix(&common).unwrap_or(&path);
            let components: Vec<_> = relative.components().collect();
            let mut directory = common.clone();
            for (depth, component) in components.iter().enumerate() {
                directory.push(component.as_os_str());
                let is_file = depth + 1 == components.len();
                if !is_file && !directories.insert(directory.clone()) {
                    if !self.expanded_folders.contains(&directory) {
                        break;
                    }
                    continue;
                }
                entries.push((
                    directory.clone(),
                    component.as_os_str().to_string_lossy().into_owned(),
                    depth,
                    is_file,
                ));
                if !is_file && !self.expanded_folders.contains(&directory) {
                    break;
                }
            }
        }
        let active = self.workspace.active.clone();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(256.0))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(px(36.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(
                        icon(Icon::FolderOpen)
                            .size(px(14.0))
                            .color(theme.muted_foreground),
                    )
                    .child("miniperf"),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .children(available_sidebar_views.map(|view| {
                                let selected = self.active_view() == Some(view);
                                div()
                                    .id(view.title())
                                    .flex()
                                    .items_center()
                                    .h(px(32.0))
                                    .gap(px(8.0))
                                    .px(px(8.0))
                                    .rounded(px(4.0))
                                    .text_size(px(14.0))
                                    .line_height(px(20.0))
                                    .cursor_pointer()
                                    .when(selected, |el| {
                                        el.bg(theme.accent)
                                            .text_color(theme.accent_foreground)
                                            .font_weight(FontWeight::MEDIUM)
                                    })
                                    .hover(|el| el.bg(theme.accent))
                                    .child(icon(view.icon()).size(px(16.0)))
                                    .child(view.title())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.select_view(view, cx)
                                        }),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .h(px(48.0))
                            .flex()
                            .items_end()
                            .px(px(8.0))
                            .pb(px(6.0))
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted_foreground)
                            .child("Files"),
                    )
                    .children(entries.into_iter().enumerate().map(
                        |(index, (path, name, depth, is_file))| {
                            let selected = active.as_ref() == Some(&DocumentId::File(path.clone()));
                            let expanded = self.expanded_folders.contains(&path);
                            div()
                                .id(("file", index))
                                .flex()
                                .items_center()
                                .h(px(32.0))
                                .gap(px(8.0))
                                .pl(px(8.0 + depth as f32 * 16.0))
                                .pr(px(8.0))
                                .rounded(px(4.0))
                                .text_size(px(14.0))
                                .cursor_pointer()
                                .when(selected, |el| el.bg(theme.accent))
                                .hover(|el| el.bg(theme.accent))
                                .when(!is_file, |el| {
                                    el.child(
                                        icon(if expanded {
                                            Icon::ChevronDown
                                        } else {
                                            Icon::ChevronRight
                                        })
                                        .size(px(16.0)),
                                    )
                                })
                                .child(
                                    icon(if is_file { Icon::File } else { Icon::Folder })
                                        .size(px(16.0)),
                                )
                                .child(div().min_w(px(0.0)).truncate().child(name))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if is_file {
                                        let id = DocumentId::File(path.clone());
                                        if this.workspace.documents.contains(&id) {
                                            this.activate_document(id, cx);
                                        } else if let Some(frame_id) =
                                            this.session.as_ref().and_then(|session| {
                                                session.profile.frames.iter().position(|frame| {
                                                    frame.file.as_ref().is_some_and(|file| {
                                                        std::path::Path::new(file) == path
                                                    })
                                                })
                                            })
                                        {
                                            this.open_source_tab(frame_id, cx);
                                        }
                                    } else if !this.expanded_folders.remove(&path) {
                                        this.expanded_folders.insert(path.clone());
                                    }
                                    cx.notify();
                                }))
                        },
                    )),
            )
    }
}
