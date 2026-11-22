//! The scratchpad editor.
//!
//! Design notes (M1):
//! - Text model is a flat `String` plus cursor/anchor byte offsets and a
//!   snapshot undo stack. Fine for scratchpad-sized documents.
//! - Rendering uses one `div` per line with the monospace `.ZedMono` font;
//!   caret and selection are painted in a `canvas` overlay using glyph
//!   advances from the text system, so geometry stays consistent with the
//!   rendered text without needing a full text-layout pipeline.
//! - Input arrives two ways on web: printable characters are routed by the
//!   platform through `EntityInputHandler::replace_text_in_range` (this is
//!   also the paste and IME path), while editing commands (arrows, backspace,
//!   enter, shortcuts) are handled in `on_key_down`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    time::Duration,
};

use zapslab_document::{
    Document, byte_offset_for_utf16, display,
    encoding::{self, Encoding},
    hexdump::HexLayout,
    utf16_offset,
};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler,
    FocusHandle, FontId, HighlightStyle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render,
    ScrollHandle, StatefulInteractiveElement, Styled, StyledText, Subscription, Task, UTF16Selection,
    Window, canvas, div, fill, font, point, px, rgb, rgba, size,
};

use crate::{storage, transforms};

const FONT_SIZE: f32 = 16.0;
/// Smaller monospace size for the dense hex dump.
const HEX_FONT_SIZE: f32 = 12.0;
const PAD: f32 = 16.0;
const CARET_WIDTH: f32 = 2.0;
const STATUS_BAR_HEIGHT: f32 = 26.0;
const SCROLLBAR_HEIGHT: f32 = 10.0;
const SCROLLBAR_MARGIN: f32 = 4.0;
const MIN_THUMB_WIDTH: f32 = 25.0;
/// Line box height as a multiple of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.618;
/// Fallback advance for the monospace font when the text system has no metric.
const FALLBACK_ASCII_WIDTH_PX: f32 = 8.0;
/// localStorage writes are synchronous; coalesce bursts of typing into one write.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// How long a transient status-bar hint stays visible.
const HINT_DURATION: Duration = Duration::from_millis(1800);
/// Default hex panel width as a fraction of the viewport.
const HEX_PANEL_DEFAULT_RATIO: f32 = 0.40;
/// Lower/upper bounds the hex panel is clamped to while resizing.
const HEX_PANEL_MIN_RATIO: f32 = 0.05;
const HEX_PANEL_MAX_RATIO: f32 = 0.85;
/// Width of the drag handle between the editor and the hex panel.
const HEX_PANEL_DRAG_HANDLE: f32 = 6.0;
/// Accent color for escaped non-printing characters.
const NON_PRINTING_COLOR: u32 = 0xf9e2af;

#[derive(Clone, Copy)]
struct Metrics {
    font_id: FontId,
    line_height: Pixels,
    ascii_width: Pixels,
}

#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
}

#[derive(Clone, Copy)]
struct ScrollDrag {
    /// Distance from the pointer to the thumb's left edge, in pixels.
    grab: f32,
}

#[derive(Clone, Copy)]
struct PanelDrag {
    /// Pointer offset from the panel's left edge, in pixels.
    grab: f32,
}

/// Modal state for changing the hex panel encoding.
#[derive(Clone, Copy)]
struct EncodingPrompt {
    selected: Encoding,
}

/// Inline editor for a single encoded byte.
struct ByteEdit {
    /// Encoded byte offset within the current encoding's byte stream.
    offset: usize,
    /// Hex digits typed so far (at most two).
    value: String,
}

/// Fixed monospace geometry for the hex panel. Widths are derived from the
/// ascii advance so rendering and hit-testing agree.
struct HexMetrics {
    line_height: f32,
    offset_width: f32,
    byte_width: f32,
    byte_gap: f32,
}

impl HexMetrics {
    fn new(ascii_width: f32, line_height: f32) -> Self {
        Self {
            line_height,
            offset_width: 9.0 * ascii_width,
            byte_width: 2.0 * ascii_width,
            byte_gap: ascii_width,
        }
    }

    /// Distance between the left edges of adjacent byte cells.
    fn step(&self) -> f32 {
        self.byte_width + self.byte_gap
    }

    /// Horizontal offset of byte cell `index` within the hex columns.
    fn byte_x(&self, index: usize) -> f32 {
        index as f32 * self.step()
    }
}

/// How many bytes fit in one row of the hex panel at the current width.
fn hex_bytes_per_row(panel_width: f32, metrics: &HexMetrics) -> usize {
    let available = (panel_width - 2.0 * PAD - metrics.offset_width).max(metrics.byte_width);
    (available / metrics.step()).floor().max(1.0) as usize
}

/// Whether a text byte range intersects the selection `[start, end)`.
fn intersects(range: &Range<usize>, start: usize, end: usize) -> bool {
    range.start < end && range.end > start
}

pub struct Scratchpad {
    focus_handle: FocusHandle,
    /// Text, cursor/anchor, line index, and undo history all live here. Keeping
    /// the model in a gpui-free crate lets it be unit-tested in isolation.
    document: Document,
    metrics: Metrics,
    /// Measured advances for characters the primary monospace font lacks — CJK
    /// via the Canvas fallback, Arabic/Hebrew/Devanagari/Thai via the embedded
    /// Noto faces. The editor font and size are fixed, so each distinct char is
    /// measured once instead of every frame. Interior mutability because the
    /// geometry helpers take `&self`.
    fallback_widths: RefCell<HashMap<char, Pixels>>,
    /// Current soft-wrap width in pixels, updated from the viewport each
    /// render. `INFINITY` means "do not wrap".
    wrap_width: Cell<f32>,
    /// Bounds of the editor content area in window coordinates, recorded in
    /// paint. Used for hit-testing and caret geometry in the input handler.
    last_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    did_autofocus: bool,
    dragging: bool,
    caret_visible: bool,
    word_wrap: bool,
    palette: Option<Palette>,
    /// Render non-printing/invisible characters as colored escape sequences.
    /// On by default: the buffer is often inspected as near-binary data.
    show_invisibles: bool,
    /// Whether the right-hand hex panel is open.
    hex_panel: bool,
    /// Encoding the hex panel renders the buffer in.
    hex_encoding: Encoding,
    /// Current hex panel width in pixels (0 until first laid out).
    hex_panel_width: Cell<f32>,
    /// Active drag on the hex panel's resize handle.
    hex_drag: Option<PanelDrag>,
    /// Whether a drag-selection is in progress in the hex panel.
    hex_dragging: bool,
    hex_scroll: ScrollHandle,
    /// Bounds of the hex content area, recorded in paint for hit-testing.
    hex_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Most recent hex layout, reused for painting and hit-testing.
    hex_layout: Option<HexLayout>,
    /// Device-pixel-snapped hex row height, refreshed each render so rows,
    /// hit-testing, and painting share one row pitch.
    hex_line_height: Cell<f32>,
    /// Modal for choosing an encoding and a conversion direction.
    encoding_prompt: Option<EncodingPrompt>,
    /// Inline editor for a single hex byte, opened by double-clicking it.
    byte_edit: Option<ByteEdit>,
    scroll_handle: ScrollHandle,
    scroll_drag: Option<ScrollDrag>,
    content_width: Cell<f32>,
    /// Debounced autosave: replaced on each edit (dropping the previous task
    /// cancels its timer). Held only so the timer stays alive.
    _save_task: Option<Task<()>>,
    /// Whether the most recent autosave attempt failed (shown in the status bar).
    save_failed: bool,
    /// Flushes the pending autosave when focus leaves the editor.
    _blur_subscription: Option<Subscription>,
    /// Transient status-bar hint, e.g. when the palette is invoked without a
    /// selection.
    hint: Option<String>,
    _hint_task: Option<Task<()>>,
    _caret_blink_task: Task<()>,
}

impl Scratchpad {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let document = Document::new(storage::load().unwrap_or_default());

        let text_system = cx.text_system();
        let font_id = text_system.resolve_font(&font(".ZedMono"));
        let ascii_width = text_system
            .ch_advance(font_id, px(FONT_SIZE))
            .unwrap_or(px(FALLBACK_ASCII_WIDTH_PX));
        let line_height = px(FONT_SIZE * LINE_HEIGHT_RATIO);

        Self {
            focus_handle: cx.focus_handle(),
            document,
            metrics: Metrics {
                font_id,
                line_height,
                ascii_width,
            },
            fallback_widths: RefCell::new(HashMap::new()),
            last_bounds: Rc::new(Cell::new(None)),
            wrap_width: Cell::new(f32::INFINITY),
            did_autofocus: false,
            dragging: false,
            caret_visible: true,
            word_wrap: true,
            palette: None,
            show_invisibles: true,
            hex_panel: false,
            hex_encoding: Encoding::Utf8,
            hex_panel_width: Cell::new(0.0),
            hex_drag: None,
            hex_dragging: false,
            hex_scroll: ScrollHandle::default(),
            hex_bounds: Rc::new(Cell::new(None)),
            hex_layout: None,
            hex_line_height: Cell::new(HEX_FONT_SIZE * LINE_HEIGHT_RATIO),
            encoding_prompt: None,
            byte_edit: None,
            scroll_handle: ScrollHandle::default(),
            scroll_drag: None,
            content_width: Cell::new(0.0),
            _save_task: None,
            save_failed: false,
            _blur_subscription: None,
            hint: None,
            _hint_task: None,
            // Holding the task keeps the blink loop alive; dropping the view
            // drops the task and cancels the loop.
            _caret_blink_task: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(530))
                        .await;
                    let updated = this.update(&mut *cx, |view, cx| {
                        view.caret_visible = !view.caret_visible;
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            }),
        }
    }

    // ---- Geometry -------------------------------------------------------

    fn char_width(&self, ch: char, cx: &App) -> Pixels {
        if ch.is_ascii() {
            return self.metrics.ascii_width;
        }
        if let Some(width) = self.fallback_widths.borrow().get(&ch).copied() {
            return width;
        }
        let text_system = cx.text_system();
        let width = match text_system.advance(self.metrics.font_id, px(FONT_SIZE), ch) {
            // The primary font covers the glyph; use its advance.
            Ok(advance) => advance.width,
            // Glyph absent from the primary font. Shape it through the full
            // pipeline so fallback fonts (Noto) and the Canvas CJK/emoji
            // fallback contribute their real advance. Guessing one cell here
            // would drift the caret and soft-wrap for wide glyphs.
            Err(_) => text_system.layout_width(self.metrics.font_id, px(FONT_SIZE), ch),
        };
        self.fallback_widths.borrow_mut().insert(ch, width);
        width
    }

    /// Width of the editor area (viewport minus the hex panel when open).
    fn editor_viewport_width(&self, viewport_width: f32) -> f32 {
        let panel = if self.hex_panel {
            self.hex_panel_width_px(viewport_width) + HEX_PANEL_DRAG_HANDLE
        } else {
            0.0
        };
        (viewport_width - panel).max(1.0)
    }

    /// Stored hex panel width, defaulted and clamped to the viewport ratios.
    fn hex_panel_width_px(&self, viewport_width: f32) -> f32 {
        let stored = self.hex_panel_width.get();
        let width = if stored > 0.0 {
            stored
        } else {
            viewport_width * HEX_PANEL_DEFAULT_RATIO
        };
        width.clamp(
            viewport_width * HEX_PANEL_MIN_RATIO,
            viewport_width * HEX_PANEL_MAX_RATIO,
        )
    }

    fn hex_metrics(&self, cx: &App) -> HexMetrics {
        let ascii_width = cx
            .text_system()
            .ch_advance(self.metrics.font_id, px(HEX_FONT_SIZE))
            .unwrap_or(px(FALLBACK_ASCII_WIDTH_PX * HEX_FONT_SIZE / FONT_SIZE));
        HexMetrics::new(f32::from(ascii_width), self.hex_line_height.get())
    }

    /// Width of a piece of display text, using the same per-glyph metric as the
    /// caret and hit-testing so escapes (`\x18` = 4 cells) line up.
    fn display_width(&self, display: &str, cx: &App) -> f32 {
        display
            .chars()
            .map(|ch| f32::from(self.char_width(ch, cx)))
            .sum()
    }

    fn width_fn<'a>(&'a self, cx: &'a App) -> impl Fn(&str) -> f32 + 'a {
        move |display| self.display_width(display, cx)
    }

    /// Visual rows after soft wrapping: `(source line, start byte, end byte)`.
    /// Wrapped continuation rows share the same source line; an empty source
    /// line always yields exactly one (empty) row. Wrapping is measured in
    /// display width, so escaped invisibles count as the width they are drawn.
    fn visual_rows(&self, cx: &App) -> Vec<(usize, usize, usize)> {
        let wrap_width = self.wrap_width.get();
        let text = self.document.text();
        let width_of = self.width_fn(cx);
        let mut rows = Vec::new();

        for line in 0..self.document.line_count() {
            let (start, end) = self.document.line_range(line);
            for (chunk_start, chunk_end) in display::wrap_line(
                text,
                start,
                end,
                wrap_width,
                self.show_invisibles,
                &width_of,
            ) {
                rows.push((line, chunk_start, chunk_end));
            }
        }

        if rows.is_empty() {
            rows.push((0, 0, 0));
        }
        rows
    }

    fn visual_row_of_offset(&self, rows: &[(usize, usize, usize)], offset: usize) -> usize {
        // A document always yields at least one row (an empty document is one
        // empty row), so indexing the result is safe.
        debug_assert!(!rows.is_empty(), "visual_rows must never be empty");
        // The last row whose start is at or before the offset. This resolves
        // wrap boundaries to the continuation row, line ends to the row that
        // actually contains the cursor, empty rows, and end-of-document.
        rows.iter()
            .rposition(|&(_, start, _)| start <= offset)
            .unwrap_or_else(|| rows.len().saturating_sub(1))
    }

    fn x_in_row(&self, start: usize, offset: usize, cx: &App) -> Pixels {
        let width_of = self.width_fn(cx);
        px(display::x_in_row(
            self.document.text(),
            start,
            offset,
            self.show_invisibles,
            &width_of,
        ))
    }

    fn offset_for_x_in_row(&self, start: usize, end: usize, x: Pixels, cx: &App) -> usize {
        let width_of = self.width_fn(cx);
        display::offset_for_x_in_row(
            self.document.text(),
            start,
            end,
            f32::from(x),
            self.show_invisibles,
            &width_of,
        )
    }

    fn offset_for_window_point(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &App,
    ) -> usize {
        let relative_x = (f32::from(position.x) - f32::from(bounds.origin.x) - PAD).max(0.0);
        let relative_y = (f32::from(position.y) - f32::from(bounds.origin.y) - PAD).max(0.0);
        let rows = self.visual_rows(cx);
        let row_index = ((relative_y / f32::from(self.metrics.line_height)).floor() as usize)
            .min(rows.len().saturating_sub(1));
        let (_, start, end) = rows[row_index];
        self.offset_for_x_in_row(start, end, px(relative_x), cx)
    }

    /// Bounds reported to the platform (IME / candidate popover). This must
    /// stay in sync with the caret painted in `paint_selection_and_caret`: both
    /// use a full line-height bar at `PAD + line_height * row`.
    fn caret_bounds(&self, offset: usize, bounds: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
        let rows = self.visual_rows(cx);
        let row_index = self.visual_row_of_offset(&rows, offset);
        let (_, start, _) = rows[row_index];
        let x = f32::from(self.x_in_row(start, offset, cx));
        let line_height = f32::from(self.metrics.line_height);
        let y = line_height * row_index as f32;
        Bounds {
            origin: point(
                px(f32::from(bounds.origin.x) + PAD + x),
                px(f32::from(bounds.origin.y) + PAD + y),
            ),
            size: size(px(CARET_WIDTH), px(line_height)),
        }
    }

    // ---- Editing --------------------------------------------------------

    fn after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.caret_visible = true;
        self.custom_after_edit(window, cx);
        cx.notify();
    }

    fn custom_after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        window.notify_text_input_changed(&self.focus_handle);
    }

    /// Coalesces bursts of edits into one debounced localStorage write. The
    /// previous timer is cancelled by replacing `_save_task`, so only the last
    /// edit in a burst actually persists.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(&mut *cx, |view, cx| view.flush_save(cx));
        }));
    }

    /// Writes the document immediately and records whether it failed, so the
    /// status bar can tell the user their text was not saved.
    fn flush_save(&mut self, cx: &mut Context<Self>) {
        self.save_failed = storage::save(self.document.text()).is_err();
        cx.notify();
    }

    fn replace_range(
        &mut self,
        start: usize,
        end: usize,
        insert: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.document.replace_range(start, end, insert) {
            self.after_edit(window, cx);
        }
    }

    fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.insert_text(text) {
            self.after_edit(window, cx);
        }
    }

    fn backspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.backspace() {
            self.after_edit(window, cx);
        }
    }

    fn delete_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.delete_forward() {
            self.after_edit(window, cx);
        }
    }

    fn set_cursor(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        self.document.set_cursor(offset, extend);
        self.caret_visible = true;
        cx.notify();
    }

    fn move_left(&mut self, extend: bool, cx: &mut Context<Self>) {
        self.document.move_left(extend);
        self.caret_visible = true;
        cx.notify();
    }

    fn move_right(&mut self, extend: bool, cx: &mut Context<Self>) {
        self.document.move_right(extend);
        self.caret_visible = true;
        cx.notify();
    }

    fn move_line(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let rows = self.visual_rows(cx);
        let cursor = self.document.cursor();
        let current = self.visual_row_of_offset(&rows, cursor);
        let (_, row_start, _) = rows[current];
        let x = self.x_in_row(row_start, cursor, cx);
        let target =
            (current as isize + delta).clamp(0, rows.len().saturating_sub(1) as isize) as usize;
        let (_, start, end) = rows[target];
        let offset = self.offset_for_x_in_row(start, end, x, cx);
        self.set_cursor(offset, extend, cx);
    }

    fn move_home(&mut self, extend: bool, cx: &mut Context<Self>) {
        self.document.move_home(extend);
        self.caret_visible = true;
        cx.notify();
    }

    fn move_end(&mut self, extend: bool, cx: &mut Context<Self>) {
        self.document.move_end(extend);
        self.caret_visible = true;
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.document.select_all();
        cx.notify();
    }

    /// Writes the current selection to the system clipboard, if non-empty.
    fn write_selection_to_clipboard(&self, cx: &mut Context<Self>) {
        let (start, end) = self.document.selection_range();
        if start != end {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.document.text()[start..end].to_string(),
            ));
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        self.write_selection_to_clipboard(cx);
    }

    fn cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (start, end) = self.document.selection_range();
        if start != end {
            self.write_selection_to_clipboard(cx);
            self.replace_range(start, end, "", window, cx);
        }
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.undo() {
            self.after_edit(window, cx);
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.redo() {
            self.after_edit(window, cx);
        }
    }

    // ---- Input ----------------------------------------------------------

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;

        // The encoding prompt is modal: swallow typing (including Escape) so it
        // never edits the document behind the overlay.
        if self.encoding_prompt.is_some() {
            if keystroke.key.as_str() == "escape" {
                self.encoding_prompt = None;
                cx.notify();
            }
            return true;
        }

        // The byte editor is modal: Enter applies, Escape cancels, Backspace
        // edits the typed value, and other keys never reach the document.
        if self.byte_edit.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.byte_edit = None;
                    window.notify_text_input_changed(&self.focus_handle);
                    cx.notify();
                }
                "enter" => self.apply_byte_edit(window, cx),
                "backspace" => {
                    if let Some(edit) = &mut self.byte_edit {
                        edit.value.pop();
                    }
                    cx.notify();
                }
                "left" | "right" | "home" | "end" | "tab" | "delete" => {}
                _ => {
                    // On web, printable keys fall through to the input handler.
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(text) = &keystroke.key_char {
                        self.push_byte_edit(text);
                        cx.notify();
                        return true;
                    }
                    return false;
                }
            }
            return true;
        }

        if self.palette.is_some() {
            return self.handle_palette_key_down(event, window, cx);
        }

        if modifiers.platform || modifiers.control {
            match keystroke.key.as_str() {
                "a" => self.select_all(cx),
                "c" => self.copy(cx),
                "x" => self.cut(window, cx),
                // Cmd/Ctrl+Shift+Space opens the transform palette. The usual
                // editor convention, Cmd/Ctrl+Shift+P, is reserved by Firefox
                // for "New Private Window" and cannot be cancelled by page
                // content, so it can't be used in a browser tab.
                "space" if modifiers.shift => self.open_palette(window, cx),
                "z" if modifiers.shift => self.redo(window, cx),
                "z" => self.undo(window, cx),
                _ => return false,
            }
            return true;
        }

        if modifiers.alt {
            return false;
        }

        match keystroke.key.as_str() {
            "backspace" => self.backspace(window, cx),
            "delete" => self.delete_forward(window, cx),
            "enter" => self.insert_text("\n", window, cx),
            "tab" => self.insert_text("    ", window, cx),
            "left" => self.move_left(modifiers.shift, cx),
            "right" => self.move_right(modifiers.shift, cx),
            "up" => self.move_line(-1, modifiers.shift, cx),
            "down" => self.move_line(1, modifiers.shift, cx),
            "home" => self.move_home(modifiers.shift, cx),
            "end" => self.move_end(modifiers.shift, cx),
            _ => {
                // On web, printable keys fall through to the platform, which
                // routes them through `replace_text_in_range` (the same path
                // used by paste and IME). Native builds have no such fallback,
                // so insert the character directly there.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(text) = &keystroke.key_char {
                    self.insert_text(text, window, cx);
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn handle_palette_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;

        if (modifiers.platform || modifiers.control) && modifiers.shift && keystroke.key == "space"
        {
            self.palette = None;
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return true;
        }

        match keystroke.key.as_str() {
            "escape" => {
                self.palette = None;
                window.notify_text_input_changed(&self.focus_handle);
                cx.notify();
            }
            "enter" => self.apply_selected_transform(window, cx),
            "up" => self.move_palette_selection(-1, cx),
            "down" => self.move_palette_selection(1, cx),
            "backspace" => {
                if let Some(palette) = &mut self.palette {
                    palette.query.pop();
                    palette.selected = 0;
                }
                cx.notify();
            }
            "left" | "right" | "home" | "end" | "tab" | "delete" => {}
            _ => {
                // On web, printable keys fall through to the input handler,
                // which appends them to the palette query.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(text) = &keystroke.key_char {
                    self.push_palette_query(text);
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (start, end) = self.document.selection_range();
        if start == end {
            // There is nothing to transform; say so instead of silently doing
            // nothing.
            self.show_hint("Select text to transform", cx);
            return;
        }
        self.palette = Some(Palette::default());
        self.dragging = false;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Shows a transient status-bar message, replacing any current hint.
    fn show_hint(&mut self, hint: impl Into<String>, cx: &mut Context<Self>) {
        self.hint = Some(hint.into());
        self._hint_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HINT_DURATION).await;
            let _ = this.update(&mut *cx, |view, cx| {
                view.hint = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn palette_matches(&self) -> Vec<usize> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.to_lowercase())
            .unwrap_or_default();
        transforms::TRANSFORMS
            .iter()
            .enumerate()
            .filter(|(_, transform)| {
                query.is_empty() || transform.name.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        if let Some(palette) = &mut self.palette {
            let current = palette.selected.min(matches.len() - 1) as isize;
            palette.selected = (current + delta).clamp(0, matches.len() as isize - 1) as usize;
            cx.notify();
        }
    }

    fn apply_selected_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let selected = self.palette.as_ref().map_or(0, |palette| palette.selected);
        let transform = &transforms::TRANSFORMS[matches[selected.min(matches.len() - 1)]];
        let apply = transform.apply;
        let (start, end) = self.document.selection_range();
        let output = apply(&self.document.text()[start..end]);
        self.palette = None;
        self.replace_range(start, end, &output, window, cx);
        cx.notify();
    }

    #[allow(dead_code)] // used by the native key path
    fn push_palette_query(&mut self, text: &str) {
        if let Some(palette) = &mut self.palette {
            palette.query.push_str(text);
            palette.selected = 0;
        }
    }

    fn update_scroll_drag(
        &mut self,
        position_x: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.scroll_drag else {
            return false;
        };
        if self.word_wrap {
            self.scroll_drag = None;
            return false;
        }
        // Recompute geometry from the current frame rather than trusting the
        // values captured when the drag started (window may have resized).
        // The editor shrinks when the hex panel is open, so measure its area.
        let viewport_width = self.editor_viewport_width(f32::from(window.viewport_size().width));
        let content_width = self.content_width.get();
        let overflow = (content_width - viewport_width).max(0.0);
        if !overflow.is_finite() || overflow <= 0.0 {
            self.scroll_drag = None;
            return false;
        }
        let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_width);
        let thumb_width =
            (track_width * viewport_width / content_width).clamp(min_thumb, track_width);
        let travel = (track_width - thumb_width).max(0.0);
        if travel <= 0.0 {
            self.scroll_drag = None;
            return false;
        }
        let fraction =
            ((f32::from(position_x) - drag.grab - SCROLLBAR_MARGIN) / travel).clamp(0.0, 1.0);
        let offset = self.scroll_handle.offset();
        self.scroll_handle
            .set_offset(point(px(-fraction * overflow), offset.y));
        cx.notify();
        true
    }

    fn begin_scroll_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Never let a scrollbar click reach the text handlers, even if the
        // geometry guard below bails out.
        cx.stop_propagation();
        self.dragging = false;

        let viewport_width = self.editor_viewport_width(f32::from(window.viewport_size().width));
        let content_width = self.content_width.get();
        let overflow = (content_width - viewport_width).max(0.0);
        if !overflow.is_finite() || overflow <= 0.0 {
            return;
        }
        let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_width);
        let thumb_width =
            (track_width * viewport_width / content_width).clamp(min_thumb, track_width);
        let travel = (track_width - thumb_width).max(0.0);
        if travel <= 0.0 {
            return;
        }
        let grab = if centered {
            thumb_width / 2.0
        } else {
            let offset_x = (-f32::from(self.scroll_handle.offset().x)).clamp(0.0, overflow);
            let thumb_left = SCROLLBAR_MARGIN + (offset_x / overflow) * travel;
            f32::from(position.x) - thumb_left
        };
        self.scroll_drag = Some(ScrollDrag { grab });
        self.update_scroll_drag(position.x, window, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        // A new press always ends any scrollbar drag (the previous release may
        // have happened outside the editor).
        self.scroll_drag = None;
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

    fn on_mouse_move(
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

    fn on_mouse_up(&mut self) {
        self.dragging = false;
        self.scroll_drag = None;
    }

    // ---- Hex panel ------------------------------------------------------

    fn open_encoding_prompt(&mut self, cx: &mut Context<Self>) {
        self.encoding_prompt = Some(EncodingPrompt {
            selected: self.hex_encoding,
        });
        cx.notify();
    }

    /// Applies an encoding choice. `reinterpret` keeps the bytes and replaces
    /// the text; otherwise the text is kept and only the hex display changes.
    fn apply_encoding(
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

    fn begin_hex_panel_drag(
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

    fn update_hex_panel_drag(
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
    fn hex_offset_for_point(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &App,
    ) -> Option<usize> {
        let layout = self.hex_layout.as_ref()?;
        let metrics = self.hex_metrics(cx);
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;
        let relative_x = f32::from(position.x) - origin_x;
        let relative_y = f32::from(position.y) - origin_y;
        if relative_y < 0.0 {
            return None;
        }
        let row_index = (relative_y / metrics.line_height).floor() as usize;
        let row = layout.rows.get(row_index)?;

        let row_start = row
            .cells
            .first()
            .map(|cell| layout.spans[cell.span].text_range.start);
        let row_end = row
            .cells
            .last()
            .map(|cell| layout.spans[cell.span].text_range.end);

        // Offset column selects the start of the row.
        if relative_x < metrics.offset_width {
            return row_start;
        }

        for (index, cell) in row.cells.iter().enumerate() {
            let cell_left = metrics.offset_width + metrics.byte_x(index);
            if relative_x >= cell_left && relative_x < cell_left + metrics.byte_width {
                let span = &layout.spans[cell.span];
                return Some(if relative_x < cell_left + metrics.byte_width * 0.5 {
                    span.text_range.start
                } else {
                    span.text_range.end
                });
            }
        }

        // Past the last byte cell: snap to the end of the row.
        row_end
    }

    fn on_hex_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.hex_bounds.get() else {
            return;
        };
        if let Some(offset) = self.hex_offset_for_point(event.position, bounds, cx) {
            window.focus(&self.focus_handle, cx);
            self.document.set_cursor(offset, event.modifiers.shift);
            self.caret_visible = true;
            window.notify_text_selection_changed(&self.focus_handle);
        }
        if event.click_count >= 2 {
            // Double-click a byte to edit it directly.
            self.hex_dragging = false;
            if let Some(index) = self.hex_byte_index_for_point(event.position, bounds, cx) {
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
    fn hex_byte_index_for_point(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &App,
    ) -> Option<usize> {
        let layout = self.hex_layout.as_ref()?;
        let metrics = self.hex_metrics(cx);
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;
        let relative_x = f32::from(position.x) - origin_x;
        let relative_y = f32::from(position.y) - origin_y;
        if relative_y < 0.0 {
            return None;
        }
        let row_index = (relative_y / metrics.line_height).floor() as usize;
        let row = layout.rows.get(row_index)?;
        for (index, cell) in row.cells.iter().enumerate() {
            let cell_left = metrics.offset_width + metrics.byte_x(index);
            if relative_x >= cell_left && relative_x < cell_left + metrics.byte_width {
                return Some(cell.encoded_offset);
            }
        }
        None
    }

    fn push_byte_edit(&mut self, text: &str) {
        if let Some(edit) = &mut self.byte_edit {
            for ch in text.chars() {
                if ch.is_ascii_hexdigit() && edit.value.len() < 2 {
                    edit.value.push(ch.to_ascii_lowercase());
                }
            }
        }
    }

    fn apply_byte_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn on_hex_mouse_move(
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
        let Some(offset) = self.hex_offset_for_point(event.position, bounds, cx) else {
            return;
        };
        if offset != self.document.cursor() {
            self.document.set_cursor(offset, true);
            self.caret_visible = true;
            cx.notify();
        }
    }

    // ---- Painting -------------------------------------------------------

    fn paint_selection_and_caret(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) {
        let line_height = f32::from(self.metrics.line_height);
        let origin_x = f32::from(bounds.origin.x) + PAD;
        let origin_y = f32::from(bounds.origin.y) + PAD;
        let rows = self.visual_rows(cx);

        let (selection_start, selection_end) = self.document.selection_range();
        if selection_start != selection_end {
            for (row_index, &(line, start, end)) in rows.iter().enumerate() {
                let (_, line_end) = self.document.line_range(line);
                let sel_start = selection_start.max(start).min(end);
                let sel_end = selection_end.min(end).max(start);
                let newline_selected =
                    end == line_end && selection_end > line_end && selection_start <= line_end;
                if sel_start == sel_end && !newline_selected {
                    continue;
                }
                let x0 = f32::from(self.x_in_row(start, sel_start, cx));
                let mut x1 = f32::from(self.x_in_row(start, sel_end, cx));
                if newline_selected {
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
        let row_index = self.visual_row_of_offset(&rows, cursor);
        let (_, row_start, _) = rows[row_index];
        let caret_x = origin_x + f32::from(self.x_in_row(row_start, cursor, cx));
        if !self.caret_visible {
            return;
        }
        // Full row height, like Zed's editor caret: glyphs are centered in the
        // line box, so a full-height bar lines up at any font size.
        let caret_y = origin_y + line_height * row_index as f32;
        let caret = Bounds {
            origin: point(px(caret_x), px(caret_y)),
            size: size(px(CARET_WIDTH), px(line_height)),
        };
        window.paint_quad(fill(caret, rgb(0x89b4fa)));
    }

    fn render_hex_pane(
        &mut self,
        panel_width: f32,
        bytes_per_row: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let metrics = self.hex_metrics(cx);
        let line_height = metrics.line_height;
        // Highlight the byte currently open in the inline editor so the cell
        // being changed stays visually tied to the "Edit byte" dialog.
        let editing_offset = self.byte_edit.as_ref().map(|edit| edit.offset);

        let content = match &self.hex_layout {
            Some(layout) => {
                let (selection_start, selection_end) = self.document.selection_range();
                let mut rows: Vec<AnyElement> = Vec::with_capacity(layout.rows.len());

                for row in &layout.rows {
                    // Paint the selection as each byte div's background rather
                    // than recomputing cell rectangles for a canvas overlay: the
                    // per-element pixel snapping gpui applies while laying out
                    // the flex row drifts a recomputed overlay further right
                    // with every column. Binding the highlight to the cell's own
                    // bounds keeps it aligned at any width.
                    let mut cells: Vec<AnyElement> = Vec::with_capacity(bytes_per_row);
                    for index in 0..bytes_per_row {
                        let Some(cell) = row.cells.get(index) else {
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
                        let selected = intersects(
                            &layout.spans[cell.span].text_range,
                            selection_start,
                            selection_end,
                        );
                        let editing = Some(cell.encoded_offset) == editing_offset;
                        let mut cell_div = div()
                            .w(px(metrics.byte_width))
                            .h(px(line_height))
                            .mr(px(metrics.byte_gap))
                            .flex_shrink_0()
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

                    rows.push(
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

                let content_width =
                    2.0 * PAD + metrics.offset_width + bytes_per_row as f32 * metrics.step();
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
                    .children(rows);

                div()
                    .relative()
                    .w(px(content_width.max(1.0)))
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
                    .id("zapslab-hex-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .w_full()
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
            .into_any_element()
    }
}

// ---- UTF-16 helpers live in the `zapslab-document` crate ---------------

impl EntityInputHandler for Scratchpad {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        if let Some(palette) = &self.palette {
            let query = &palette.query;
            let start = byte_offset_for_utf16(query, range_utf16.start);
            let end = byte_offset_for_utf16(query, range_utf16.end);
            *adjusted_range = Some(utf16_offset(query, start)..utf16_offset(query, end));
            return Some(query[start..end].to_string());
        }
        let text = self.document.text();
        let start = byte_offset_for_utf16(text, range_utf16.start);
        let end = byte_offset_for_utf16(text, range_utf16.end);
        *adjusted_range = Some(utf16_offset(text, start)..utf16_offset(text, end));
        Some(text[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(palette) = &self.palette {
            let len = utf16_offset(&palette.query, palette.query.len());
            return Some(UTF16Selection {
                range: len..len,
                reversed: false,
            });
        }
        let (start, end) = self.document.selection_range();
        let text = self.document.text();
        Some(UTF16Selection {
            range: utf16_offset(text, start)..utf16_offset(text, end),
            reversed: self.document.cursor() < self.document.anchor(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The encoding prompt is modal; ignore IME/paste input while it is up.
        if self.encoding_prompt.is_some() {
            return;
        }
        // Route typing to the inline byte editor when it is open.
        if self.byte_edit.is_some() {
            self.push_byte_edit(text);
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        if let Some(palette) = &mut self.palette {
            // Derive the replaced range and the query length from a single
            // borrow of the palette; no `expect` on an is_some() check.
            let len = palette.query.len();
            let (start, end) = match replacement_range {
                Some(range) => (
                    byte_offset_for_utf16(&palette.query, range.start),
                    byte_offset_for_utf16(&palette.query, range.end),
                ),
                None => (len, len),
            };
            let start = start.min(len);
            let end = end.min(len).max(start);
            palette.query.replace_range(start..end, text);
            palette.selected = 0;
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        let (start, end) = match replacement_range {
            Some(range) => (
                byte_offset_for_utf16(self.document.text(), range.start),
                byte_offset_for_utf16(self.document.text(), range.end),
            ),
            None => self.document.selection_range(),
        };
        self.replace_range(start, end, text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // M1 treats IME composition as plain replacement. This still inserts
        // composed text correctly; marked-text styling can come later.
        self.replace_text_in_range(range_utf16, new_text, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.palette.is_some() {
            return None;
        }
        let offset = byte_offset_for_utf16(self.document.text(), range_utf16.end);
        Some(self.caret_bounds(offset, element_bounds, cx))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if self.palette.is_some() {
            return None;
        }
        let bounds = self.last_bounds.get()?;
        let offset = self.offset_for_window_point(position, bounds, cx);
        Some(utf16_offset(self.document.text(), offset))
    }
}

impl Render for Scratchpad {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.did_autofocus {
            self.did_autofocus = true;
            window.focus(&self.focus_handle, cx);
            // Persist any pending edit as soon as the editor loses focus, so a
            // debounced save is never lost when the user clicks away.
            self._blur_subscription =
                Some(cx.on_blur(&self.focus_handle, window, |this, _window, cx| {
                    this.flush_save(cx)
                }));
        }

        // gpui rounds each authored row height to the device-pixel grid during
        // layout. Snap the row pitch here and reuse it for the caret/selection
        // overlay and hit-testing; otherwise a fractional line height makes the
        // overlay drift downward by a fraction of a pixel per row.
        self.metrics.line_height = window.pixel_snap(self.metrics.line_height);
        self.hex_line_height
            .set(f32::from(window.pixel_snap(px(self.hex_line_height.get()))));

        let viewport = window.viewport_size();
        let viewport_width = f32::from(viewport.width);
        let panel_width = if self.hex_panel {
            self.hex_panel_width_px(viewport_width)
        } else {
            0.0
        };
        if self.hex_panel {
            // Persist the clamped width so resizing and hit-testing agree.
            self.hex_panel_width.set(panel_width);
        }
        let editor_width = self.editor_viewport_width(viewport_width);
        let available_width = (editor_width - 2.0 * PAD).max(40.0);
        self.wrap_width.set(if self.word_wrap {
            available_width
        } else {
            f32::INFINITY
        });

        let line_height = f32::from(self.metrics.line_height);
        let row_ranges = self.visual_rows(cx);
        // With wrapping off there is no scroll extent unless the content is
        // given an explicit width, so size it to the longest row.
        let content_width = if self.word_wrap {
            None
        } else {
            let mut max = 0.0f32;
            for &(_, start, end) in &row_ranges {
                max = max.max(f32::from(self.x_in_row(start, end, cx)));
            }
            Some(max + 2.0 * PAD + 24.0)
        };
        self.content_width.set(content_width.unwrap_or(0.0));

        // Rebuild the hex layout for this frame. The row length is derived from
        // the panel width so the bytes fill the pane, and the layout is reused by
        // hit-testing so clicks map to the same bytes that are drawn.
        let hex_bytes = if self.hex_panel {
            hex_bytes_per_row(panel_width, &self.hex_metrics(cx))
        } else {
            1
        };
        let hex_layout = if self.hex_panel {
            HexLayout::build(self.document.text(), self.hex_encoding, hex_bytes).ok()
        } else {
            None
        };
        self.hex_layout = hex_layout;

        // Shape each visual row as a single text element. Splitting a row into
        // one flex item per run makes gpui measure each item to a whole pixel;
        // those roundings accumulate, so the caret and selection overlay drift
        // further right with every escaped run. A single `StyledText` shapes the
        // row once and only recolors escapes via highlights, so glyph positions
        // stay exact and the overlay math lines up with the drawn text.
        let rows: Vec<AnyElement> = {
            let text = self.document.text();
            row_ranges
                .iter()
                .map(|&(_, start, end)| {
                    let mut display_text = String::new();
                    let mut escaped_runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
                    let mut escaped_start: Option<usize> = None;
                    for grapheme in
                        display::display_graphemes(&text[start..end], self.show_invisibles)
                    {
                        if grapheme.escaped {
                            escaped_start.get_or_insert(display_text.len());
                        } else if let Some(run_start) = escaped_start.take() {
                            escaped_runs.push((
                                run_start..display_text.len(),
                                HighlightStyle {
                                    color: Some(rgb(NON_PRINTING_COLOR).into()),
                                    ..Default::default()
                                },
                            ));
                        }
                        display_text.push_str(&grapheme.display);
                    }
                    if let Some(run_start) = escaped_start {
                        escaped_runs.push((
                            run_start..display_text.len(),
                            HighlightStyle {
                                color: Some(rgb(NON_PRINTING_COLOR).into()),
                                ..Default::default()
                            },
                        ));
                    }
                    div()
                        .h(px(line_height))
                        .flex()
                        .flex_row()
                        .items_center()
                        .whitespace_nowrap()
                        .child(StyledText::new(display_text).with_highlights(escaped_runs))
                        .into_any_element()
                })
                .collect()
        };

        // gpui does not draw scrollbars for overflow containers, so when word
        // wrap is off we render (and drag) our own horizontal one.
        let h_scrollbar = {
            let viewport_width = editor_width;
            let content_px = content_width.unwrap_or(0.0);
            let overflow = (content_px - viewport_width).max(0.0);
            if !self.word_wrap && overflow > 0.0 && content_px.is_finite() {
                let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
                let min_thumb = MIN_THUMB_WIDTH.min(track_width);
                let thumb_width =
                    (track_width * viewport_width / content_px).clamp(min_thumb, track_width);
                let travel = (track_width - thumb_width).max(0.0);
                let offset_x = (-f32::from(self.scroll_handle.offset().x)).clamp(0.0, overflow);
                let thumb_left = SCROLLBAR_MARGIN + (offset_x / overflow) * travel;
                Some(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .justify_end()
                        .child(
                            div()
                                .h(px(SCROLLBAR_HEIGHT))
                                .w_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                        this.begin_scroll_drag(event.position, true, window, cx);
                                    }),
                                )
                                .child(div().h(px(SCROLLBAR_HEIGHT)).w(px(thumb_left)))
                                .child(
                                    div()
                                        .h(px(6.0))
                                        .w(px(thumb_width))
                                        .bg(rgb(0x45475a))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |this, event: &MouseDownEvent, window, cx| {
                                                    this.begin_scroll_drag(
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
        };

        let weak_view = cx.entity().downgrade();
        let focus_handle = self.focus_handle.clone();
        let bounds_cell = self.last_bounds.clone();

        let overlay = canvas(
            move |_bounds, _window, _cx| {},
            move |bounds, _state, window, cx| {
                bounds_cell.set(Some(bounds));

                let Some(view) = weak_view.upgrade() else {
                    return;
                };
                // Register the input handler while this element is painted, so
                // the platform can route text, paste and IME edits here.
                window.handle_input(
                    &focus_handle,
                    ElementInputHandler::new(bounds, view.clone()),
                    cx,
                );
                view.read(cx).paint_selection_and_caret(bounds, window, cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let scroll = div()
            .id("zapslab-scroll")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle);
        let scroll = if self.word_wrap {
            scroll
        } else {
            scroll.overflow_x_scroll()
        };

        let content = div().relative();
        let content = if let Some(width) = content_width {
            content.w(px(width))
        } else {
            content.w_full()
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
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.word_wrap = !this.word_wrap;
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
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.show_invisibles = !this.show_invisibles;
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
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.hex_panel = !this.hex_panel;
                                    if !this.hex_panel {
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

        let palette_overlay = self.palette.as_ref().map(|palette| {
            let matches = self.palette_matches();
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
            for (position, transform_index) in matches.iter().enumerate() {
                let transform = &transforms::TRANSFORMS[*transform_index];
                let selected = position == selected_index;
                let item = div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_color(if selected {
                        rgb(0xcdd6f4)
                    } else {
                        rgb(0xa6adc8)
                    })
                    .child(transform.name);
                let item = if selected {
                    item.bg(rgb(0x313244))
                } else {
                    item
                };
                items.push(item.into_any_element());
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
                    cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
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
                        .child(
                            div()
                                .px(px(8.0))
                                .py(px(4.0))
                                .text_color(rgb(0xf5e0dc))
                                .child(format!("> {}\u{258F}", palette.query)),
                        )
                        .children(items),
                )
        });

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

        let hex_side = if self.hex_panel {
            Some(
                div()
                    .h_full()
                    .min_h(px(0.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .w(px(HEX_PANEL_DRAG_HANDLE))
                            .h_full()
                            .flex_shrink_0()
                            .cursor_col_resize()
                            .bg(rgb(0x313244))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    this.begin_hex_panel_drag(event.position, window, cx);
                                }),
                            ),
                    )
                    .child(self.render_hex_pane(panel_width, hex_bytes, cx)),
            )
        } else {
            None
        };

        let editor_area = div()
            .flex_1()
            .relative()
            .h_full()
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.on_mouse_down(event, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.on_mouse_move(event, window, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    this.on_mouse_up();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.handle_key_down(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                scroll.child(
                    content
                        .child(
                            div()
                                .p(px(PAD))
                                .font(font(".ZedMono"))
                                .text_size(px(FONT_SIZE))
                                .line_height(px(line_height))
                                .text_color(rgb(0xcdd6f4))
                                .whitespace_nowrap()
                                .flex()
                                .flex_col()
                                .children(rows),
                        )
                        .child(overlay),
                ),
            )
            .children(h_scrollbar);

        let content_row = div()
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .w_full()
            .flex()
            .flex_row()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.hex_drag.is_some() {
                    this.update_hex_panel_drag(event, window, cx);
                } else if this.hex_dragging {
                    this.on_hex_mouse_move(event, window, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    this.hex_drag = None;
                    this.hex_dragging = false;
                }),
            )
            .child(editor_area)
            .children(hex_side);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x11111b))
            .child(content_row)
            .child(status_bar)
            .children(palette_overlay)
            .children(encoding_overlay)
            .children(byte_edit_overlay)
    }
}
