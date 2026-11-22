//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;
use super::geom::ScrollbarGeometry;

impl Scratchpad {
    // ---- Painting -------------------------------------------------------

    pub(crate) fn paint_selection_and_caret(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &App,
    ) {
        let line_height = f32::from(self.metrics.line_height);
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;

        // Only rows in the viewport need painting overlay rectangles.
        let layout = self.wrap_layout(cx);
        let (first, last) = self.visible_window(&layout, self.editor_height(window));
        let ranges = self.visible_row_ranges(&layout, first, last, cx);

        let (selection_start, selection_end) = self.document.selection_range();
        if selection_start != selection_end {
            for &(row_index, line, start, end) in &ranges {
                let (_, line_end) = self.document.line_range(line);
                let sel_start = selection_start.max(start).min(end);
                let sel_end = selection_end.min(end).max(start);
                // When line endings are shown the newline glyph lives on the row
                // whose range includes it (`end` is one past `line_end`);
                // otherwise it belongs to the last content row (`end == line_end`).
                let last_row_of_line = if self.show_line_endings {
                    end > line_end
                } else {
                    end == line_end
                };
                let newline_selected =
                    last_row_of_line && selection_end > line_end && selection_start <= line_end;
                if sel_start == sel_end && !newline_selected {
                    continue;
                }
                let x0 = f32::from(self.x_in_row(start, sel_start, cx));
                let mut x1 = f32::from(self.x_in_row(start, sel_end, cx));
                if newline_selected && !self.show_line_endings {
                    // Without a drawn line-ending glyph, mark the selection with
                    // one cell so it stays visible.
                    x1 += f32::from(self.metrics.ascii_width);
                }
                if x1 <= x0 {
                    x1 = x0 + f32::from(self.metrics.ascii_width);
                }
                let y = origin_y + line_height * row_index as f32;
                let rect = Bounds {
                    origin: point(px(origin_x + x0), px(y)),
                    size: size(px(x1 - x0), px(line_height)),
                };
                window.paint_quad(fill(rect, rgba(0x89b4fa40)));
            }
        }

        let cursor = self.document.cursor();
        let caret_row = self.row_of_offset(&layout, cursor, cx);
        if caret_row < first || caret_row > last {
            return;
        }
        let (_, row_start, _) = self.row_at(&layout, cx, caret_row);
        let caret_x = origin_x + f32::from(self.x_in_row(row_start, cursor, cx));
        if !self.caret_visible {
            return;
        }
        // Full row height, like Zed's editor caret: glyphs are centered in the
        // line box, so a full-height bar lines up at any font size.
        let caret_y = origin_y + line_height * caret_row as f32;
        let caret = Bounds {
            origin: point(px(caret_x), px(caret_y)),
            size: size(px(CARET_WIDTH), px(line_height)),
        };
        window.paint_quad(fill(caret, rgb(0x89b4fa)));
    }

    pub(crate) fn render_hex_pane(
        &mut self,
        panel_width: f32,
        bytes_per_row: usize,
        hex_viewport_height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let metrics = self.hex_metrics();
        let line_height = metrics.line_height;
        // Highlight the byte currently open in the inline editor so the cell
        // being changed stays visually tied to the "Edit byte" dialog.
        let editing_offset = self.byte_edit.as_ref().map(|edit| edit.offset);
        // The last cell in a row has no trailing gap, so subtract one `byte_gap`
        // to avoid a permanent one-gap horizontal scroll extent.
        let content_width =
            2.0 * PAD + metrics.offset_width + bytes_per_row as f32 * metrics.step()
                - metrics.byte_gap;

        let content = match &self.hex_index {
            Some(index) => {
                // Only the rows in the viewport are materialized; the rest of the
                // scroll extent is carried by spacers, exactly like the editor.
                let total_rows = index.row_count();
                let scroll_top = (-f32::from(self.hex_scroll.offset().y)).max(0.0);
                let first =
                    ((scroll_top / line_height).floor() as usize).min(total_rows.saturating_sub(1));
                let visible = (hex_viewport_height / line_height).ceil() as usize
                    + 2 * OVERSCAN_ROWS as usize;
                let last = first
                    .saturating_add(visible)
                    .min(total_rows.saturating_sub(1));

                let total_height = total_rows as f32 * line_height + 2.0 * PAD;
                let top_spacer = first as f32 * line_height;
                let bottom_spacer =
                    total_rows.saturating_sub(last).saturating_sub(1) as f32 * line_height;

                let (selection_start, selection_end) = self.document.selection_range();
                let rows = index.rows(self.document.text(), first, last);
                let mut row_elements: Vec<AnyElement> = Vec::with_capacity(rows.len());

                for row in &rows {
                    // Paint the selection as each byte div's background rather
                    // than recomputing cell rectangles for a canvas overlay: the
                    // per-element pixel snapping gpui applies while laying out
                    // the flex row drifts a recomputed overlay further right
                    // with every column. Binding the highlight to the cell's own
                    // bounds keeps it aligned at any width.
                    let mut cells: Vec<AnyElement> = Vec::with_capacity(bytes_per_row);
                    for column in 0..bytes_per_row {
                        let Some(cell) = row.cells.get(column) else {
                            // Pad short rows so the columns line up.
                            cells.push(
                                div()
                                    .w(px(metrics.byte_width))
                                    .h(px(line_height))
                                    .mr(px(metrics.byte_gap))
                                    .flex_shrink_0()
                                    .into_any_element(),
                            );
                            continue;
                        };
                        let selected = intersects(&cell.text_range, selection_start, selection_end);
                        let editing = Some(row.encoded_offset + column) == editing_offset;
                        let mut cell_div = div()
                            .w(px(metrics.byte_width))
                            .h(px(line_height))
                            .mr(px(metrics.byte_gap))
                            .flex_shrink_0()
                            // Double-clicking a byte opens the inline editor.
                            .cursor_text()
                            .child(format!("{:02x}", cell.byte));
                        if editing {
                            // Draw the byte under edit over the selection so the
                            // target cell is unmistakable.
                            cell_div = cell_div.bg(rgb(0xf9e2af)).text_color(rgb(0x11111b));
                        } else if selected {
                            cell_div = cell_div.bg(rgba(0x89b4fa40));
                        }
                        cells.push(cell_div.into_any_element());
                    }

                    row_elements.push(
                        div()
                            .h(px(line_height))
                            .flex()
                            .flex_row()
                            .items_center()
                            .whitespace_nowrap()
                            .child(
                                div()
                                    .w(px(metrics.offset_width))
                                    .flex_shrink_0()
                                    .text_color(rgb(0x6c7086))
                                    .child(format!("{:08x}", row.encoded_offset)),
                            )
                            .children(cells)
                            .into_any_element(),
                    );
                }

                // The overlay no longer paints; it only records the content
                // bounds so pointer hit-testing maps to the same geometry.
                let bounds_cell = self.hex_bounds.clone();
                let overlay = canvas(
                    move |_bounds, _window, _cx| {},
                    move |bounds, _state, _window, _cx| bounds_cell.set(Some(bounds)),
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full();

                let padded = div()
                    .p(px(PAD))
                    .flex()
                    .flex_col()
                    .font(font(".ZedMono"))
                    .text_size(px(HEX_FONT_SIZE))
                    .line_height(px(line_height))
                    .text_color(rgb(0xa6adc8))
                    .child(div().h(px(top_spacer)))
                    .children(row_elements)
                    .child(div().h(px(bottom_spacer)));

                div()
                    .relative()
                    .w(px(content_width.max(1.0)))
                    .h(px(total_height))
                    .child(padded)
                    .child(overlay)
            }
            None => div().w_full().child(
                div()
                    .p(px(PAD))
                    .text_size(px(12.0))
                    .text_color(rgb(0xf38ba8))
                    .child("Buffer cannot be encoded in this encoding"),
            ),
        };

        // Vertical scrollbar for the hex pane (gpui draws none for overflow
        // containers), mirroring the editor's.
        let hex_scrollbar = {
            if let Some(index) = self.hex_index.as_ref() {
                let content_height = index.row_count() as f32 * line_height + 2.0 * PAD;
                let track_height = (hex_viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
                if let Some(geometry) =
                    ScrollbarGeometry::new(content_height, hex_viewport_height, track_height)
                {
                    let fraction =
                        geometry.fraction_from_offset(f32::from(self.hex_scroll.offset().y));
                    let thumb_top = geometry.thumb_start(SCROLLBAR_MARGIN, fraction);
                    Some(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .child(
                                div()
                                    .w(px(SCROLLBAR_HEIGHT))
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                            this.begin_hex_v_drag(event.position, true, window, cx);
                                        }),
                                    )
                                    .child(div().w(px(6.0)).h(px(thumb_top)))
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(geometry.thumb))
                                            .bg(rgb(0x45475a))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this, event: &MouseDownEvent, window, cx| {
                                                        this.begin_hex_v_drag(
                                                            event.position,
                                                            false,
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                ),
                                            ),
                                    ),
                            ),
                    )
                } else {
                    None
                }
            } else {
                None
            }
        };

        let header = div()
            .h(px(STATUS_BAR_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(8.0))
            .bg(rgb(0x181825))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0xa6adc8))
                    .child("Hex"),
            )
            .child(
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .text_size(px(12.0))
                    .text_color(rgb(0xcdd6f4))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            this.open_encoding_prompt(cx);
                        }),
                    )
                    .child(format!("Encoding: {}", self.hex_encoding.label())),
            );

        div()
            .w(px(panel_width))
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .relative()
            .bg(rgb(0x181825))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .w_full()
                    .child(
                        div()
                            .id("zapslab-hex-scroll")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .overflow_y_scroll()
                            .overflow_x_scroll()
                            .track_scroll(&self.hex_scroll)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    this.on_hex_mouse_down(event, window, cx);
                                }),
                            )
                            .child(content),
                    )
                    .children(hex_scrollbar),
            )
            .into_any_element()
    }
}
