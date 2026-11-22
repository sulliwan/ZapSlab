//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;
use super::geom::ScrollbarGeometry;

impl Scratchpad {
    // ---- Hex panel ------------------------------------------------------

    pub(crate) fn open_encoding_prompt(&mut self, cx: &mut Context<Self>) {
        self.encoding_prompt = Some(EncodingPrompt {
            selected: self.hex_encoding,
        });
        cx.notify();
    }

    /// Applies an encoding choice. `reinterpret` keeps the bytes and replaces
    /// the text; otherwise the text is kept and only the hex display changes.
    pub(crate) fn apply_encoding(
        &mut self,
        target: Encoding,
        reinterpret: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.encoding_prompt = None;
        let current = self.document.text().to_string();

        if reinterpret {
            let bytes = match encoding::encode(&current, self.hex_encoding) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.show_hint(
                        format!("Cannot encode as {}: {error}", self.hex_encoding.label()),
                        cx,
                    );
                    return;
                }
            };
            match encoding::decode(&bytes, target) {
                Ok(new_text) => {
                    self.hex_encoding = target;
                    let len = self.document.len();
                    // `replace_range` normalizes CR/CRLF to `\n` (a document
                    // invariant), so reinterpreting bytes containing 0x0D will
                    // not preserve those exact bytes. Every other byte
                    // round-trips losslessly.
                    self.replace_range(0, len, &new_text, window, cx);
                    self.show_hint(format!("Reinterpreted bytes as {}", target.label()), cx);
                }
                Err(error) => {
                    self.show_hint(
                        format!("Cannot decode bytes as {}: {error}", target.label()),
                        cx,
                    );
                }
            }
        } else {
            match encoding::encode(&current, target) {
                Ok(_) => {
                    self.hex_encoding = target;
                    self.show_hint(format!("Re-encoded text as {}", target.label()), cx);
                }
                Err(error) => {
                    self.show_hint(
                        format!("Cannot represent buffer as {}: {error}", target.label()),
                        cx,
                    );
                }
            }
        }
    }

    // ---- Line endings ---------------------------------------------------

    /// Opens the line-ending dialog, preselecting the current style. A `Mixed`
    /// document preselects LF, since `Mixed` is not a conversion target.
    pub(crate) fn open_line_ending_prompt(&mut self, cx: &mut Context<Self>) {
        let current = self.document.line_ending();
        let selected = if current == LineEnding::Mixed {
            LineEnding::Lf
        } else {
            current
        };
        self.line_ending_prompt = Some(LineEndingPrompt { selected });
        cx.notify();
    }

    /// Converts the whole document to `target`. O(1): the buffer is `\n`-only,
    /// so this changes only the stored style (one undo step) and needs no size
    /// check or confirmation.
    pub(crate) fn apply_line_ending(&mut self, target: LineEnding, cx: &mut Context<Self>) {
        self.line_ending_prompt = None;
        if self.document.set_line_ending(target) {
            // With line endings displayed, their escapes change every row's
            // width, so the cached longest-line extent must be rescanned.
            if self.show_line_endings {
                self.content_width_dirty.set(true);
            }
            self.schedule_save(cx);
            self.show_hint(format!("Line endings: {}", target.label()), cx);
        } else {
            self.show_hint(format!("Already {}", target.label()), cx);
        }
        cx.notify();
    }

    pub(crate) fn begin_hex_panel_drag(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let viewport_width = f32::from(window.viewport_size().width);
        let panel_left = viewport_width - self.hex_panel_width_px(viewport_width);
        self.hex_drag = Some(PanelDrag {
            grab: f32::from(position.x) - panel_left,
        });
        cx.notify();
    }

    pub(crate) fn update_hex_panel_drag(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.hex_drag else {
            return;
        };
        if !event.dragging() {
            self.hex_drag = None;
            return;
        }
        let viewport_width = f32::from(window.viewport_size().width);
        let new_left = f32::from(event.position.x) - drag.grab;
        let width = (viewport_width - new_left).clamp(
            viewport_width * HEX_PANEL_MIN_RATIO,
            viewport_width * HEX_PANEL_MAX_RATIO,
        );
        self.hex_panel_width.set(width);
        cx.notify();
    }

    /// Maps a window point over the hex panel to a text byte offset, snapping to
    /// a grapheme boundary. Returns `None` outside the rows.
    pub(crate) fn hex_offset_for_point(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> Option<usize> {
        let index = self.hex_index.as_ref()?;
        // Ignore an index that predates the current text (an edit between the
        // last render and this event); the next render rebuilds it.
        if self.hex_index_key.map(|key| key.0) != Some(self.document.revision()) {
            return None;
        }
        let metrics = self.hex_metrics();
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;
        let relative_x = f32::from(position.x) - origin_x;
        let relative_y = f32::from(position.y) - origin_y;
        if relative_y < 0.0 {
            return None;
        }
        let row_index = (relative_y / metrics.line_height).floor() as usize;
        if row_index >= index.row_count() {
            return None;
        }

        let bytes_per_row = index.bytes_per_row();
        let text = self.document.text();
        let row_encoded = row_index * bytes_per_row;
        let row_start = index
            .text_range_at(text, row_encoded)
            .map(|range| range.start);
        let last_encoded = ((row_index + 1) * bytes_per_row)
            .min(index.encoded_len())
            .saturating_sub(1);
        let row_end = index
            .text_range_at(text, last_encoded)
            .map(|range| range.end)
            .or(row_start);

        // Offset column selects the start of the row.
        if relative_x < metrics.offset_width {
            return row_start;
        }

        for column in 0..bytes_per_row {
            let encoded = row_encoded + column;
            if encoded >= index.encoded_len() {
                break;
            }
            let cell_left = metrics.offset_width + metrics.byte_x(column);
            if relative_x >= cell_left && relative_x < cell_left + metrics.byte_width {
                let range = index.text_range_at(text, encoded)?;
                return Some(if relative_x < cell_left + metrics.byte_width * 0.5 {
                    range.start
                } else {
                    range.end
                });
            }
        }

        // Past the last byte cell: snap to the end of the row.
        row_end
    }

    pub(crate) fn on_hex_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.hex_bounds.get() else {
            return;
        };
        if let Some(offset) = self.hex_offset_for_point(event.position, bounds) {
            window.focus(&self.focus_handle, cx);
            self.document.set_cursor(offset, event.modifiers.shift);
            self.caret_visible = true;
            window.notify_text_selection_changed(&self.focus_handle);
        }
        if event.click_count >= 2 {
            // Double-click a byte to edit it directly.
            self.hex_dragging = false;
            if let Some(index) = self.hex_byte_index_for_point(event.position, bounds) {
                self.byte_edit = Some(ByteEdit {
                    offset: index,
                    value: String::new(),
                });
                window.notify_text_input_changed(&self.focus_handle);
            }
        } else {
            self.hex_dragging = true;
        }
        cx.notify();
    }

    /// Encoded byte offset under a window point in the hex panel.
    pub(crate) fn hex_byte_index_for_point(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> Option<usize> {
        let index = self.hex_index.as_ref()?;
        // Ignore an index that predates the current text; the next render
        // rebuilds it.
        if self.hex_index_key.map(|key| key.0) != Some(self.document.revision()) {
            return None;
        }
        let metrics = self.hex_metrics();
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;
        let relative_x = f32::from(position.x) - origin_x;
        let relative_y = f32::from(position.y) - origin_y;
        if relative_y < 0.0 {
            return None;
        }
        let row_index = (relative_y / metrics.line_height).floor() as usize;
        if row_index >= index.row_count() {
            return None;
        }
        let bytes_per_row = index.bytes_per_row();
        let row_encoded = row_index * bytes_per_row;
        for column in 0..bytes_per_row {
            let encoded = row_encoded + column;
            if encoded >= index.encoded_len() {
                break;
            }
            let cell_left = metrics.offset_width + metrics.byte_x(column);
            if relative_x >= cell_left && relative_x < cell_left + metrics.byte_width {
                return Some(encoded);
            }
        }
        None
    }

    /// `(content_height, viewport_height, overflow)` for the hex pane's scroll
    /// area, shared by the scrollbar's paint and drag handlers.
    pub(crate) fn hex_scroll_geometry(&self, window: &Window) -> Option<(f32, f32, f32)> {
        let index = self.hex_index.as_ref()?;
        let line_height = f32::from(self.hex_line_height.get());
        let content_height = index.row_count() as f32 * line_height + 2.0 * PAD;
        let viewport_height =
            (f32::from(window.viewport_size().height) - 2.0 * STATUS_BAR_HEIGHT).max(1.0);
        let overflow = (content_height - viewport_height).max(0.0);
        Some((content_height, viewport_height, overflow))
    }

    /// Text byte offset at the top of the editor viewport: the start of the
    /// first visible visual row (a line start, or a wrapped chunk start when
    /// wrap is on). `None` only when the document has no rows.
    pub(crate) fn editor_top_offset(&self, viewport_height: f32, cx: &App) -> Option<usize> {
        let layout = self.wrap_layout(cx);
        if layout.total_rows(&self.document) == 0 {
            return None;
        }
        let (first, _) = self.visible_window(&layout, viewport_height);
        let (_, start, _) = self.row_at(&layout, cx, first);
        Some(start)
    }

    /// Text byte offset at the top of the hex viewport: the first byte of the
    /// first visible row, snapped to its grapheme start.
    pub(crate) fn hex_top_offset(&self) -> Option<usize> {
        let index = self.hex_index.as_ref()?;
        if index.encoded_len() == 0 {
            return Some(0);
        }
        let line_height = f32::from(self.hex_line_height.get());
        let scroll_top = (-f32::from(self.hex_scroll.offset().y)).max(0.0);
        let row = ((scroll_top / line_height).floor() as usize).min(index.row_count() - 1);
        index
            .text_range_at(self.document.text(), row * index.bytes_per_row())
            .map(|range| range.start)
    }

    /// Scroll the hex pane so the row containing `text_offset` is first.
    pub(crate) fn set_hex_scroll_for_text_offset(&self, text_offset: usize, window: &Window) {
        let Some(index) = self.hex_index.as_ref() else {
            return;
        };
        let Some((_, _, overflow)) = self.hex_scroll_geometry(window) else {
            return;
        };
        let line_height = f32::from(self.hex_line_height.get());
        let row = index.row_of_text_offset(self.document.text(), text_offset);
        let scroll_top = (row as f32 * line_height).clamp(0.0, overflow);
        let offset = self.hex_scroll.offset();
        self.hex_scroll.set_offset(point(offset.x, px(-scroll_top)));
    }

    /// Scroll the editor so the visual row containing `text_offset` is first.
    pub(crate) fn set_editor_scroll_for_text_offset(
        &self,
        text_offset: usize,
        cx: &App,
        window: &Window,
    ) {
        let line_height = f32::from(self.metrics.line_height);
        let (row, total_rows) = {
            let layout = self.wrap_layout(cx);
            (
                self.row_of_offset(&layout, text_offset, cx),
                layout.total_rows(&self.document),
            )
        };
        // Derive the extent here rather than reading the cached `content_height`,
        // which is only refreshed later in the frame.
        let content_height = total_rows as f32 * line_height + 2.0 * PAD;
        let overflow = (content_height - self.editor_height(window)).max(0.0);
        let scroll_top = (row as f32 * line_height).clamp(0.0, overflow);
        let offset = self.scroll_handle.offset();
        self.scroll_handle
            .set_offset(point(offset.x, px(-scroll_top)));
    }

    /// Keep the editor and hex pane showing the same region.
    ///
    /// `ScrollHandle` has no change notification, so this polls once per render:
    /// whichever handle moved since the last frame drives the other, anchored on
    /// the first visible text byte. gpui notifies the view when an overflow
    /// container scrolls, so a render always follows a scroll. Programmatic
    /// sets update `last_*` too, which is what stops a two-way feedback loop.
    pub(crate) fn sync_scroll_offsets(&mut self, window: &Window, cx: &App) {
        let editor_y = f32::from(self.scroll_handle.offset().y);
        let hex_y = f32::from(self.hex_scroll.offset().y);
        if !self.hex_panel || self.hex_index.is_none() {
            self.last_editor_scroll_y = editor_y;
            self.last_hex_scroll_y = hex_y;
            return;
        }

        if (editor_y - self.last_editor_scroll_y).abs() > SCROLL_SYNC_EPSILON {
            self.last_editor_scroll_y = editor_y;
            let viewport_height = f32::from(window.viewport_size().height);
            if let Some(top) = self.editor_top_offset(viewport_height, cx) {
                self.set_hex_scroll_for_text_offset(top, window);
            }
            self.last_hex_scroll_y = f32::from(self.hex_scroll.offset().y);
        } else if (hex_y - self.last_hex_scroll_y).abs() > SCROLL_SYNC_EPSILON {
            self.last_hex_scroll_y = hex_y;
            if let Some(top) = self.hex_top_offset() {
                self.set_editor_scroll_for_text_offset(top, cx, window);
            }
            self.last_editor_scroll_y = f32::from(self.scroll_handle.offset().y);
        }
        // Sub-epsilon deltas are left pending rather than re-baselined, so slow
        // trackpad steps accumulate until both panes move. Layout clamping is
        // bounded and never grows the delta further, so it cannot trigger a
        // false sync on its own.
    }

    pub(crate) fn update_hex_v_drag(
        &mut self,
        position_y: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.hex_v_drag else {
            return false;
        };
        let Some((content_height, viewport_height, _)) = self.hex_scroll_geometry(window) else {
            self.hex_v_drag = None;
            return false;
        };
        let track_height = (viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) = ScrollbarGeometry::new(content_height, viewport_height, track_height)
        else {
            self.hex_v_drag = None;
            return false;
        };
        let new_y = geometry.offset_for_pointer(
            position_y - STATUS_BAR_HEIGHT,
            drag.grab,
            SCROLLBAR_MARGIN,
        );
        let offset = self.hex_scroll.offset();
        self.hex_scroll.set_offset(point(offset.x, px(new_y)));
        cx.notify();
        true
    }

    pub(crate) fn begin_hex_v_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.hex_dragging = false;
        let Some((content_height, viewport_height, _)) = self.hex_scroll_geometry(window) else {
            return;
        };
        let track_height = (viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) = ScrollbarGeometry::new(content_height, viewport_height, track_height)
        else {
            return;
        };
        let grab = if centered {
            geometry.thumb / 2.0
        } else {
            let fraction = geometry.fraction_from_offset(f32::from(self.hex_scroll.offset().y));
            let thumb_top = geometry.thumb_start(SCROLLBAR_MARGIN, fraction);
            f32::from(position.y) - (STATUS_BAR_HEIGHT + thumb_top)
        };
        self.hex_v_drag = Some(ScrollDrag { grab });
        self.update_hex_v_drag(f32::from(position.y), window, cx);
    }

    pub(crate) fn push_byte_edit(&mut self, text: &str) {
        if let Some(edit) = &mut self.byte_edit {
            for ch in text.chars() {
                if ch.is_ascii_hexdigit() && edit.value.len() < 2 {
                    edit.value.push(ch.to_ascii_lowercase());
                }
            }
        }
    }

    pub(crate) fn apply_byte_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.byte_edit.take() else {
            return;
        };
        if edit.value.is_empty() {
            cx.notify();
            return;
        }
        let value = match u8::from_str_radix(&edit.value, 16) {
            Ok(value) => value,
            Err(_) => {
                self.show_hint("Enter a hex byte from 00 to FF", cx);
                self.byte_edit = Some(edit);
                return;
            }
        };
        let current = self.document.text().to_string();
        let mut bytes = match encoding::encode(&current, self.hex_encoding) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.show_hint(
                    format!("Cannot encode as {}: {error}", self.hex_encoding.label()),
                    cx,
                );
                self.byte_edit = Some(edit);
                return;
            }
        };
        if edit.offset >= bytes.len() {
            self.show_hint("That byte no longer exists", cx);
            return;
        }
        bytes[edit.offset] = value;
        match encoding::decode(&bytes, self.hex_encoding) {
            Ok(new_text) => {
                let len = self.document.len();
                self.replace_range(0, len, &new_text, window, cx);
                self.show_hint(
                    format!("Set byte 0x{:08x} to 0x{value:02x}", edit.offset),
                    cx,
                );
            }
            Err(error) => {
                self.show_hint(
                    format!(
                        "0x{value:02x} is not valid {}: {error}. Try Latin-1 for arbitrary bytes.",
                        self.hex_encoding.label()
                    ),
                    cx,
                );
                self.byte_edit = Some(edit);
            }
        }
    }

    pub(crate) fn on_hex_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.hex_dragging {
            return;
        }
        if !event.dragging() {
            self.hex_dragging = false;
            return;
        }
        let Some(bounds) = self.hex_bounds.get() else {
            return;
        };
        let Some(offset) = self.hex_offset_for_point(event.position, bounds) else {
            return;
        };
        if offset != self.document.cursor() {
            self.document.set_cursor(offset, true);
            self.caret_visible = true;
            cx.notify();
        }
    }
}
