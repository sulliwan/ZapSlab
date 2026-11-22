//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;

impl Scratchpad {
    pub(crate) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        // A new press always ends any scrollbar drag (the previous release may
        // have happened outside the editor).
        self.scroll_drag = None;
        self.v_scroll_drag = None;
        if let Some(bounds) = self.last_bounds.get() {
            let offset = self.document.clamp_boundary(self.offset_for_window_point(
                event.position,
                bounds,
                cx,
            ));
            if event.click_count >= 2 {
                // Double-click selects the word (or whitespace/punctuation run)
                // under the pointer, expanded over whole grapheme clusters.
                let (start, end) = self.document.word_range_at(offset);
                self.document.set_cursor(start, false);
                self.document.set_cursor(end, true);
            } else {
                // A plain click moves the caret; shift-click extends the selection.
                self.document.set_cursor(offset, event.modifiers.shift);
            }
            self.dragging = true;
            self.caret_visible = true;
            window.notify_text_selection_changed(&self.focus_handle);
            cx.notify();
        }
    }

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scroll_drag.is_some() && !event.dragging() {
            self.scroll_drag = None;
        }
        if self.scroll_drag.is_some() {
            self.update_scroll_drag(event.position.x, _window, cx);
            return;
        }
        if self.v_scroll_drag.is_some() && !event.dragging() {
            self.v_scroll_drag = None;
        }
        if self.v_scroll_drag.is_some() {
            self.update_v_scroll_drag(f32::from(event.position.y), _window, cx);
            return;
        }
        if !self.dragging || self.palette.is_some() {
            return;
        }
        if !event.dragging() {
            self.dragging = false;
            return;
        }
        if let Some(bounds) = self.last_bounds.get() {
            let offset = self.document.clamp_boundary(self.offset_for_window_point(
                event.position,
                bounds,
                cx,
            ));
            if offset != self.document.cursor() {
                // Dragging extends the selection, keeping the anchor fixed.
                self.document.set_cursor(offset, true);
                self.caret_visible = true;
                cx.notify();
            }
        }
    }

    pub(crate) fn on_mouse_up(&mut self) {
        self.dragging = false;
        self.scroll_drag = None;
        self.v_scroll_drag = None;
    }
}
