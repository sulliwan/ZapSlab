//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;

impl Scratchpad {
    /// Palette entries matching the query, best match first. Recently used
    /// transforms lead when the query is empty.
    pub(crate) fn palette_matches(&self) -> Vec<PaletteEntry> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.as_str())
            .unwrap_or("");

        // Empty query: commands lead, then the transform order with recents
        // promoted (matching `transforms::search`).
        if query.trim().is_empty() {
            let mut ordered: Vec<PaletteEntry> = (0..commands::COMMANDS.len())
                .map(PaletteEntry::Command)
                .collect();
            ordered.extend(
                transforms::search("", &self.recent_transforms)
                    .into_iter()
                    .map(PaletteEntry::Transform),
            );
            return ordered;
        }

        let query = query.trim().to_lowercase();
        // Enumerate transforms then commands so equal-score ties keep the
        // palette's transform-first order. `rank_matches` owns the shared
        // scoring and recency rules.
        let mut entries: Vec<PaletteEntry> = (0..transforms::TRANSFORMS.len())
            .map(PaletteEntry::Transform)
            .collect();
        entries.extend((0..commands::COMMANDS.len()).map(PaletteEntry::Command));

        let ranked =
            transforms::rank_matches(&query, &self.recent_transforms, &entries, |_, entry| {
                match *entry {
                    PaletteEntry::Transform(index) => {
                        let transform = &transforms::TRANSFORMS[index];
                        (transform.name, transform.tags, transform.description, index)
                    }
                    PaletteEntry::Command(index) => {
                        let command = &commands::COMMANDS[index];
                        (command.name, command.tags, command.description, index)
                    }
                }
            });
        ranked.into_iter().map(|index| entries[index]).collect()
    }

    pub(crate) fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let (sel_start, sel_end) = self.document.selection_range();
        let has_selection = sel_start != sel_end;
        let len = matches.len() as isize;
        let selected = {
            let Some(palette) = &mut self.palette else {
                return;
            };
            let mut current = palette.selected.min(matches.len() - 1) as isize;
            // Skip entries that cannot run (selection-only with no selection).
            // Bounded by `len` so an all-unavailable list never spins.
            for _ in 0..len {
                current = (current + delta).clamp(0, len - 1);
                if palette_entry_available(matches[current as usize], has_selection) {
                    break;
                }
            }
            palette.selected = current as usize;
            palette.selected
        };
        // Keep the highlighted entry visible while arrowing through the list.
        self.palette_scroll.scroll_to_item(selected);
        cx.notify();
    }

    pub(crate) fn apply_selected_palette_entry(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let selected = self.palette.as_ref().map_or(0, |palette| palette.selected);
        let entry = matches[selected.min(matches.len() - 1)];

        // Commands open UI; they close the palette and never touch the document.
        if let PaletteEntry::Command(index) = entry {
            let command = commands::COMMANDS[index];
            self.palette = None;
            window.notify_text_input_changed(&self.focus_handle);
            match command.id {
                commands::CommandId::OpenFind => self.open_find(window, cx),
                commands::CommandId::OpenFindReplace => self.open_find_replace(window, cx),
            }
            return;
        }

        let PaletteEntry::Transform(index) = entry else {
            return;
        };
        let transform = transforms::TRANSFORMS[index];

        let (sel_start, sel_end) = self.document.selection_range();
        let has_selection = sel_start != sel_end;
        if !transforms::is_available(&transform, has_selection) {
            self.show_hint("Select text to transform", cx);
            return;
        }

        self.record_recent_transform(transform.name);

        // Resolve the read/write range from the transform's scope. Boop's
        // `state.text` is the selection when one exists, else the whole buffer.
        let (start, end) = match transform.scope {
            transforms::Scope::Selection => (sel_start, sel_end),
            transforms::Scope::FullText => (0, self.document.len()),
            transforms::Scope::SelectionOrFullText => {
                if has_selection {
                    (sel_start, sel_end)
                } else {
                    (0, self.document.len())
                }
            }
        };

        // A heavy transform over a big range can block the UI for a while;
        // confirm first instead of appearing to hang.
        if transforms::is_expensive(&transform)
            && end.saturating_sub(start) > TRANSFORM_CONFIRM_BYTES
        {
            self.palette = None;
            self.pending_transform = Some(PendingTransform {
                index,
                start,
                end,
                keep_selection: has_selection,
            });
            cx.notify();
            return;
        }

        self.run_transform(index, start, end, has_selection, window, cx);
    }

    /// Runs a transform over `start..end` (already resolved from its scope).
    pub(crate) fn run_transform(
        &mut self,
        index: usize,
        start: usize,
        end: usize,
        keep_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let transform = transforms::TRANSFORMS[index];
        // Byte-interpreting transforms decode under the hex pane's encoding so
        // arbitrary binary can round-trip through Latin-1.
        let encoding = self.hex_encoding;
        let result = {
            let input = &self.document.text()[start..end];
            match transforms::apply_with_encoding(&transform, input, encoding) {
                Some(result) => result,
                None => (transform.apply)(input),
            }
        };
        self.palette = None;
        match result {
            transforms::TransformResult::Replace(text) => {
                self.replace_range_keeping_selection(start, end, &text, keep_selection, window, cx);
            }
            transforms::TransformResult::ReplaceAndReport { text, message } => {
                self.replace_range_keeping_selection(start, end, &text, keep_selection, window, cx);
                self.show_hint(message, cx);
            }
            transforms::TransformResult::Report(message) => {
                self.show_hint(message, cx);
            }
            transforms::TransformResult::Error(message) => {
                self.show_hint(message, cx);
            }
        }
        cx.notify();
    }

    /// Confirms the pending expensive transform (Enter or the Run button).
    pub(crate) fn confirm_pending_transform(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.pending_transform.take() {
            self.run_transform(
                pending.index,
                pending.start,
                pending.end,
                pending.keep_selection,
                window,
                cx,
            );
        }
    }

    /// Dismisses the pending transform without running it.
    pub(crate) fn cancel_pending_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_transform = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Promotes `name` to the front of the recently-used list and persists it.
    /// Persistence is a convenience; a failed write is intentionally ignored so
    /// it cannot disrupt the edit.
    pub(crate) fn record_recent_transform(&mut self, name: &'static str) {
        self.recent_transforms.retain(|recent| recent != name);
        self.recent_transforms.insert(0, name.to_string());
        self.recent_transforms.truncate(RECENT_TRANSFORMS_MAX);
        let _ = storage::save_recent(&self.recent_transforms);
    }

    #[allow(dead_code)] // used by the native key path
    pub(crate) fn push_palette_query(&mut self, text: &str) {
        if let Some(palette) = &mut self.palette {
            palette.query.push_str(text);
            palette.selected = 0;
        }
        self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
    }
}

impl Scratchpad {
    pub(crate) fn palette_overlay(
        &self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let palette_overlay = self.palette.as_ref().map(|palette| {
            let matches = self.palette_matches();
            let (sel_start, sel_end) = self.document.selection_range();
            let has_selection = sel_start != sel_end;
            // Leave room for the 80px top offset, the query line, and padding
            // so the list never extends past the window.
            let list_max_height = (f32::from(viewport.height) - 80.0 - 56.0).max(120.0);
            let mut items: Vec<AnyElement> = Vec::new();
            let selected_index = palette.selected.min(matches.len().saturating_sub(1));
            if matches.is_empty() {
                items.push(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_color(rgb(0x6c7086))
                        .child("No matches")
                        .into_any_element(),
                );
            }
            for (position, entry) in matches.iter().enumerate() {
                let (name, description, available) = match *entry {
                    PaletteEntry::Transform(index) => {
                        let transform = &transforms::TRANSFORMS[index];
                        (
                            transform.name,
                            transform.description,
                            transforms::is_available(transform, has_selection),
                        )
                    }
                    PaletteEntry::Command(index) => {
                        let command = &commands::COMMANDS[index];
                        (command.name, command.description, true)
                    }
                };
                let selected = position == selected_index;
                let name_color = if !available {
                    rgb(0x585b70)
                } else if selected {
                    rgb(0xcdd6f4)
                } else {
                    rgb(0xa6adc8)
                };
                let info_icon = div()
                    .child("info")
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .px(px(6.0))
                    .py(px(2.0))
                    .id(("palette-info", position))
                    // Clicking the label must not also apply the entry.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _event, _window, cx| cx.stop_propagation()),
                    )
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .tooltip(move |_window, cx| {
                        cx.new(|_| PaletteTooltip { text: description }).into()
                    });
                let item = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_color(name_color)
                    .child(name)
                    .child(info_icon);
                let item = if selected {
                    item.bg(rgb(0x313244))
                } else {
                    item
                };
                let item = item
                    .id(("palette-row", position))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        if let Some(palette) = &mut this.palette {
                            palette.selected = position;
                        }
                        this.apply_selected_palette_entry(window, cx);
                    }));
                items.push(item.into_any_element());
            }

            // Offered only when the page reported a newer deployed build. The
            // palette stays open behind the confirmation overlay.
            let update_button = self.update_available.is_some().then(|| {
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .text_color(rgb(0x11111b))
                    .bg(rgb(0xf9e2af))
                    .child("New version")
                    .id("zapslab-palette-update")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.pending_reload = true;
                        cx.stop_propagation();
                        cx.notify();
                    }))
            });

            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(rgba(0x00000066))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(80.0))
                // Stop the hit test at the modal so the editor's scroll
                // container behind it no longer receives wheel events (gpui's
                // scroll hit-testing considers every hitbox under the pointer).
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, window, cx| {
                        // Clicking the dimmed backdrop cancels, like Esc.
                        this.palette = None;
                        window.notify_text_input_changed(&this.focus_handle);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(440.0))
                        .p(px(8.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        // Keep clicks on the dialog from bubbling to the
                        // backdrop's cancel handler.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        // App name and version sit above the query, matching the
                        // requested "above the palette menu" placement.
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .pb(px(4.0))
                                .text_size(px(11.0))
                                .text_color(rgb(0x6c7086))
                                .child(format!(
                                    "{} {}",
                                    crate::version::APP_NAME,
                                    crate::version::APP_VERSION
                                ))
                                .children(update_button),
                        )
                        .child(
                            div()
                                .px(px(8.0))
                                .py(px(4.0))
                                .text_color(rgb(0xf5e0dc))
                                .child(format!("> {}\u{258F}", palette.query)),
                        )
                        .child(
                            div()
                                .id("zapslab-palette-scroll")
                                .flex()
                                .flex_col()
                                .max_h(px(list_max_height))
                                .min_h(px(0.0))
                                .overflow_y_scroll()
                                .track_scroll(&self.palette_scroll)
                                .children(items),
                        ),
                )
        });
        palette_overlay.map(|element| element.into_any_element())
    }
}
