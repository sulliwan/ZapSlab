//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;

impl Scratchpad {
    // ---- Find / replace -------------------------------------------------

    /// Ctrl/Cmd+F toggles the floating find/replace dialog. A current
    /// single-line selection becomes the find query (fully selected) so typing
    /// replaces it; otherwise the last closed dialog state is restored. It jumps
    /// to the first match at or after the selection start.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_some() {
            self.close_find(window, cx);
            return;
        }
        let (sel_start, sel_end) = self.document.selection_range();
        let selection = if sel_start != sel_end {
            Some((sel_start, sel_end))
        } else {
            None
        };

        let seed = match selection {
            Some((start, end)) => {
                let text = &self.document.text()[start..end];
                if !text.contains('\n') && text.len() <= 512 {
                    Some(text.to_string())
                } else {
                    None
                }
            }
            None => None,
        };

        let (find, replace, regex, case_insensitive, field, select_find) =
            match (seed, self.last_find.clone()) {
                // A live selection always becomes the find query, fully selected.
                (Some(find), saved) => {
                    let (replace, regex, case_insensitive) = match saved {
                        Some(saved) => (saved.replace, saved.regex, saved.case_insensitive),
                        None => (String::new(), false, false),
                    };
                    (
                        find,
                        replace,
                        regex,
                        case_insensitive,
                        FindField::Find,
                        true,
                    )
                }
                // No selection: restore the last closed dialog.
                (None, Some(saved)) => (
                    saved.find,
                    saved.replace,
                    saved.regex,
                    saved.case_insensitive,
                    if saved.replace_focused {
                        FindField::Replace
                    } else {
                        FindField::Find
                    },
                    false,
                ),
                (None, None) => (
                    String::new(),
                    String::new(),
                    false,
                    false,
                    FindField::Find,
                    false,
                ),
            };

        let find_len = find.len();
        let replace_len = replace.len();
        self.find = Some(FindState {
            find,
            replace,
            find_caret: find_len,
            replace_caret: replace_len,
            find_scroll: 0.0,
            replace_scroll: 0.0,
            find_anchor: if select_find { 0 } else { find_len },
            replace_anchor: replace_len,
            field,
            regex,
            case_insensitive,
            matcher: None,
            error: None,
            current: None,
            wrapped: false,
            history_index: None,
            history_draft: None,
            field_drag: None,
            history_open: false,
        });
        // Seed the scroll key with the freshly opened state so the first frame
        // shows the start of the query instead of auto-scrolling to the caret.
        self.find_scroll_key = self
            .find
            .as_ref()
            .map(|state| (state.field, state.caret(), state.active().len()));
        self.palette = None;
        self.refresh_find(sel_start);
        window.notify_text_input_changed(&self.focus_handle);
        if let Some(range) = self.find.as_ref().and_then(|state| state.current) {
            self.select_match(range, window, cx);
        }
        cx.notify();
    }

    /// Opened from the palette command: same dialog, replacement field focused.
    pub(crate) fn open_find_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.open_find(window, cx);
        }
        if let Some(state) = &mut self.find {
            state.field = FindField::Replace;
        }
        // Keep the replacement field's view still on the first frame, like open.
        self.find_scroll_key = self
            .find
            .as_ref()
            .map(|state| (state.field, state.caret(), state.active().len()));
        self.palette = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    pub(crate) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Remember the dialog for the next open and surface the query in the
        // recent-finds history. Storage failures are harmless UI-only state; the
        // in-memory `last_find` still restores it for this session.
        let saved = self.find.as_ref().map(|state| storage::SavedFindState {
            find: state.find.clone(),
            replace: state.replace.clone(),
            regex: state.regex,
            case_insensitive: state.case_insensitive,
            replace_focused: state.field == FindField::Replace,
        });
        if let Some(saved) = saved {
            let _ = storage::save_find_state(&saved);
            self.last_find = Some(saved);
        }
        self.record_find();
        self.find = None;
        self.find_scroll_key = None;
        // Keep the recorded field bounds: the dialog is a fixed width, so the
        // next open can draw its edge ellipses on the very first frame instead
        // of waiting for a paint.
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Records the current query in the recent-finds list (newest first,
    /// deduplicated, capped) and persists it. Invalid regexes are not recorded.
    pub(crate) fn record_find(&mut self) {
        let Some((query, invalid)) = self
            .find
            .as_ref()
            .map(|state| (state.find.clone(), state.error.is_some()))
        else {
            return;
        };
        if query.is_empty() || invalid {
            return;
        }
        self.recent_finds.retain(|existing| existing != &query);
        self.recent_finds.insert(0, query);
        self.recent_finds.truncate(MAX_RECENT_FINDS);
        let _ = storage::save_recent_finds(&self.recent_finds);
        if let Some(state) = &mut self.find {
            state.reset_history();
            // Committing a find dismisses the history dropdown.
            state.history_open = false;
        }
    }

    /// Up in the Find field: step to older recent queries.
    pub(crate) fn find_history_prev(&mut self, cx: &mut Context<Self>) {
        let Some((index, draft, query)) = (|| {
            let state = self.find.as_ref()?;
            if state.field != FindField::Find || self.recent_finds.is_empty() {
                return None;
            }
            let index = match state.history_index {
                Some(index) => (index + 1).min(self.recent_finds.len() - 1),
                None => 0,
            };
            let draft = state
                .history_draft
                .clone()
                .unwrap_or_else(|| state.find.clone());
            let query = self.recent_finds[index].clone();
            Some((index, draft, query))
        })() else {
            return;
        };

        if let Some(state) = &mut self.find {
            state.history_index = Some(index);
            state.history_draft = Some(draft);
            state.find = query;
            let len = state.find.len();
            state.find_caret = len;
            state.find_anchor = len;
        }
        self.after_find_query_changed(cx);
    }

    /// Down in the Find field: step toward newer queries, restoring the stashed
    /// draft after the newest entry.
    pub(crate) fn find_history_next(&mut self, cx: &mut Context<Self>) {
        let Some((index, query)) = (|| {
            let state = self.find.as_ref()?;
            if state.field != FindField::Find {
                return None;
            }
            let index = state.history_index?;
            if index == 0 {
                Some((None, state.history_draft.clone().unwrap_or_default()))
            } else {
                let previous = index - 1;
                Some((Some(previous), self.recent_finds.get(previous)?.clone()))
            }
        })() else {
            return;
        };

        if let Some(state) = &mut self.find {
            state.find = query;
            let len = state.find.len();
            state.find_caret = len;
            state.find_anchor = len;
            state.history_index = index;
            if index.is_none() {
                state.history_draft = None;
            }
        }
        self.after_find_query_changed(cx);
    }

    /// Toggles the recent-finds dropdown below the Find field. Opening it also
    /// focuses the Find field, since the dropdown belongs to that field.
    pub(crate) fn toggle_find_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        if let Some(state) = &mut self.find {
            state.field = FindField::Find;
            state.history_open = !state.history_open;
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Chooses a recent query from the dropdown: fills the Find field, closes
    /// the dropdown, and refreshes the live match.
    pub(crate) fn select_find_history(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(query) = self.recent_finds.get(index).cloned() else {
            return;
        };
        if let Some(state) = &mut self.find {
            state.field = FindField::Find;
            state.find = query;
            let len = state.find.len();
            state.find_caret = len;
            state.find_anchor = len;
            state.history_open = false;
            state.reset_history();
        }
        self.after_find_query_changed(cx);
    }

    pub(crate) fn handle_find_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;

        // Ctrl/Cmd+F toggles the dialog closed, mirroring how it opened it.
        if (modifiers.platform || modifiers.control) && keystroke.key == "f" {
            self.close_find(window, cx);
            return true;
        }

        // Ctrl/Cmd+A selects the whole active field, like a real text input.
        if (modifiers.platform || modifiers.control) && keystroke.key == "a" {
            self.select_all_find(cx);
            window.notify_text_input_changed(&self.focus_handle);
            return true;
        }

        match keystroke.key.as_str() {
            "escape" => self.close_find(window, cx),
            "enter" => {
                if modifiers.shift {
                    self.find_prev_match(window, cx);
                } else if self
                    .find
                    .as_ref()
                    .is_some_and(|state| state.field == FindField::Replace)
                {
                    self.replace_current(window, cx);
                } else {
                    self.find_next_match(window, cx);
                }
            }
            "tab" => {
                self.toggle_find_field(cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "backspace" => {
                self.backspace_find(cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "delete" => {
                self.delete_forward_find(cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "left" => {
                self.move_find_caret(false, modifiers.shift, cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "right" => {
                self.move_find_caret(true, modifiers.shift, cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "home" => {
                self.move_find_caret_home(modifiers.shift, cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "end" => {
                self.move_find_caret_end(modifiers.shift, cx);
                window.notify_text_input_changed(&self.focus_handle);
            }
            "up" => {
                if !modifiers.shift {
                    self.find_history_prev(cx);
                    window.notify_text_input_changed(&self.focus_handle);
                }
            }
            "down" => {
                if !modifiers.shift {
                    self.find_history_next(cx);
                    window.notify_text_input_changed(&self.focus_handle);
                }
            }
            _ => {
                // On web, printable keys fall through to the input handler,
                // which appends them to the active field.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(text) = &keystroke.key_char {
                    self.push_find_text(text, cx);
                    return true;
                }
                return false;
            }
        }
        true
    }

    pub(crate) fn toggle_find_field(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            state.field = match state.field {
                FindField::Find => FindField::Replace,
                FindField::Replace => FindField::Find,
            };
            cx.notify();
        }
    }

    pub(crate) fn toggle_find_regex(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            return;
        };
        state.regex = !state.regex;
        self.after_find_query_changed(cx);
    }

    pub(crate) fn toggle_find_case_insensitive(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            return;
        };
        state.case_insensitive = !state.case_insensitive;
        self.after_find_query_changed(cx);
    }

    // Native printable keys are handled here; on web they fall through to the
    // input handler instead, so this is only referenced by the native path.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn push_find_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let editing_find = {
            let Some(state) = &mut self.find else {
                return;
            };
            let insert = field_input(text);
            let (start, end) = state.selection();
            let text_ref = state.active();
            let start = snap_boundary(text_ref, start);
            let end = snap_boundary(text_ref, end).max(start);
            state.active_mut().replace_range(start..end, &insert);
            let caret = start + insert.len();
            state.set_caret(caret);
            state.set_anchor(caret);
            let editing_find = state.field == FindField::Find;
            if editing_find {
                state.reset_history();
            }
            editing_find
        };
        if editing_find {
            self.after_find_query_changed(cx);
        } else {
            cx.notify();
        }
    }

    /// Deletes the selection, or the grapheme before the field's caret.
    pub(crate) fn backspace_find(&mut self, cx: &mut Context<Self>) {
        let (deletion, editing_find) = {
            let Some(state) = &self.find else {
                return;
            };
            let text = state.active();
            let (sel_start, sel_end) = state.selection();
            let (start, end) = if sel_start != sel_end {
                (sel_start, sel_end)
            } else {
                let caret = snap_boundary(text, state.caret());
                if caret == 0 {
                    (0, 0)
                } else {
                    (previous_grapheme_start(text, caret), caret)
                }
            };
            (
                if start < end {
                    Some((start, end))
                } else {
                    None
                },
                state.field == FindField::Find,
            )
        };

        let Some((start, end)) = deletion else {
            cx.notify();
            return;
        };
        if let Some(state) = &mut self.find {
            state.active_mut().replace_range(start..end, "");
            state.set_caret(start);
            state.set_anchor(start);
            if editing_find {
                state.reset_history();
            }
        }
        if editing_find {
            self.after_find_query_changed(cx);
        } else {
            cx.notify();
        }
    }

    /// Deletes the selection, or the grapheme after the field's caret.
    pub(crate) fn delete_forward_find(&mut self, cx: &mut Context<Self>) {
        let (deletion, editing_find) = {
            let Some(state) = &self.find else {
                return;
            };
            let text = state.active();
            let (sel_start, sel_end) = state.selection();
            let (start, end) = if sel_start != sel_end {
                (sel_start, sel_end)
            } else {
                let caret = snap_boundary(text, state.caret());
                if caret >= text.len() {
                    (0, 0)
                } else {
                    (caret, next_grapheme_end(text, caret))
                }
            };
            (
                if start < end {
                    Some((start, end))
                } else {
                    None
                },
                state.field == FindField::Find,
            )
        };

        let Some((start, end)) = deletion else {
            cx.notify();
            return;
        };
        if let Some(state) = &mut self.find {
            state.active_mut().replace_range(start..end, "");
            state.set_caret(start);
            state.set_anchor(start);
            if editing_find {
                state.reset_history();
            }
        }
        if editing_find {
            self.after_find_query_changed(cx);
        } else {
            cx.notify();
        }
    }

    /// Moves the active field's caret one grapheme left or right. With `extend`
    /// the anchor stays put (growing a selection); without it a selection
    /// collapses to its forward/backward edge.
    pub(crate) fn move_find_caret(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            return;
        };
        let (sel_start, sel_end) = state.selection();
        let target = {
            let text = state.active();
            let caret = snap_boundary(text, state.caret());
            if !extend && sel_start != sel_end {
                if forward { sel_end } else { sel_start }
            } else if forward {
                next_grapheme_end(text, caret)
            } else {
                previous_grapheme_start(text, caret)
            }
        };
        state.set_caret(target);
        if !extend {
            state.set_anchor(target);
        }
        cx.notify();
    }

    pub(crate) fn move_find_caret_home(&mut self, extend: bool, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            state.set_caret(0);
            if !extend {
                state.set_anchor(0);
            }
            cx.notify();
        }
    }

    pub(crate) fn move_find_caret_end(&mut self, extend: bool, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            let end = state.active().len();
            state.set_caret(end);
            if !extend {
                state.set_anchor(end);
            }
            cx.notify();
        }
    }

    /// Ctrl/Cmd+A: select the whole active field.
    pub(crate) fn select_all_find(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            let end = state.active().len();
            state.set_anchor(0);
            state.set_caret(end);
            cx.notify();
        }
    }

    /// Scrolls the active field horizontally just enough to keep the caret in
    /// view, clamped to the text extent. Returns `false` until the field has
    /// been laid out (the visible width comes from the recorded bounds).
    pub(crate) fn ensure_find_caret_visible(&mut self, window: &Window) -> bool {
        let Some((field, scroll, next)) = (|| {
            let state = self.find.as_ref()?;
            let field = state.field;
            let text = match field {
                FindField::Find => &state.find,
                FindField::Replace => &state.replace,
            };
            let caret = match field {
                FindField::Find => state.find_caret,
                FindField::Replace => state.replace_caret,
            };
            let bounds = match field {
                FindField::Find => self.find_query_bounds.get(),
                FindField::Replace => self.find_replace_bounds.get(),
            }?;
            let visible_width = (f32::from(bounds.size.width)
                - FIND_FIELD_PADDING
                - find_field_right_padding(field))
            .max(0.0);
            if visible_width <= 0.0 {
                return None;
            }
            let layout = window.text_system().layout_line(
                text,
                px(FIND_FIELD_TEXT_SIZE),
                &[find_field_run(text)],
                None,
            );
            let max_scroll = (f32::from(layout.width) - visible_width).max(0.0);
            let caret_x = f32::from(layout.x_for_index(snap_boundary(text, caret)));
            let scroll = match field {
                FindField::Find => state.find_scroll,
                FindField::Replace => state.replace_scroll,
            };
            let mut next = scroll;
            if caret_x < next {
                next = caret_x;
            } else if caret_x > next + visible_width {
                next = caret_x - visible_width;
            }
            next = next.clamp(0.0, max_scroll);
            Some((field, scroll, next))
        })() else {
            return false;
        };
        if (next - scroll).abs() > 0.01 {
            if let Some(state) = &mut self.find {
                match field {
                    FindField::Find => state.find_scroll = next,
                    FindField::Replace => state.replace_scroll = next,
                }
            }
        }
        true
    }

    /// Wheel/trackpad panning inside a field. Vertical wheel deltas pan
    /// horizontally too: the field is a single line, so there is nothing else
    /// for the wheel to do there.
    pub(crate) fn pan_find_scroll(
        &mut self,
        field: FindField,
        event: &ScrollEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let dx = match event.delta {
            ScrollDelta::Pixels(delta) => {
                if f32::from(delta.x) != 0.0 {
                    f32::from(delta.x)
                } else {
                    f32::from(delta.y)
                }
            }
            ScrollDelta::Lines(delta) => {
                if delta.x != 0.0 {
                    delta.x * FIND_SCROLL_LINE_PX
                } else {
                    delta.y * FIND_SCROLL_LINE_PX
                }
            }
        };
        if dx == 0.0 {
            return;
        }
        let (scroll, max_scroll) = {
            let Some(state) = &self.find else {
                return;
            };
            let text = match field {
                FindField::Find => &state.find,
                FindField::Replace => &state.replace,
            };
            let bounds = match field {
                FindField::Find => self.find_query_bounds.get(),
                FindField::Replace => self.find_replace_bounds.get(),
            };
            let Some(bounds) = bounds else {
                return;
            };
            let visible_width = (f32::from(bounds.size.width)
                - FIND_FIELD_PADDING
                - find_field_right_padding(field))
            .max(0.0);
            if visible_width <= 0.0 {
                return;
            }
            let layout = window.text_system().layout_line(
                text,
                px(FIND_FIELD_TEXT_SIZE),
                &[find_field_run(text)],
                None,
            );
            let max_scroll = (f32::from(layout.width) - visible_width).max(0.0);
            let scroll = match field {
                FindField::Find => state.find_scroll,
                FindField::Replace => state.replace_scroll,
            };
            (scroll, max_scroll)
        };
        if let Some(state) = &mut self.find {
            let next = (scroll + dx).clamp(0.0, max_scroll);
            match field {
                FindField::Find => state.find_scroll = next,
                FindField::Replace => state.replace_scroll = next,
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Maps a window point to a byte index in a find/replace field, using the
    /// field bounds recorded during paint.
    pub(crate) fn find_index_for_point(
        &self,
        field: FindField,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<usize> {
        let bounds = match field {
            FindField::Find => self.find_query_bounds.get()?,
            FindField::Replace => self.find_replace_bounds.get()?,
        };
        let state = self.find.as_ref()?;
        let text = match field {
            FindField::Find => &state.find,
            FindField::Replace => &state.replace,
        };
        let scroll = match field {
            FindField::Find => state.find_scroll,
            FindField::Replace => state.replace_scroll,
        };
        let left = f32::from(bounds.origin.x) + FIND_FIELD_PADDING;
        // The text is shifted left by the pan offset, so add it back when
        // mapping a screen x back into text space. Negative values (a click in
        // the left padding, or dragging past the edge) map to the text start
        // inside `closest_index_for_x`.
        let relative = f32::from(position.x) - left + scroll;
        Some(closest_index_for_x(text, px(relative), window))
    }

    /// Mouse down in a field: place the caret at the click and start a drag.
    pub(crate) fn begin_find_drag(
        &mut self,
        field: FindField,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback = self.find.as_ref().map(|state| match field {
            FindField::Find => state.find.len(),
            FindField::Replace => state.replace.len(),
        });
        let index = self
            .find_index_for_point(field, position, window)
            .or(fallback);
        if let Some(state) = &mut self.find {
            state.field = field;
            // Clicking into a field dismisses the Find history dropdown.
            state.history_open = false;
            if let Some(index) = index {
                state.set_caret(index);
                state.set_anchor(index);
            }
            state.field_drag = Some(field);
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Mouse move during a field drag: extend the selection to the pointer.
    pub(crate) fn update_find_drag(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(field) = self.find.as_ref().and_then(|state| state.field_drag) else {
            return;
        };
        if !event.dragging() {
            if let Some(state) = &mut self.find {
                state.field_drag = None;
            }
            return;
        }
        if let Some(index) = self.find_index_for_point(field, event.position, window) {
            if let Some(state) = &mut self.find {
                state.field = field;
                state.set_caret(index);
            }
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
        }
    }

    /// Recompiles after the query, regex flag, or case flag changed, then
    /// live-highlights the first match so the query previews results as it is
    /// typed. The view does not scroll here; `Enter` reveals the match.
    pub(crate) fn after_find_query_changed(&mut self, cx: &mut Context<Self>) {
        // Any query change invalidates the open dropdown; the list would
        // otherwise show stale entries while the live query has already moved on.
        if let Some(state) = &mut self.find {
            state.history_open = false;
        }
        let from = self
            .find
            .as_ref()
            .and_then(|state| state.current.map(|(start, _)| start))
            .unwrap_or(self.document.cursor());
        self.refresh_find(from);
        if let Some((start, end)) = self.find.as_ref().and_then(|state| state.current) {
            self.document.select_range(start, end);
        }
        cx.notify();
    }

    /// Recompiles the query and recomputes the current match at or after
    /// `from`, wrapping to the top. Called on query changes and on open, never
    /// per frame.
    pub(crate) fn refresh_find(&mut self, from: usize) {
        let Some((query, is_regex, case_insensitive)) = self
            .find
            .as_ref()
            .map(|state| (state.find.clone(), state.regex, state.case_insensitive))
        else {
            return;
        };

        let compiled = find::compile(&query, is_regex, case_insensitive);
        let (matcher, error, current) = match compiled {
            Ok(Some(matcher)) => {
                let text = self.document.text();
                let from = from.min(text.len());
                let current = find::find_next(text, &matcher, from)
                    .or_else(|| find::find_next(text, &matcher, 0));
                (Some(matcher), None, current)
            }
            Ok(None) => (None, None, None),
            Err(message) => (None, Some(message), None),
        };

        if let Some(state) = &mut self.find {
            state.matcher = matcher;
            state.error = error;
            state.current = current;
            state.wrapped = false;
        }
    }

    pub(crate) fn find_next_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record_find();
        let (next, wrapped) = {
            let Some(state) = self.find.as_ref() else {
                return;
            };
            let Some(matcher) = state.matcher.as_ref() else {
                return;
            };
            let from = state
                .current
                .map(|(_, end)| end)
                .unwrap_or(self.document.cursor());
            let text = self.document.text();
            match find::find_next(text, matcher, from) {
                Some(range) => (Some(range), false),
                None => (find::find_next(text, matcher, 0), true),
            }
        };

        if let Some(state) = &mut self.find {
            state.current = next;
            state.wrapped = wrapped && next.is_some();
        }
        match next {
            Some(range) => self.select_match(range, window, cx),
            None => self.show_hint("No matches", cx),
        }
    }

    pub(crate) fn find_prev_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record_find();
        let (previous, wrapped) = {
            let Some(state) = self.find.as_ref() else {
                return;
            };
            let Some(matcher) = state.matcher.as_ref() else {
                return;
            };
            let before = state
                .current
                .map(|(start, _)| start)
                .unwrap_or(self.document.cursor());
            let text = self.document.text();
            match find::find_prev(text, matcher, before) {
                Some(range) => (Some(range), false),
                None => (find::find_prev(text, matcher, text.len()), true),
            }
        };

        if let Some(state) = &mut self.find {
            state.current = previous;
            state.wrapped = wrapped && previous.is_some();
        }
        match previous {
            Some(range) => self.select_match(range, window, cx),
            None => self.show_hint("No matches", cx),
        }
    }

    /// Selects a match and, only if it is off-screen, scrolls it into view.
    pub(crate) fn select_match(
        &mut self,
        range: (usize, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.select_range(range.0, range.1);
        self.caret_visible = true;
        self.reveal_offset(range.0, window, cx);
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    /// Scrolls only when the row containing `offset` is outside the visible
    /// window, so stepping through nearby matches does not jerk to the top.
    /// This uses the on-screen row range, not `visible_window` (which includes
    /// overscan rows that are not actually painted).
    pub(crate) fn reveal_offset(&self, offset: usize, window: &Window, cx: &App) {
        let line_height = f32::from(self.metrics.line_height);
        let viewport_height = self.editor_height(window);
        let (row, first, last) = {
            let layout = self.wrap_layout(cx);
            let row = self.row_of_offset(&layout, offset, cx);
            let total = layout.total_rows(&self.document);
            let scroll_top = (-f32::from(self.scroll_handle.offset().y)).max(0.0);
            let first = ((scroll_top / line_height).floor() as u32).min(total.saturating_sub(1));
            let count = (viewport_height / line_height).ceil() as u32;
            let last = first.saturating_add(count).min(total.saturating_sub(1));
            (row, first, last)
        };
        if row < first || row > last {
            self.set_editor_scroll_for_text_offset(offset, cx, window);
        }
    }

    pub(crate) fn replace_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record_find();
        let Some(range) = self.find.as_ref().and_then(|state| state.current) else {
            self.show_hint("No matches", cx);
            return;
        };
        let replacement = {
            let Some(state) = self.find.as_ref() else {
                return;
            };
            let Some(matcher) = state.matcher.as_ref() else {
                return;
            };
            find::replacement_for(self.document.text(), matcher, range, &state.replace)
        };

        if self.replace_range(range.0, range.1, &replacement, window, cx) {
            // The match offsets are stale after the edit; re-find from the end
            // of the inserted text, which the model parked the caret on.
            if let Some(state) = &mut self.find {
                state.current = None;
            }
            self.find_next_match(window, cx);
        }
    }

    pub(crate) fn replace_all_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record_find();

        if !self
            .find
            .as_ref()
            .is_some_and(|state| state.matcher.is_some())
        {
            self.show_hint("No matches", cx);
            return;
        }

        let outcome = {
            let Some(state) = self.find.as_ref() else {
                return;
            };
            let Some(matcher) = state.matcher.as_ref() else {
                return;
            };
            // Cap the replacement so an expanding template is refused before
            // the document is mutated (the model enforces the same cap).
            find::replace_all(
                self.document.text(),
                matcher,
                &state.replace,
                MAX_DOCUMENT_BYTES,
            )
        };

        let new_text = match outcome {
            find::ReplaceAll::OverLimit => {
                self.show_hint("Document limit reached (64 MB).", cx);
                return;
            }
            find::ReplaceAll::Unchanged => {
                self.show_hint("No matches", cx);
                return;
            }
            find::ReplaceAll::Replaced(text) => text,
        };
        // A replacement that yields identical bytes is a no-op; `replace_range`
        // reports it as unchanged, so treat it as "No matches" for the user.
        if new_text == self.document.text() {
            self.show_hint("No matches", cx);
            return;
        }

        let end = self.document.len();
        if self.replace_range(0, end, &new_text, window, cx) {
            // Park the caret at the start of the replacement.
            self.document.set_cursor(0, false);
            self.caret_visible = true;
            if let Some(state) = &mut self.find {
                state.current = None;
                state.wrapped = false;
            }
            window.notify_text_selection_changed(&self.focus_handle);
            cx.notify();
        }
    }
}
