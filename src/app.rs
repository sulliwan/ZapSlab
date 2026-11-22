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
    cell::{Cell, Ref, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    time::Duration,
};

use zapslab_document::{
    Document, MAX_DOCUMENT_BYTES, byte_offset_for_utf16, display,
    encoding::{self, Encoding},
    hexdump::HexIndex,
    utf16_offset,
};
use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, ElementInputHandler,
    EntityInputHandler,
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
/// Coalesce bursts of typing into one IndexedDB write.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Above this size, copying and writing the document is expensive, so autosave
/// waits longer to avoid competing with typing.
const SAVE_LARGE_THRESHOLD: usize = 4 * 1024 * 1024;
const SAVE_DEBOUNCE_LARGE: Duration = Duration::from_secs(2);
/// Running an expensive transform over more than this much text asks first.
const TRANSFORM_CONFIRM_BYTES: usize = 2 * 1024 * 1024;
/// Keep the recently-used list short enough to scan at a glance.
const RECENT_TRANSFORMS_MAX: usize = 8;
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
/// Visual rows rendered above and below the viewport to smooth scrolling.
const OVERSCAN_ROWS: u32 = 24;
/// Soft wrap is force-disabled past either bound so row layout stays O(1) per
/// frame (identity row mapping) and the per-line wrap index stays small.
const WRAP_DISABLE_BYTES: usize = 8 * 1024 * 1024;
const WRAP_DISABLE_LINES: usize = 1_000_000;
/// Scroll deltas below this are treated as layout clamping, not user scrolling,
/// when deciding which pane drives the editor/hex sync. gpui can clamp an
/// offset by sub-pixel amounts during layout, which would otherwise ping-pong
/// the two panes between frames.
const SCROLL_SYNC_EPSILON: f32 = 0.5;

#[derive(Clone, Copy)]
struct Metrics {
    font_id: FontId,
    line_height: Pixels,
    ascii_width: Pixels,
}

/// Per-line soft-wrap layout for a fixed document revision and wrap settings.
///
/// `prefix[i]` is the number of visual rows before source line `i`, so it has
/// `line_count + 1` entries and its final entry is the total row count. The
/// prefix sums turn "global row -> source line" and back into a binary search
/// instead of a full scan.
///
/// When wrapping is off every line is exactly one row, so `prefix` stays empty
/// and the mapping is the identity: large documents pay nothing for layout.
struct WrapLayout {
    on: bool,
    revision: u64,
    wrap_width: f32,
    show_invisibles: bool,
    prefix: Vec<u32>,
}

impl WrapLayout {
    /// Whether this layout still applies to the app's current document and
    /// wrap settings.
    fn matches(&self, app: &Scratchpad) -> bool {
        if self.on != app.word_wrap {
            return false;
        }
        if !self.on {
            return true;
        }
        self.revision == app.document.revision()
            && self.show_invisibles == app.show_invisibles
            // Compare rounded widths so sub-pixel jitter does not rebuild the
            // whole index on every frame.
            && self.wrap_width.round() as u32 == app.wrap_width.get().round() as u32
    }

    fn total_rows(&self, doc: &Document) -> u32 {
        if self.on {
            self.prefix.last().copied().unwrap_or(0)
        } else {
            doc.line_count() as u32
        }
    }

    /// Source line containing visual row `row`.
    fn line_of_row(&self, row: u32) -> usize {
        if !self.on {
            return row as usize;
        }
        // `prefix` is strictly increasing (every line has at least one row), so
        // the last entry `<= row` identifies the line.
        let index = self.prefix.partition_point(|&start| start <= row);
        index
            .saturating_sub(1)
            .min(self.prefix.len().saturating_sub(2))
    }

    /// First visual row of source line `line`.
    fn row_of_line(&self, line: usize) -> u32 {
        if !self.on {
            return line as u32;
        }
        self.prefix.get(line).copied().unwrap_or(0)
    }
}

#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
}

/// An expensive transform waiting for confirmation against a large range.
/// Offsets are captured when the palette applies; the modal blocks edits until
/// the user runs or cancels it.
#[derive(Clone, Copy)]
struct PendingTransform {
    index: usize,
    start: usize,
    end: usize,
    keep_selection: bool,
}

/// Floating palette description, shown by gpui's tooltip machinery when the
/// pointer rests on a row's info icon.
struct PaletteTooltip {
    text: &'static str,
}

impl Render for PaletteTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(320.0))
            .px(px(8.0))
            .py(px(6.0))
            .bg(rgb(0x313244))
            .text_color(rgb(0xcdd6f4))
            .text_size(px(12.0))
            .child(self.text)
    }
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
    /// Set when an expensive transform needs confirmation before running.
    pending_transform: Option<PendingTransform>,
    /// Scroll position of the transform list. Reset when the palette opens or
    /// the query changes; driven by the wheel/trackpad and arrow navigation.
    palette_scroll: ScrollHandle,
    /// Recently applied transform names, newest first. Persisted in
    /// localStorage and used to float them to the top of the empty palette.
    recent_transforms: Vec<String>,
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
    /// Active drag on the hex pane's vertical scrollbar thumb.
    hex_v_drag: Option<ScrollDrag>,
    /// Bounds of the hex content area, recorded in paint for hit-testing.
    hex_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Cached virtualized hex layout, rebuilt only when the document revision,
    /// encoding, or row width changes.
    hex_index: Option<HexIndex>,
    /// Key `hex_index` was built for: `(revision, encoding, bytes_per_row)`.
    hex_index_key: Option<(u64, Encoding, usize)>,
    /// Device-pixel-snapped hex row height, refreshed each render so rows,
    /// hit-testing, and painting share one row pitch.
    hex_line_height: Cell<f32>,
    /// Modal for choosing an encoding and a conversion direction.
    encoding_prompt: Option<EncodingPrompt>,
    /// Inline editor for a single hex byte, opened by double-clicking it.
    byte_edit: Option<ByteEdit>,
    scroll_handle: ScrollHandle,
    scroll_drag: Option<ScrollDrag>,
    /// Active drag on the vertical scrollbar thumb.
    v_scroll_drag: Option<ScrollDrag>,
    /// Last settled vertical offsets, used to detect which pane the user
    /// scrolled so the other can follow (see `sync_scroll_offsets`).
    last_editor_scroll_y: f32,
    last_hex_scroll_y: f32,
    /// Set when the hex pane is toggled on so the next render aligns it to the
    /// editor's first visible line instead of its stale offset.
    pending_hex_snap: bool,
    content_width: Cell<f32>,
    /// Full scrollable content height (pixels), refreshed each render so the
    /// vertical scrollbar's drag math matches the painted extent.
    content_height: Cell<f32>,
    /// Longest source-line display width. Grown incrementally on edits while
    /// word wrap is off instead of being rescanned every frame;
    /// `content_width_dirty` forces a full rescan after settings changes or
    /// whole-document edits (undo/redo).
    content_max_width: Cell<f32>,
    content_width_dirty: Cell<bool>,
    /// Cached soft-wrap layout, rebuilt lazily when the document revision or
    /// wrap settings change. Interior mutability keeps geometry helpers `&self`.
    wrap_layout: RefCell<Option<WrapLayout>>,
    /// Debounced autosave: replaced on each edit (dropping the previous task
    /// cancels its timer). Held only so the timer stays alive.
    _save_task: Option<Task<()>>,
    /// Loads the persisted document from IndexedDB asynchronously; held so the
    /// task is not cancelled before it resolves.
    _load_task: Option<Task<()>>,
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
        // The document is loaded asynchronously from IndexedDB (see
        // `_load_task`); start empty and populate it once the load resolves.
        let document = Document::new(String::new());
        let word_wrap = true;

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
            palette: None,
            pending_transform: None,
            palette_scroll: ScrollHandle::default(),
            recent_transforms: storage::load_recent(),
            show_invisibles: true,
            word_wrap,
            hex_panel: false,
            hex_encoding: Encoding::Utf8,
            hex_panel_width: Cell::new(0.0),
            hex_drag: None,
            hex_dragging: false,
            hex_scroll: ScrollHandle::default(),
            hex_v_drag: None,
            hex_bounds: Rc::new(Cell::new(None)),
            hex_index: None,
            hex_index_key: None,
            hex_line_height: Cell::new(HEX_FONT_SIZE * LINE_HEIGHT_RATIO),
            encoding_prompt: None,
            byte_edit: None,
            scroll_handle: ScrollHandle::default(),
            scroll_drag: None,
            v_scroll_drag: None,
            last_editor_scroll_y: 0.0,
            last_hex_scroll_y: 0.0,
            pending_hex_snap: false,
            content_width: Cell::new(0.0),
            content_height: Cell::new(0.0),
            content_max_width: Cell::new(0.0),
            content_width_dirty: Cell::new(true),
            wrap_layout: RefCell::new(None),
            _save_task: None,
            _load_task: Some(cx.spawn(async move |this, cx| {
                let text = storage::load().await;
                let _ = this.update(&mut *cx, |view, cx| {
                    // Do not clobber anything the user typed while loading.
                    if !view.document.is_empty() {
                        return;
                    }
                    view.document = Document::new(text.unwrap_or_default());
                    view.word_wrap = !view.document_is_large();
                    view.content_width_dirty.set(true);
                    // The replacement document starts at revision 0, colliding with
                    // the empty boot document's revision. Clear the revision-keyed
                    // caches so they rebuild instead of reusing layout computed for
                    // the empty buffer (which would show only the first line).
                    *view.wrap_layout.borrow_mut() = None;
                    view.hex_index = None;
                    view.hex_index_key = None;
                    view.caret_visible = true;
                    cx.notify();
                });
            })),
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

    /// Height of the editor area: the viewport minus the status bar (the editor
    /// and hex panel sit above it).
    fn editor_height(&self, window: &Window) -> f32 {
        (f32::from(window.viewport_size().height) - STATUS_BAR_HEIGHT).max(1.0)
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

    /// Borrow the cached wrap layout, rebuilding it first if the document
    /// revision or wrap settings changed. The cache lives behind `RefCell` so
    /// the geometry helpers can stay `&self`.
    fn wrap_layout(&self, cx: &App) -> Ref<'_, WrapLayout> {
        let stale = match self.wrap_layout.borrow().as_ref() {
            Some(layout) => !layout.matches(self),
            None => true,
        };
        if stale {
            let layout = self.compute_wrap_layout(cx);
            *self.wrap_layout.borrow_mut() = Some(layout);
        }
        Ref::map(self.wrap_layout.borrow(), |slot| {
            slot.as_ref().expect("wrap layout was just ensured")
        })
    }

    /// Build the per-line wrap index. When wrapping is on this scans every line
    /// once (`wrap_count` does not allocate); when it is off it is a no-op.
    fn compute_wrap_layout(&self, cx: &App) -> WrapLayout {
        if !self.word_wrap {
            return WrapLayout {
                on: false,
                revision: self.document.revision(),
                wrap_width: f32::INFINITY,
                show_invisibles: self.show_invisibles,
                prefix: Vec::new(),
            };
        }

        let wrap_width = self.wrap_width.get();
        let text = self.document.text();
        let width_of = self.width_fn(cx);
        let mut prefix = Vec::with_capacity(self.document.line_count() + 1);
        prefix.push(0u32);
        let mut total = 0u32;
        for line in 0..self.document.line_count() {
            let (start, end) = self.document.line_range(line);
            let count = display::wrap_count(
                text,
                start,
                end,
                wrap_width,
                self.show_invisibles,
                &width_of,
            ) as u32;
            total = total.saturating_add(count);
            prefix.push(total);
        }

        WrapLayout {
            on: true,
            revision: self.document.revision(),
            wrap_width,
            show_invisibles: self.show_invisibles,
            prefix,
        }
    }

    /// Inclusive range of visual rows currently in (and just around) the
    /// viewport, derived from the scroll offset and the uniform row height.
    fn visible_window(&self, layout: &WrapLayout, viewport_height: f32) -> (u32, u32) {
        let line_height = f32::from(self.metrics.line_height);
        let total = layout.total_rows(&self.document);
        let scroll_top = (-f32::from(self.scroll_handle.offset().y)).max(0.0);
        let first = ((scroll_top / line_height).floor() as u32).min(total.saturating_sub(1));
        let count = (viewport_height / line_height).ceil() as u32 + 2 * OVERSCAN_ROWS;
        let last = first.saturating_add(count).min(total.saturating_sub(1));
        (first, last)
    }

    /// Resolve the visible rows to `(global row, source line, start, end)`.
    ///
    /// Rows are grouped by source line so `wrap_line` runs once per line rather
    /// than once per row — important for a single long wrapped line.
    fn visible_row_ranges(
        &self,
        layout: &WrapLayout,
        first: u32,
        last: u32,
        cx: &App,
    ) -> Vec<(u32, usize, usize, usize)> {
        let text = self.document.text();
        let width_of = self.width_fn(cx);
        let mut rows = Vec::with_capacity((last.saturating_sub(first) as usize) + 1);

        let first_line = layout.line_of_row(first);
        let last_line = layout.line_of_row(last);
        for line in first_line..=last_line {
            let (start, end) = self.document.line_range(line);
            let base = layout.row_of_line(line);
            if layout.on {
                for (index, (chunk_start, chunk_end)) in display::wrap_line(
                    text,
                    start,
                    end,
                    layout.wrap_width,
                    layout.show_invisibles,
                    &width_of,
                )
                .into_iter()
                .enumerate()
                {
                    let row = base + index as u32;
                    if row >= first && row <= last {
                        rows.push((row, line, chunk_start, chunk_end));
                    }
                }
            } else {
                rows.push((base, line, start, end));
            }
        }
        rows
    }

    /// `(source line, start byte, end byte)` for a single visual row.
    fn row_at(&self, layout: &WrapLayout, cx: &App, index: u32) -> (usize, usize, usize) {
        let line = layout.line_of_row(index);
        let (start, end) = self.document.line_range(line);
        if !layout.on {
            return (line, start, end);
        }
        let chunk = index.saturating_sub(layout.row_of_line(line)) as usize;
        display::wrap_line(
            self.document.text(),
            start,
            end,
            layout.wrap_width,
            layout.show_invisibles,
            &self.width_fn(cx),
        )
        .get(chunk)
        .map(|&(chunk_start, chunk_end)| (line, chunk_start, chunk_end))
        .unwrap_or((line, start, end))
    }

    /// Global visual row containing byte `offset`.
    fn row_of_offset(&self, layout: &WrapLayout, offset: usize, cx: &App) -> u32 {
        let line = self.document.line_of_offset(offset);
        let base = layout.row_of_line(line);
        if !layout.on {
            return base;
        }
        let (start, end) = self.document.line_range(line);
        let chunk = display::wrap_line(
            self.document.text(),
            start,
            end,
            layout.wrap_width,
            layout.show_invisibles,
            &self.width_fn(cx),
        )
        .iter()
        .rposition(|&(chunk_start, _)| chunk_start <= offset)
        .unwrap_or(0);
        base + chunk as u32
    }

    /// Build one rendered row element from a display byte range.
    fn row_element(&self, start: usize, end: usize) -> AnyElement {
        let text = self.document.text();
        let mut display_text = String::new();
        let mut escaped_runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        let mut escaped_start: Option<usize> = None;

        for grapheme in display::display_graphemes(&text[start..end], self.show_invisibles) {
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
            .h(px(f32::from(self.metrics.line_height)))
            .flex()
            .flex_row()
            .items_center()
            .whitespace_nowrap()
            .child(StyledText::new(display_text).with_highlights(escaped_runs))
            .into_any_element()
    }

    /// Longest line's display width across the whole document. Used only when
    /// the incremental max is invalidated and a full rescan is required.
    fn measure_all_line_widths(&self, cx: &App) -> f32 {
        let mut max = 0.0f32;
        for line in 0..self.document.line_count() {
            let (start, end) = self.document.line_range(line);
            max = max.max(f32::from(self.x_in_row(start, end, cx)));
        }
        max
    }

    /// Grow the cached longest-line width by measuring the lines touched by an
    /// edit. Deletions do not shrink it (a slightly wide scroll extent is
    /// harmless and the next full rescan corrects it).
    fn grow_content_width_around(&self, start: usize, end: usize, cx: &App) {
        let last_line = self.document.line_count().saturating_sub(1);
        let first = self.document.line_of_offset(start).min(last_line);
        let last = self.document.line_of_offset(end).min(last_line);
        let mut max = self.content_max_width.get();
        for line in first..=last {
            let (line_start, line_end) = self.document.line_range(line);
            max = max.max(f32::from(self.x_in_row(line_start, line_end, cx)));
        }
        self.content_max_width.set(max);
    }

    fn document_is_large(&self) -> bool {
        self.document.len() > WRAP_DISABLE_BYTES || self.document.line_count() > WRAP_DISABLE_LINES
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
        let line_height = f32::from(self.metrics.line_height);
        let layout = self.wrap_layout(cx);
        let total = layout.total_rows(&self.document);
        let row = ((relative_y / line_height).floor() as u32).min(total.saturating_sub(1));
        let (_, start, end) = self.row_at(&layout, cx, row);
        self.offset_for_x_in_row(start, end, px(relative_x), cx)
    }

    /// Bounds reported to the platform (IME / candidate popover). This must
    /// stay in sync with the caret painted in `paint_selection_and_caret`: both
    /// use a full line-height bar at `PAD + line_height * row`.
    fn caret_bounds(&self, offset: usize, bounds: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
        let layout = self.wrap_layout(cx);
        let row_index = self.row_of_offset(&layout, offset, cx);
        let (_, start, _) = self.row_at(&layout, cx, row_index);
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
        self.maybe_disable_wrap(cx);
        self.custom_after_edit(window, cx);
        cx.notify();
    }

    /// Force soft wrap off once the document is large enough that per-line wrap
    /// layout would be costly. The status-bar toggle refuses to re-enable it
    /// while the document stays large (`on_mouse_down` on the toggle).
    fn maybe_disable_wrap(&mut self, cx: &mut Context<Self>) {
        if self.word_wrap && self.document_is_large() {
            self.word_wrap = false;
            self.content_width_dirty.set(true);
            self.show_hint("Wrap disabled for large file", cx);
        }
    }

    fn custom_after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        window.notify_text_input_changed(&self.focus_handle);
    }

    /// Coalesces bursts of edits into one debounced IndexedDB write. The
    /// previous timer is cancelled by replacing `_save_task`, so only the last
    /// edit in a burst actually persists.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let debounce = if self.document.len() > SAVE_LARGE_THRESHOLD {
            SAVE_DEBOUNCE_LARGE
        } else {
            SAVE_DEBOUNCE
        };
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(debounce).await;
            // Copy the text only once the debounce fires, not on every keystroke.
            let Ok(text) = this.update(&mut *cx, |view, _cx| view.document.text().to_string())
            else {
                return;
            };
            let failed = storage::save(&text).await.is_err();
            let _ = this.update(&mut *cx, |view, cx| {
                view.save_failed = failed;
                cx.notify();
            });
        }));
    }

    /// Flushes the pending autosave immediately (e.g. when focus leaves), so a
    /// debounced edit is never lost.
    fn flush_save(&mut self, cx: &mut Context<Self>) {
        let text = self.document.text().to_string();
        self._save_task = Some(cx.spawn(async move |this, cx| {
            let failed = storage::save(&text).await.is_err();
            let _ = this.update(&mut *cx, |view, cx| {
                view.save_failed = failed;
                cx.notify();
            });
        }));
    }

    /// Refuse an edit that would push the document past the size cap. The paste
    /// path checks the pasted chunk itself first (with its own message) before
    /// reaching this; everything else reports the generic limit here.
    fn accept_insert_len(
        &mut self,
        start: usize,
        end: usize,
        insert_len: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let replaced = end.saturating_sub(start);
        let resulting = self
            .document
            .len()
            .saturating_sub(replaced)
            .saturating_add(insert_len);
        if resulting <= MAX_DOCUMENT_BYTES {
            return true;
        }
        self.show_hint("Document limit reached (64 MB).", cx);
        false
    }

    fn replace_range(
        &mut self,
        start: usize,
        end: usize,
        insert: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.accept_insert_len(start, end, insert.len(), cx) {
            return false;
        }
        if self.document.replace_range(start, end, insert) {
            if !self.word_wrap {
                self.grow_content_width_around(start, self.document.cursor(), cx);
            }
            self.after_edit(window, cx);
            true
        } else {
            false
        }
    }

    /// Replaces `start..end` and, when `keep_selection` is set, re-selects the
    /// inserted output so a transform leaves its result highlighted.
    fn replace_range_keeping_selection(
        &mut self,
        start: usize,
        end: usize,
        insert: &str,
        keep_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.replace_range(start, end, insert, window, cx);
        if keep_selection {
            // A no-op replace leaves the cursor alone; otherwise the document
            // parks it at the end of the (newline-normalized) inserted text.
            let new_end = if changed { self.document.cursor() } else { end };
            self.document.select_range(start, new_end);
            cx.notify();
        }
    }

    fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (start, end) = self.document.selection_range();
        if !self.accept_insert_len(start, end, text.len(), cx) {
            return;
        }
        if self.document.insert_text(text) {
            if !self.word_wrap {
                self.grow_content_width_around(start, self.document.cursor(), cx);
            }
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
        let cursor = self.document.cursor();
        let offset = {
            let layout = self.wrap_layout(cx);
            let total = layout.total_rows(&self.document);
            let current = self.row_of_offset(&layout, cursor, cx);
            let (_, row_start, _) = self.row_at(&layout, cx, current);
            let x = self.x_in_row(row_start, cursor, cx);
            let target =
                (current as i64 + delta as i64).clamp(0, total.saturating_sub(1) as i64) as u32;
            let (_, start, end) = self.row_at(&layout, cx, target);
            self.offset_for_x_in_row(start, end, x, cx)
        };
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
            // Undo replaces the text wholesale, so the incremental width max is
            // no longer trustworthy.
            self.content_width_dirty.set(true);
            self.after_edit(window, cx);
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.document.redo() {
            self.content_width_dirty.set(true);
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

        // The transform confirmation is modal: Enter runs it, Escape cancels.
        if self.pending_transform.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.cancel_pending_transform(window, cx),
                "enter" => self.confirm_pending_transform(window, cx),
                _ => {}
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
                // A changed query re-ranks the list; show it from the top.
                self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
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
        // Open even with nothing selected: most transforms fall back to the
        // whole document (Boop's `state.text`), and selection-only entries are
        // greyed out in the list instead.
        self.palette = Some(Palette::default());
        self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
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

    /// Palette entries matching the query, best match first. Recently used
    /// transforms lead when the query is empty.
    fn palette_matches(&self) -> Vec<usize> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.as_str())
            .unwrap_or("");
        transforms::search(query, &self.recent_transforms)
    }

    fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
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
                let index = matches[current as usize];
                if transforms::is_available(&transforms::TRANSFORMS[index], has_selection) {
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

    fn apply_selected_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let selected = self.palette.as_ref().map_or(0, |palette| palette.selected);
        let index = matches[selected.min(matches.len() - 1)];
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
    fn run_transform(
        &mut self,
        index: usize,
        start: usize,
        end: usize,
        keep_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let transform = transforms::TRANSFORMS[index];
        let result = (transform.apply)(&self.document.text()[start..end]);
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
    fn confirm_pending_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn cancel_pending_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_transform = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Promotes `name` to the front of the recently-used list and persists it.
    /// Persistence is a convenience; a failed write is intentionally ignored so
    /// it cannot disrupt the edit.
    fn record_recent_transform(&mut self, name: &'static str) {
        self.recent_transforms.retain(|recent| recent != name);
        self.recent_transforms.insert(0, name.to_string());
        self.recent_transforms.truncate(RECENT_TRANSFORMS_MAX);
        let _ = storage::save_recent(&self.recent_transforms);
    }

    #[allow(dead_code)] // used by the native key path
    fn push_palette_query(&mut self, text: &str) {
        if let Some(palette) = &mut self.palette {
            palette.query.push_str(text);
            palette.selected = 0;
        }
        self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
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

    fn update_v_scroll_drag(
        &mut self,
        position_y: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.v_scroll_drag else {
            return false;
        };
        // Recompute geometry from the current frame rather than trusting the
        // values captured when the drag started (window may have resized).
        let editor_height = self.editor_height(window);
        let content_height = self.content_height.get();
        let overflow = (content_height - editor_height).max(0.0);
        if !overflow.is_finite() || overflow <= 0.0 {
            self.v_scroll_drag = None;
            return false;
        }
        let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_height);
        let thumb_height =
            (track_height * editor_height / content_height).clamp(min_thumb, track_height);
        let travel = (track_height - thumb_height).max(0.0);
        if travel <= 0.0 {
            self.v_scroll_drag = None;
            return false;
        }
        let fraction = ((position_y - drag.grab - SCROLLBAR_MARGIN) / travel).clamp(0.0, 1.0);
        let offset = self.scroll_handle.offset();
        self.scroll_handle
            .set_offset(point(offset.x, px(-fraction * overflow)));
        cx.notify();
        true
    }

    fn begin_v_scroll_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        // Never let a scrollbar click reach the text handlers.
        cx.stop_propagation();
        self.dragging = false;

        let editor_height = self.editor_height(window);
        let content_height = self.content_height.get();
        let overflow = (content_height - editor_height).max(0.0);
        if !overflow.is_finite() || overflow <= 0.0 {
            return;
        }
        let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_height);
        let thumb_height =
            (track_height * editor_height / content_height).clamp(min_thumb, track_height);
        let travel = (track_height - thumb_height).max(0.0);
        if travel <= 0.0 {
            return;
        }
        let grab = if centered {
            thumb_height / 2.0
        } else {
            let offset_y = (-f32::from(self.scroll_handle.offset().y)).clamp(0.0, overflow);
            let thumb_top = SCROLLBAR_MARGIN + (offset_y / overflow) * travel;
            f32::from(position.y) - thumb_top
        };
        self.v_scroll_drag = Some(ScrollDrag { grab });
        self.update_v_scroll_drag(f32::from(position.y), window, cx);
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

    fn on_mouse_up(&mut self) {
        self.dragging = false;
        self.scroll_drag = None;
        self.v_scroll_drag = None;
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
        let index = self.hex_index.as_ref()?;
        // Ignore an index that predates the current text (an edit between the
        // last render and this event); the next render rebuilds it.
        if self.hex_index_key.map(|key| key.0) != Some(self.document.revision()) {
            return None;
        }
        let metrics = self.hex_metrics(cx);
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
        let index = self.hex_index.as_ref()?;
        // Ignore an index that predates the current text; the next render
        // rebuilds it.
        if self.hex_index_key.map(|key| key.0) != Some(self.document.revision()) {
            return None;
        }
        let metrics = self.hex_metrics(cx);
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
    fn hex_scroll_geometry(&self, window: &Window) -> Option<(f32, f32, f32)> {
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
    fn editor_top_offset(&self, viewport_height: f32, cx: &App) -> Option<usize> {
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
    fn hex_top_offset(&self) -> Option<usize> {
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
    fn set_hex_scroll_for_text_offset(&self, text_offset: usize, window: &Window) {
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
    fn set_editor_scroll_for_text_offset(&self, text_offset: usize, cx: &App, window: &Window) {
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
    fn sync_scroll_offsets(&mut self, window: &Window, cx: &App) {
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

    fn update_hex_v_drag(
        &mut self,
        position_y: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.hex_v_drag else {
            return false;
        };
        let Some((content_height, viewport_height, overflow)) = self.hex_scroll_geometry(window)
        else {
            self.hex_v_drag = None;
            return false;
        };
        if overflow <= 0.0 {
            self.hex_v_drag = None;
            return false;
        }
        let track_height = (viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_height);
        let thumb_height =
            (track_height * viewport_height / content_height).clamp(min_thumb, track_height);
        let travel = (track_height - thumb_height).max(0.0);
        if travel <= 0.0 {
            self.hex_v_drag = None;
            return false;
        }
        let fraction = ((position_y - STATUS_BAR_HEIGHT - drag.grab - SCROLLBAR_MARGIN) / travel)
            .clamp(0.0, 1.0);
        let offset = self.hex_scroll.offset();
        self.hex_scroll
            .set_offset(point(offset.x, px(-fraction * overflow)));
        cx.notify();
        true
    }

    fn begin_hex_v_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.hex_dragging = false;
        let Some((content_height, viewport_height, overflow)) = self.hex_scroll_geometry(window)
        else {
            return;
        };
        if overflow <= 0.0 {
            return;
        }
        let track_height = (viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let min_thumb = MIN_THUMB_WIDTH.min(track_height);
        let thumb_height =
            (track_height * viewport_height / content_height).clamp(min_thumb, track_height);
        let travel = (track_height - thumb_height).max(0.0);
        if travel <= 0.0 {
            return;
        }
        let grab = if centered {
            thumb_height / 2.0
        } else {
            let offset_y = (-f32::from(self.hex_scroll.offset().y)).clamp(0.0, overflow);
            let thumb_top = SCROLLBAR_MARGIN + (offset_y / overflow) * travel;
            f32::from(position.y) - (STATUS_BAR_HEIGHT + thumb_top)
        };
        self.hex_v_drag = Some(ScrollDrag { grab });
        self.update_hex_v_drag(f32::from(position.y), window, cx);
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

        // Only rows in the viewport need painting overlay rectangles.
        let layout = self.wrap_layout(cx);
        let (first, last) = self.visible_window(&layout, f32::from(window.viewport_size().height));
        let ranges = self.visible_row_ranges(&layout, first, last, cx);

        let (selection_start, selection_end) = self.document.selection_range();
        if selection_start != selection_end {
            for &(row_index, line, start, end) in &ranges {
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

    fn render_hex_pane(
        &mut self,
        panel_width: f32,
        bytes_per_row: usize,
        hex_viewport_height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let metrics = self.hex_metrics(cx);
        let line_height = metrics.line_height;
        // Highlight the byte currently open in the inline editor so the cell
        // being changed stays visually tied to the "Edit byte" dialog.
        let editing_offset = self.byte_edit.as_ref().map(|edit| edit.offset);
        let content_width =
            2.0 * PAD + metrics.offset_width + bytes_per_row as f32 * metrics.step();

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
                let overflow = (content_height - hex_viewport_height).max(0.0);
                if overflow > 0.0 {
                    let track_height = (hex_viewport_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
                    let min_thumb = MIN_THUMB_WIDTH.min(track_height);
                    let thumb_height = (track_height * hex_viewport_height / content_height)
                        .clamp(min_thumb, track_height);
                    let travel = (track_height - thumb_height).max(0.0);
                    let offset_y = (-f32::from(self.hex_scroll.offset().y)).clamp(0.0, overflow);
                    let thumb_top = SCROLLBAR_MARGIN + (offset_y / overflow) * travel;
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
                                            .h(px(thumb_height))
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
        let start = self.document.byte_offset_for_utf16(range_utf16.start);
        let end = self.document.byte_offset_for_utf16(range_utf16.end);
        *adjusted_range = Some(self.document.utf16_offset(start)..self.document.utf16_offset(end));
        Some(self.document.text()[start..end].to_string())
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
        Some(UTF16Selection {
            range: self.document.utf16_offset(start)..self.document.utf16_offset(end),
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
        // The transform confirmation is modal; ignore input while it is up.
        if self.pending_transform.is_some() {
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
            self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        // A single chunk larger than the cap is a too-large paste; refuse it
        // outright with the paste-specific message before doing any work.
        if text.len() > MAX_DOCUMENT_BYTES {
            self.show_hint("Attempted paste was too large.", cx);
            return;
        }
        let (start, end) = match replacement_range {
            Some(range) => (
                self.document.byte_offset_for_utf16(range.start),
                self.document.byte_offset_for_utf16(range.end),
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
        let offset = self.document.byte_offset_for_utf16(range_utf16.end);
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
        Some(self.document.utf16_offset(offset))
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
        let viewport_height = f32::from(viewport.height);
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

        // With wrapping off the content needs an explicit width for the
        // horizontal scroll extent. The longest line is cached grow-only and
        // only fully rescanned when the wrap settings change or undo/redo
        // replaces the text, so large documents are not measured every frame.
        let content_width = if self.word_wrap {
            None
        } else {
            if self.content_width_dirty.get() {
                let max = self.measure_all_line_widths(cx);
                self.content_max_width.set(max);
                self.content_width_dirty.set(false);
            }
            Some(self.content_max_width.get() + 2.0 * PAD + 24.0)
        };
        self.content_width.set(content_width.unwrap_or(0.0));

        // The row length is derived from the panel width so the bytes fill the
        // pane. The virtualized index is rebuilt only when the document revision,
        // encoding, or row width changes (not every frame), and hit-testing reads
        // the same index. It must exist before the scroll sync below, which maps
        // text offsets to hex rows.
        let hex_bytes = if self.hex_panel {
            hex_bytes_per_row(panel_width, &self.hex_metrics(cx))
        } else {
            1
        };
        let hex_viewport_height = (viewport_height - 2.0 * STATUS_BAR_HEIGHT).max(1.0);
        if self.hex_panel {
            let key = (self.document.revision(), self.hex_encoding, hex_bytes);
            if self.hex_index_key != Some(key) {
                self.hex_index =
                    HexIndex::build(self.document.text(), self.hex_encoding, hex_bytes).ok();
                self.hex_index_key = Some(key);
            }
        } else {
            // Free the index while the panel is closed.
            self.hex_index = None;
            self.hex_index_key = None;
        }

        // Opening the pane aligns it to the editor's first visible line; after
        // that, scrolling either pane moves the other. Runs before layout so the
        // offsets painted this frame are the ones chosen here.
        if std::mem::take(&mut self.pending_hex_snap) {
            if let Some(top) = self.editor_top_offset(viewport_height, cx) {
                self.set_hex_scroll_for_text_offset(top, window);
            }
            self.last_editor_scroll_y = f32::from(self.scroll_handle.offset().y);
            self.last_hex_scroll_y = f32::from(self.hex_scroll.offset().y);
        } else {
            self.sync_scroll_offsets(window, cx);
        }

        // Only the rows in (and just around) the viewport are materialized. The
        // wrap index is cached against the document revision and wrap settings,
        // so per-frame work is O(visible), not O(document).
        let layout = self.wrap_layout(cx);
        let total_rows = layout.total_rows(&self.document);
        let (first_visible, last_visible) = self.visible_window(&layout, viewport_height);
        let total_height = total_rows as f32 * line_height + 2.0 * PAD;
        let top_spacer = first_visible as f32 * line_height;
        let bottom_spacer =
            total_rows.saturating_sub(last_visible).saturating_sub(1) as f32 * line_height;
        let visible_row_ranges = self.visible_row_ranges(&layout, first_visible, last_visible, cx);
        drop(layout);
        // The vertical scrollbar's drag math reads the painted extent back.
        self.content_height.set(total_height);

        // Shape each visible row as a single text element (see `row_element` for
        // why a single `StyledText` keeps the overlay geometry exact). Only the
        // viewport's rows are built.
        let rows: Vec<AnyElement> = visible_row_ranges
            .iter()
            .map(|&(_, _, start, end)| self.row_element(start, end))
            .collect();

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

        // gpui draws no scrollbars, so we render (and drag) a vertical one too.
        // It tracks scroll extent for both wrapped and unwrapped modes.
        let v_scrollbar = {
            let editor_height = (viewport_height - STATUS_BAR_HEIGHT).max(1.0);
            let overflow = (total_height - editor_height).max(0.0);
            if overflow > 0.0 && total_height.is_finite() {
                let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
                let min_thumb = MIN_THUMB_WIDTH.min(track_height);
                let thumb_height =
                    (track_height * editor_height / total_height).clamp(min_thumb, track_height);
                let travel = (track_height - thumb_height).max(0.0);
                let offset_y = (-f32::from(self.scroll_handle.offset().y)).clamp(0.0, overflow);
                let thumb_top = SCROLLBAR_MARGIN + (offset_y / overflow) * travel;
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
                                        this.begin_v_scroll_drag(event.position, true, window, cx);
                                    }),
                                )
                                .child(div().w(px(6.0)).h(px(thumb_top)))
                                .child(
                                    div()
                                        .w(px(6.0))
                                        .h(px(thumb_height))
                                        .bg(rgb(0x45475a))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |this, event: &MouseDownEvent, window, cx| {
                                                    this.begin_v_scroll_drag(
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

        // The content carries the full scroll extent; only the visible rows sit
        // inside it, offset by spacers so scrolling stays pixel-exact.
        let content = div().relative().h(px(total_height));
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
            for (position, transform_index) in matches.iter().enumerate() {
                let transform = &transforms::TRANSFORMS[*transform_index];
                let selected = position == selected_index;
                let available = transforms::is_available(transform, has_selection);
                let name_color = if !available {
                    rgb(0x585b70)
                } else if selected {
                    rgb(0xcdd6f4)
                } else {
                    rgb(0xa6adc8)
                };
                let description = transform.description;
                let info_icon = div()
                    .child("info")
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .px(px(6.0))
                    .py(px(2.0))
                    .id(("transform-info", *transform_index))
                    // Clicking the label must not also apply the transform.
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
                    .child(transform.name)
                    .child(info_icon);
                let item = if selected {
                    item.bg(rgb(0x313244))
                } else {
                    item
                };
                let item = item
                    .id(("transform-row", *transform_index))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        if let Some(palette) = &mut this.palette {
                            palette.selected = position;
                        }
                        this.apply_selected_transform(window, cx);
                    }));
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
                    .child(self.render_hex_pane(panel_width, hex_bytes, hex_viewport_height, cx)),
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
                                .child(div().h(px(top_spacer)))
                                .children(rows)
                                .child(div().h(px(bottom_spacer))),
                        )
                        .child(overlay),
                ),
            )
            .children(h_scrollbar)
            .children(v_scrollbar);

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
                } else if this.hex_v_drag.is_some() {
                    if event.dragging() {
                        this.update_hex_v_drag(f32::from(event.position.y), window, cx);
                    } else {
                        this.hex_v_drag = None;
                    }
                } else if this.hex_dragging {
                    this.on_hex_mouse_move(event, window, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    this.hex_drag = None;
                    this.hex_v_drag = None;
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
            .children(pending_overlay)
    }
}
