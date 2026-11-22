//! Overlay builders moved out of `Render::render`.
//!
//! These child modules of `app` can reach `Scratchpad`'s private state
//! and helpers, keeping `render` focused on layout.

use super::super::*;

impl Scratchpad {
    pub(crate) fn encoding_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let encoding_overlay = self.encoding_prompt.as_ref().map(|prompt| {
            let selected = prompt.selected;
            let mut options: Vec<AnyElement> = Vec::new();
            for &encoding in encoding::ALL {
                let is_selected = encoding == selected;
                let item = div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(if is_selected {
                        rgb(0xcdd6f4)
                    } else {
                        rgb(0xa6adc8)
                    })
                    .bg(if is_selected {
                        rgb(0x313244)
                    } else {
                        rgb(0x1e1e2e)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            if let Some(prompt) = &mut this.encoding_prompt {
                                prompt.selected = encoding;
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(encoding.label());
                options.push(item.into_any_element());
            }

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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        // Clicking the dimmed backdrop cancels, like Esc and the
                        // Cancel button.
                        this.encoding_prompt = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(460.0))
                        .p(px(12.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        // Keep clicks on the dialog's own background from
                        // bubbling to the backdrop's cancel handler.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .pb(px(6.0))
                                .text_color(rgb(0xf5e0dc))
                                .child("Convert encoding"),
                        )
                        .children(options)
                        .child(
                            div()
                                .mt(px(10.0))
                                .flex()
                                .flex_row()
                                .justify_between()
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x11111b))
                                        .bg(rgb(0x94e2d5))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _event, window, cx| {
                                                this.apply_encoding(selected, true, window, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child("Reinterpret bytes (change text)"),
                                )
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x11111b))
                                        .bg(rgb(0x89b4fa))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _event, window, cx| {
                                                this.apply_encoding(selected, false, window, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child("Re-encode text (change hex)"),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(rgb(0x6c7086))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.encoding_prompt = None;
                                        cx.stop_propagation();
                                        cx.notify();
                                    }),
                                )
                                .child("Cancel"),
                        ),
                )
        });
        encoding_overlay.map(|element| element.into_any_element())
    }
}

impl Scratchpad {
    pub(crate) fn line_ending_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line_ending_overlay = self.line_ending_prompt.as_ref().map(|prompt| {
            let selected = prompt.selected;
            let current = self.document.line_ending();
            let show_line_endings = self.show_line_endings;

            let mut options: Vec<AnyElement> = Vec::new();
            for &style in LineEnding::TARGETS {
                let is_selected = style == selected;
                let item = div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(if is_selected {
                        rgb(0xcdd6f4)
                    } else {
                        rgb(0xa6adc8)
                    })
                    .bg(if is_selected {
                        rgb(0x313244)
                    } else {
                        rgb(0x1e1e2e)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            if let Some(prompt) = &mut this.line_ending_prompt {
                                prompt.selected = style;
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(style.label());
                options.push(item.into_any_element());
            }

            // Mirrors the status-bar "Invisibles" toggle: when on, each newline
            // is drawn as its style's escape sequence.
            let display_toggle = div()
                .mt(px(10.0))
                .px(px(10.0))
                .py(px(6.0))
                .text_size(px(13.0))
                .text_color(rgb(0xcdd6f4))
                .bg(rgb(0x313244))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.show_line_endings = !this.show_line_endings;
                        // Escape widths change, so the longest-line extent must
                        // be rescanned.
                        this.content_width_dirty.set(true);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(format!(
                    "Display line endings: {}",
                    if show_line_endings { "on" } else { "off" }
                ));

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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        // Backdrop click cancels, like Esc and the Cancel button.
                        this.line_ending_prompt = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(460.0))
                        .p(px(12.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        // Keep clicks on the dialog's own background from
                        // bubbling to the backdrop's cancel handler.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .pb(px(6.0))
                                .text_color(rgb(0xf5e0dc))
                                .child("Line endings"),
                        )
                        .children((current == LineEnding::Mixed).then(|| {
                            div()
                                .pb(px(6.0))
                                .text_size(px(12.0))
                                .text_color(rgb(0xf9e2af))
                                .child(
                                    "Mixed endings detected; converting replaces all line endings.",
                                )
                                .into_any_element()
                        }))
                        .children(options)
                        .child(display_toggle)
                        .child(
                            div()
                                .mt(px(10.0))
                                .flex()
                                .flex_row()
                                .justify_between()
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x11111b))
                                        .bg(rgb(0x94e2d5))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _event, _window, cx| {
                                                this.apply_line_ending(selected, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child("Convert"),
                                )
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x6c7086))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _event, _window, cx| {
                                                this.line_ending_prompt = None;
                                                cx.stop_propagation();
                                                cx.notify();
                                            }),
                                        )
                                        .child("Cancel"),
                                ),
                        ),
                )
        });
        line_ending_overlay.map(|element| element.into_any_element())
    }
}

impl Scratchpad {
    pub(crate) fn byte_edit_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let byte_edit_overlay = self.byte_edit.as_ref().map(|edit| {
            let value = if edit.value.is_empty() {
                "--".to_string()
            } else {
                edit.value.clone()
            };
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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .w(px(320.0))
                        .p(px(12.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_color(rgb(0xf5e0dc))
                                .child(format!("Edit byte 0x{:08x}", edit.offset)),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_color(rgb(0xcdd6f4))
                                .child(format!("0x{value}\u{258F}")),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(rgb(0x6c7086))
                                .child("Type two hex digits, Enter to apply, Esc to cancel"),
                        ),
                )
        });
        byte_edit_overlay.map(|element| element.into_any_element())
    }
}

impl Scratchpad {
    pub(crate) fn jump_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let jump_line_count = self.document.line_count();
        let jump_overlay = self.jump_prompt.as_ref().map(|value| {
            let shown = if value.is_empty() {
                "_".to_string()
            } else {
                format!("{value}\u{258F}")
            };
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
                // Block the editor behind the modal, like the palette.
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, window, cx| {
                        // Clicking the backdrop cancels, like Esc.
                        this.jump_prompt = None;
                        window.notify_text_input_changed(&this.focus_handle);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(320.0))
                        .p(px(12.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        // Keep clicks on the dialog from cancelling.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(div().text_color(rgb(0xf5e0dc)).child("Jump to line"))
                        .child(div().mt(px(8.0)).text_color(rgb(0xcdd6f4)).child(shown))
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(rgb(0x6c7086))
                                .child(format!(
                                    "1-{jump_line_count}. Enter to jump, Esc to cancel"
                                )),
                        ),
                )
        });
        jump_overlay.map(|element| element.into_any_element())
    }
}

impl Scratchpad {
    pub(crate) fn pending_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending_overlay = self.pending_transform.map(|pending| {
            let transform = &transforms::TRANSFORMS[pending.index];
            let mb = pending.end.saturating_sub(pending.start) as f64 / (1024.0 * 1024.0);
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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        // Clicking the dimmed backdrop cancels, like Esc.
                        this.pending_transform = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(440.0))
                        .p(px(12.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        // Keep clicks on the dialog from bubbling to the backdrop.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(div().text_color(rgb(0xf5e0dc)).child("Large transform"))
                        .child(div().mt(px(6.0)).text_color(rgb(0xcdd6f4)).child(format!(
                            "{} operates on {:.1} MB and may take a while.",
                            transform.name,
                            mb.max(0.0),
                        )))
                        .child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .flex_row()
                                .justify_between()
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0xa6adc8))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _event, window, cx| {
                                                this.cancel_pending_transform(window, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child("Cancel"),
                                )
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x11111b))
                                        .bg(rgb(0x94e2d5))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _event, window, cx| {
                                                this.confirm_pending_transform(window, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child("Run"),
                                ),
                        ),
                )
        });
        pending_overlay.map(|element| element.into_any_element())
    }
}
