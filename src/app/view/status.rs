//! Overlay builders moved out of `Render::render`.
//!
//! These child modules of `app` can reach `Scratchpad`'s private state
//! and helpers, keeping `render` focused on layout.

use super::super::*;

impl Scratchpad {
    pub(crate) fn status_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Cache the cursor label by `(revision, cursor)`: the caret blink calls
        // `cx.notify()` twice a second, and counting the current line's graphemes
        // on every frame would be a whole-line scan in the worst case (a 64 MB
        // single line).
        let cursor_status = {
            let revision = self.document.revision();
            let cursor = self.document.cursor();
            let cached = match &self.cursor_status_cache {
                Some((r, c, label)) if *r == revision && *c == cursor => Some(label.clone()),
                _ => None,
            };
            match cached {
                Some(label) => label,
                None => {
                    let label = self.cursor_status_label();
                    self.cursor_status_cache = Some((revision, cursor, label.clone()));
                    label
                }
            }
        };

        let selection_status = {
            let revision = self.document.revision();
            let (start, end) = self.document.selection_range();
            if start == end {
                self.selection_status_cache = None;
                None
            } else {
                let cached = match &self.selection_status_cache {
                    Some((r, s, e, label)) if *r == revision && *s == start && *e == end => {
                        Some(label.clone())
                    }
                    _ => None,
                };
                Some(match cached {
                    Some(label) => label,
                    None => {
                        let (chars, words) = self.document.selection_stats(start, end);
                        let label = format!("S:{chars},W:{words}");
                        self.selection_status_cache = Some((revision, start, end, label.clone()));
                        label
                    }
                })
            }
        };

        let (status_message, status_color) = match &self.hint {
            Some(hint) => (hint.clone(), rgb(0xf9e2af)),
            None if self.save_failed => {
                ("Not saved: storage unavailable".to_string(), rgb(0xf38ba8))
            }
            None => (String::new(), rgb(0xa6adc8)),
        };
        let status_bar = div()
            .h(px(STATUS_BAR_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .bg(rgb(0x181825))
            .child(
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .text_size(px(12.0))
                    .text_color(status_color)
                    .child(status_message),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .children(selection_status.map(|label| {
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            // Yellow marks the selection counts, distinct from
                            // the teal cursor label and the gray toggles.
                            .text_color(rgb(0xf9e2af))
                            .child(label)
                    }))
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            // Teal distinguishes the cursor position from the
                            // toggle items next to it.
                            .text_color(rgb(0x94e2d5))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, window, cx| {
                                    this.open_jump_prompt(window, cx);
                                }),
                            )
                            .child(cursor_status),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            // Neutral label; clicking opens the conversion dialog.
                            .text_color(rgb(0xa6adc8))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.open_line_ending_prompt(cx);
                                }),
                            )
                            .child(format!("EOL: {}", self.document.line_ending().label())),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            .text_color(if self.word_wrap {
                                rgb(0xa6adc8)
                            } else {
                                rgb(0x6c7086)
                            })
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    // Large documents stay wrap-off: the per-line
                                    // wrap index would be costly to maintain.
                                    if !this.word_wrap && this.document_is_large() {
                                        this.show_hint("Wrap disabled for large file", cx);
                                        return;
                                    }
                                    this.word_wrap = !this.word_wrap;
                                    this.content_width_dirty.set(true);
                                    cx.notify();
                                }),
                            )
                            .child(format!(
                                "Wrap: {}",
                                if self.word_wrap { "on" } else { "off" }
                            )),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            .text_color(if self.show_invisibles {
                                rgb(0xa6adc8)
                            } else {
                                rgb(0x6c7086)
                            })
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.show_invisibles = !this.show_invisibles;
                                    // Escape widths change, so the longest-line
                                    // extent must be rescanned.
                                    this.content_width_dirty.set(true);
                                    cx.notify();
                                }),
                            )
                            .child(format!(
                                "Invisibles: {}",
                                if self.show_invisibles { "on" } else { "off" }
                            )),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_size(px(12.0))
                            .text_color(if self.hex_panel {
                                rgb(0xa6adc8)
                            } else {
                                rgb(0x6c7086)
                            })
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.hex_panel = !this.hex_panel;
                                    if this.hex_panel {
                                        // Align the pane to the editor on the
                                        // next render (the hex index is built
                                        // there).
                                        this.pending_hex_snap = true;
                                    } else {
                                        this.hex_drag = None;
                                        this.hex_dragging = false;
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(format!(
                                "Hex: {}",
                                if self.hex_panel { "on" } else { "off" }
                            )),
                    ),
            );
        status_bar.into_any_element()
    }
}
