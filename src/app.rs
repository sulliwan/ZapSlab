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
    Document, LineEnding, MAX_DOCUMENT_BYTES, byte_offset_for_utf16, display,
    display::DisplayOptions,
    encoding::{self, Encoding},
    hexdump::HexIndex,
    utf16_offset,
};
use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, ElementInputHandler,
    EntityInputHandler,
    FocusHandle, FontId, HighlightStyle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render,
    ScrollDelta, ScrollEvent, ScrollHandle, StatefulInteractiveElement, Styled, StyledText,
    Subscription, Task, TextRun, UTF16Selection,
    Window, canvas, div, fill, font, point, px, rgb, rgba, size, svg,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{commands, find, storage, transforms};

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
/// Recent find queries retained for Up/Down recall in the find dialog.
const MAX_RECENT_FINDS: usize = 20;
/// Font size and left/base padding of the find/replace text fields. Mouse
/// hit-testing shapes the field text at the same size so clicks map correctly.
const FIND_FIELD_TEXT_SIZE: f32 = 14.0;
const FIND_FIELD_PADDING: f32 = 6.0;
/// Minimum height of a find/replace field so an empty field does not collapse
/// and the two boxes stay the same size.
const FIND_FIELD_MIN_HEIGHT: f32 = 24.0;
/// Width of the "Find"/"Replace" labels and of the Find history arrow.
const FIND_LABEL_WIDTH: f32 = 56.0;
const FIND_HISTORY_ARROW_WIDTH: f32 = 24.0;
/// Right inset of the Find field: normal padding plus the chevron hit zone, so
/// the text never runs underneath the history affordance.
const FIND_FIELD_RIGHT_PADDING: f32 = FIND_FIELD_PADDING + FIND_HISTORY_ARROW_WIDTH;
/// Pixels per line for line-based wheel deltas while panning a find field.
const FIND_SCROLL_LINE_PX: f32 = 20.0;
/// Width of the ellipsis patch drawn at a field edge when text is scrolled
/// out of view on that side.
const FIND_OVERFLOW_PATCH_WIDTH: f32 = 16.0;
/// A font-independent down chevron, so the history affordance never renders as
/// a missing-glyph box (the embedded UI fonts do not cover U+25BE).
const CHEVRON_DOWN_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24"><path d="M6 9l6 6 6-6z" fill="#a6adc8"/></svg>"##;
/// How long a transient status-bar hint stays visible.
const HINT_DURATION: Duration = Duration::from_millis(3600);
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
    /// Escape/line-ending settings the rows were measured with. Widths change
    /// when either toggle or the style changes, so the index must rebuild.
    options: DisplayOptions,
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
            && self.options == app.display_options()
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

/// Which field of the find/replace dialog receives typed input.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FindField {
    Find,
    Replace,
}

/// Floating find/replace dialog state.
///
/// `matcher` and `current` are derived from `find`/`regex`/`case_insensitive`
/// and rebuilt by `refresh_find` on every query change, never per frame.
/// `error` holds a regex compile failure so the dialog can show it inline.
struct FindState {
    find: String,
    replace: String,
    /// Byte-offset caret within `find`, always a char boundary.
    find_caret: usize,
    /// Byte-offset caret within `replace`, always a char boundary.
    replace_caret: usize,
    /// Horizontal pan offset (pixels of text hidden on the left) for each
    /// field. Kept per field so switching focus does not lose the view.
    find_scroll: f32,
    replace_scroll: f32,
    /// Selection anchor within `find`; equals `find_caret` when nothing is
    /// selected.
    find_anchor: usize,
    /// Selection anchor within `replace`; equals `replace_caret` when nothing is
    /// selected.
    replace_anchor: usize,
    field: FindField,
    regex: bool,
    case_insensitive: bool,
    matcher: Option<find::Matcher>,
    error: Option<String>,
    current: Option<(usize, usize)>,
    wrapped: bool,
    /// Index into the view's recent-finds list while browsing it with Up/Down;
    /// `None` when editing the live query.
    history_index: Option<usize>,
    /// The live query stashed when history browsing starts, restored by Down.
    history_draft: Option<String>,
    /// Field currently being dragged with the mouse, if any.
    field_drag: Option<FindField>,
    /// Whether the recent-finds dropdown is open below the Find field.
    history_open: bool,
}

impl FindState {
    fn active(&self) -> &String {
        match self.field {
            FindField::Find => &self.find,
            FindField::Replace => &self.replace,
        }
    }

    fn active_mut(&mut self) -> &mut String {
        match self.field {
            FindField::Find => &mut self.find,
            FindField::Replace => &mut self.replace,
        }
    }

    fn caret(&self) -> usize {
        match self.field {
            FindField::Find => self.find_caret,
            FindField::Replace => self.replace_caret,
        }
    }

    fn set_caret(&mut self, caret: usize) {
        match self.field {
            FindField::Find => self.find_caret = caret,
            FindField::Replace => self.replace_caret = caret,
        }
    }

    fn anchor(&self) -> usize {
        match self.field {
            FindField::Find => self.find_anchor,
            FindField::Replace => self.replace_anchor,
        }
    }

    fn set_anchor(&mut self, anchor: usize) {
        match self.field {
            FindField::Find => self.find_anchor = anchor,
            FindField::Replace => self.replace_anchor = anchor,
        }
    }

    /// Ordered selection in the active field, clamped to its length.
    fn selection(&self) -> (usize, usize) {
        let len = self.active().len();
        let caret = self.caret().min(len);
        let anchor = self.anchor().min(len);
        if caret <= anchor {
            (caret, anchor)
        } else {
            (anchor, caret)
        }
    }

    fn reset_history(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }
}

/// One row in the transform palette. Commands (which open UI) and transforms
/// (which rewrite text) share one list so the palette stays a single search.
#[derive(Clone, Copy)]
enum PaletteEntry {
    Transform(usize),
    Command(usize),
}

fn palette_entry_available(entry: PaletteEntry, has_selection: bool) -> bool {
    match entry {
        PaletteEntry::Transform(index) => {
            transforms::is_available(&transforms::TRANSFORMS[index], has_selection)
        }
        PaletteEntry::Command(_) => true,
    }
}

/// Snap `at` into `text` and down to a char boundary.
fn snap_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Forces pasted/typed input to a single line so the field stays one row and
/// the text shaper (which asserts on newlines) is safe.
fn field_input(text: &str) -> String {
    if text.contains('\n') || text.contains('\r') {
        text.replace('\n', " ").replace('\r', " ")
    } else {
        text.to_string()
    }
}

fn previous_grapheme_start(text: &str, caret: usize) -> usize {
    text[..caret]
        .grapheme_indices(true)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_grapheme_end(text: &str, caret: usize) -> usize {
    text[caret..]
        .grapheme_indices(true)
        .next()
        .map(|(_, grapheme)| caret + grapheme.len())
        .unwrap_or(text.len())
}

fn find_field_run(text: &str) -> TextRun {
    TextRun {
        len: text.len(),
        font: font(".SystemUIFont"),
        color: rgb(0xf5e0dc).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Byte index in `text` nearest to the `x` offset within its laid-out line.
/// Shapes the line at the field's font and size so this works for the
/// proportional UI font, not just monospace.
fn closest_index_for_x(text: &str, x: Pixels, window: &Window) -> usize {
    let layout = window
        .text_system()
        .layout_line(text, px(FIND_FIELD_TEXT_SIZE), &[find_field_run(text)], None);
    let mut index = layout.closest_index_for_x(x);
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Right inset of a field: the Find field reserves room for the chevron.
fn find_field_right_padding(field: FindField) -> f32 {
    match field {
        FindField::Find => FIND_FIELD_RIGHT_PADDING,
        FindField::Replace => FIND_FIELD_PADDING,
    }
}

/// How much of a field's text is scrolled out of view, as `(left, right)`.
/// Used to draw the edge ellipses that indicate there is more text.
fn find_field_overflow(
    text: &str,
    scroll: f32,
    bounds: Option<Bounds<Pixels>>,
    right_padding: f32,
    window: &Window,
) -> (bool, bool) {
    let Some(bounds) = bounds else {
        return (false, false);
    };
    let visible_width =
        (f32::from(bounds.size.width) - FIND_FIELD_PADDING - right_padding).max(0.0);
    if visible_width <= 0.0 {
        return (false, false);
    }
    let layout = window
        .text_system()
        .layout_line(text, px(FIND_FIELD_TEXT_SIZE), &[find_field_run(text)], None);
    let text_width = f32::from(layout.width);
    (scroll > 0.5, text_width - scroll > visible_width + 0.5)
}

/// Paints a find/replace field's selection and caret over its single, stable
/// text element. The text is never reflowed to make room for the caret, so
/// moving the caret only moves the overlay and leaves the letters put. Both
/// the selection and the caret are clamped to the field's visible width so a
/// long, horizontally-panned string never paints outside the box.
fn paint_find_field_overlay(
    bounds: Bounds<Pixels>,
    text: &str,
    caret: usize,
    anchor: usize,
    scroll: f32,
    right_padding: f32,
    active: bool,
    window: &mut Window,
) {
    if !active {
        return;
    }
    let layout = window
        .text_system()
        .layout_line(text, px(FIND_FIELD_TEXT_SIZE), &[find_field_run(text)], None);
    let caret = snap_boundary(text, caret);
    let anchor = snap_boundary(text, anchor);
    let (sel_start, sel_end) = if caret <= anchor {
        (caret, anchor)
    } else {
        (anchor, caret)
    };

    let origin_x = f32::from(bounds.origin.x) + FIND_FIELD_PADDING;
    let visible_width =
        (f32::from(bounds.size.width) - FIND_FIELD_PADDING - right_padding).max(0.0);
    // `layout_line` still reports the font's ascent/descent for an empty line,
    // but guard against a zero-height layout so the caret never vanishes.
    let measured_height = f32::from(layout.ascent) + f32::from(layout.descent);
    let line_height = if measured_height > 0.0 {
        measured_height
    } else {
        FIND_FIELD_TEXT_SIZE + 2.0
    };
    let y = f32::from(bounds.origin.y) + (f32::from(bounds.size.height) - line_height) * 0.5;

    if sel_start != sel_end {
        let clamp = |x: f32| (x - scroll).clamp(0.0, visible_width);
        let x0 = clamp(f32::from(layout.x_for_index(sel_start)));
        let x1 = clamp(f32::from(layout.x_for_index(sel_end)));
        if x1 > x0 {
            let selection = Bounds {
                origin: point(px(origin_x + x0), px(y)),
                size: size(px(x1 - x0), px(line_height)),
            };
            window.paint_quad(fill(selection, rgba(0x45475a66)));
        }
    }

    let caret_x = (f32::from(layout.x_for_index(caret)) - scroll)
        .clamp(0.0, (visible_width - CARET_WIDTH).max(0.0));
    let caret_bar = Bounds {
        origin: point(px(origin_x + caret_x), px(y)),
        size: size(px(CARET_WIDTH), px(line_height)),
    };
    window.paint_quad(fill(caret_bar, rgb(0xf5e0dc)));
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

/// Modal state for converting and displaying line endings.
#[derive(Clone, Copy)]
struct LineEndingPrompt {
    /// Target style highlighted in the dialog; `Mixed` is never selectable.
    selected: LineEnding,
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
    /// Floating find/replace dialog; `None` when closed.
    find: Option<FindState>,
    /// Set when an expensive transform needs confirmation before running.
    pending_transform: Option<PendingTransform>,
    /// Scroll position of the transform list. Reset when the palette opens or
    /// the query changes; driven by the wheel/trackpad and arrow navigation.
    palette_scroll: ScrollHandle,
    /// Recently applied transform names, newest first. Persisted in
    /// localStorage and used to float them to the top of the empty palette.
    recent_transforms: Vec<String>,
    /// Recent find queries, newest first, capped at `MAX_RECENT_FINDS`.
    recent_finds: Vec<String>,
    /// Last closed find/replace dialog state, restored when it reopens.
    last_find: Option<storage::SavedFindState>,
    /// Bounds of the find/replace fields, recorded in paint for mouse
    /// hit-testing (the dialog's text is not a real text input).
    find_query_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    find_replace_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Active find field's `(field, caret, text length)` from the last frame
    /// that auto-scrolled it. Used to scroll the caret into view only when the
    /// caret or text actually changed, so a manual pan is not snapped back.
    find_scroll_key: Option<(FindField, usize, usize)>,
    /// Render non-printing/invisible characters as colored escape sequences.
    /// On by default: the buffer is often inspected as near-binary data.
    show_invisibles: bool,
    /// Render each line ending as its style (`\n`, `\r\n`, `\r`) even when
    /// invisibles are off. Toggled from the status bar and the line-ending
    /// dialog.
    show_line_endings: bool,
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
    /// Device-pixel-snapped hex glyph advance, refreshed each render so the
    /// per-cell widths and gaps (and therefore hit-testing) land on the same
    /// pixel grid as the painted flex row.
    hex_ascii_width: Cell<f32>,
    /// Modal for choosing an encoding and a conversion direction.
    encoding_prompt: Option<EncodingPrompt>,
    /// Modal for converting line endings / toggling their display.
    line_ending_prompt: Option<LineEndingPrompt>,
    /// Inline editor for a single hex byte, opened by double-clicking it.
    byte_edit: Option<ByteEdit>,
    /// Modal for jumping to a 1-based line number, opened from the status bar.
    jump_prompt: Option<String>,
    /// Cached "L: x/y, C: z" label, keyed by `(document revision, cursor)`, so
    /// the status bar does not rescan the current line on every caret blink.
    cursor_status_cache: Option<(u64, usize, String)>,
    /// Cached "S: n,W: m" selection label, keyed by
    /// `(revision, selection start, selection end)`. A selection can span the
    /// whole document, so counting it on every frame (caret blink) would be a
    /// repeated full scan; selection moves and edits both invalidate the key.
    selection_status_cache: Option<(u64, usize, usize, String)>,
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
        let hex_ascii_width = text_system
            .ch_advance(font_id, px(HEX_FONT_SIZE))
            .unwrap_or(px(FALLBACK_ASCII_WIDTH_PX * HEX_FONT_SIZE / FONT_SIZE));

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
            find: None,
            pending_transform: None,
            palette_scroll: ScrollHandle::default(),
            recent_transforms: storage::load_recent(),
            recent_finds: storage::load_recent_finds(),
            last_find: storage::load_find_state(),
            find_query_bounds: Rc::new(Cell::new(None)),
            find_replace_bounds: Rc::new(Cell::new(None)),
            find_scroll_key: None,
            show_invisibles: true,
            show_line_endings: false,
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
            hex_ascii_width: Cell::new(f32::from(hex_ascii_width)),
            encoding_prompt: None,
            line_ending_prompt: None,
            byte_edit: None,
            jump_prompt: None,
            cursor_status_cache: None,
            selection_status_cache: None,
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

    /// Raw (unsnapped) advance of the hex font, read from the text system.
    fn hex_ascii_advance(&self, cx: &App) -> f32 {
        f32::from(
            cx.text_system()
                .ch_advance(self.metrics.font_id, px(HEX_FONT_SIZE))
                .unwrap_or(px(FALLBACK_ASCII_WIDTH_PX * HEX_FONT_SIZE / FONT_SIZE)),
        )
    }

    /// Hex-panel geometry from the device-pixel-snapped metrics refreshed each
    /// render. Snapping the advance (not just the row height) keeps every cell
    /// origin on the pixel grid, so pointer hit-testing matches the painted
    /// cells instead of drifting by a fraction of a pixel per column.
    fn hex_metrics(&self) -> HexMetrics {
        HexMetrics::new(self.hex_ascii_width.get(), self.hex_line_height.get())
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
                options: self.display_options(),
                prefix: Vec::new(),
            };
        }

        let wrap_width = self.wrap_width.get();
        let text = self.document.text();
        let width_of = self.width_fn(cx);
        let options = self.display_options();
        let mut prefix = Vec::with_capacity(self.document.line_count() + 1);
        prefix.push(0u32);
        let mut total = 0u32;
        for line in 0..self.document.line_count() {
            let (start, end) = self.line_display_range(line);
            let count = display::wrap_count(text, start, end, wrap_width, options, &width_of) as u32;
            total = total.saturating_add(count);
            prefix.push(total);
        }

        WrapLayout {
            on: true,
            revision: self.document.revision(),
            wrap_width,
            options,
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
            let (start, end) = self.line_display_range(line);
            let base = layout.row_of_line(line);
            if layout.on {
                for (index, (chunk_start, chunk_end)) in display::wrap_line(
                    text,
                    start,
                    end,
                    layout.wrap_width,
                    layout.options,
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
        let (start, end) = self.line_display_range(line);
        if !layout.on {
            return (line, start, end);
        }
        let chunk = index.saturating_sub(layout.row_of_line(line)) as usize;
        display::wrap_line(
            self.document.text(),
            start,
            end,
            layout.wrap_width,
            layout.options,
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
        let (start, end) = self.line_display_range(line);
        let chunk = display::wrap_line(
            self.document.text(),
            start,
            end,
            layout.wrap_width,
            layout.options,
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

        for grapheme in display::display_graphemes(&text[start..end], self.display_options()) {
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
            let (start, end) = self.line_display_range(line);
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
            let (line_start, line_end) = self.line_display_range(line);
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
            self.display_options(),
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
            self.display_options(),
            &width_of,
        )
    }

    /// The current escape/line-ending display settings.
    fn display_options(&self) -> DisplayOptions {
        DisplayOptions::new(
            self.show_invisibles,
            self.show_line_endings,
            self.document.line_ending(),
        )
    }

    /// The byte range a line is measured and rendered over. Normally this
    /// excludes the trailing newline (the line break is implicit); when line
    /// endings are shown, the newline is included so its escape is measured,
    /// wrapped, and drawn as part of the line.
    fn line_display_range(&self, line: usize) -> (usize, usize) {
        let (start, end) = self.document.line_range(line);
        if self.show_line_endings && end < self.document.len() {
            // Every line but the last ends with `\n`; `end` points at it.
            (start, end + 1)
        } else {
            (start, end)
        }
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
        let (line, start, end) = self.row_at(&layout, cx, row);
        let offset = self.offset_for_x_in_row(start, end, px(relative_x), cx);
        // A click on a drawn line-ending marker maps to the line's content end,
        // not to the start of the next line.
        let (_, content_end) = self.document.line_range(line);
        self.document.clamp_boundary(offset.min(content_end))
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
            // Materialize the line-ending style so a CRLF document is persisted
            // as CRLF.
            let Ok(text) = this.update(&mut *cx, |view, _cx| {
                view.document.text_with_line_endings().into_owned()
            }) else {
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
        let text = self.document.text_with_line_endings().into_owned();
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
            let (target_line, start, end) = self.row_at(&layout, cx, target);
            let offset = self.offset_for_x_in_row(start, end, x, cx);
            // Clamp to the target line's content end so vertical movement never
            // lands past a drawn line-ending marker.
            let (_, content_end) = self.document.line_range(target_line);
            self.document.clamp_boundary(offset.min(content_end))
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
    /// Copies use the document's line-ending style, so copying out of a CRLF
    /// document yields CRLF bytes even though the buffer is `\n`-only.
    fn write_selection_to_clipboard(&self, cx: &mut Context<Self>) {
        let (start, end) = self.document.selection_range();
        if start != end {
            let selected = &self.document.text()[start..end];
            let text = match self.document.line_ending() {
                LineEnding::Lf | LineEnding::Mixed => selected.to_string(),
                LineEnding::Crlf => selected.replace('\n', "\r\n"),
                LineEnding::Cr => selected.replace('\n', "\r"),
            };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
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

        // The line-ending dialog is modal: swallow typing so it never edits the
        // document behind the overlay; Escape cancels.
        if self.line_ending_prompt.is_some() {
            if keystroke.key.as_str() == "escape" {
                self.line_ending_prompt = None;
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

        // The jump-to-line prompt is modal: digits build the line number, Enter
        // jumps, and Escape cancels.
        if self.jump_prompt.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.jump_prompt = None;
                    window.notify_text_input_changed(&self.focus_handle);
                    cx.notify();
                }
                "enter" => self.apply_jump_prompt(window, cx),
                "backspace" => {
                    if let Some(value) = &mut self.jump_prompt {
                        value.pop();
                    }
                    cx.notify();
                }
                "left" | "right" | "home" | "end" | "tab" | "delete" => {}
                _ => {
                    // On web, printable keys fall through to the input handler.
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(text) = &keystroke.key_char {
                        self.push_jump_digits(text);
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

        if self.find.is_some() {
            return self.handle_find_key_down(event, window, cx);
        }

        if self.palette.is_some() {
            return self.handle_palette_key_down(event, window, cx);
        }

        if modifiers.platform || modifiers.control {
            match keystroke.key.as_str() {
                "a" => self.select_all(cx),
                "c" => self.copy(cx),
                "x" => self.cut(window, cx),
                // Cmd/Ctrl+K opens the transform palette. Cmd/Ctrl+Shift+P is
                // the usual editor convention but Firefox reserves it for
                // "New Private Window", so it can't be used in a browser tab.
                // Chrome gives Cmd/Ctrl+K to its address bar in a tab, but the
                // standalone PWA has none; when we handle the key, `on_key_down`
                // stops propagation, which makes gpui call `preventDefault`.
                // Ctrl+Shift+Space is kept as a fallback where K is taken.
                "k" => self.open_palette(window, cx),
                "space" if modifiers.shift => self.open_palette(window, cx),
                // Ctrl/Cmd+F toggles find/replace. Ctrl/Cmd+Shift+F is a fallback
                // where the browser reserves Ctrl+F; shift does not change `key`,
                // so the same arm covers both.
                "f" => self.open_find(window, cx),
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

        if (modifiers.platform || modifiers.control)
            && (keystroke.key == "k" || (modifiers.shift && keystroke.key == "space"))
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
            "enter" => self.apply_selected_palette_entry(window, cx),
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

    // ---- Find / replace -------------------------------------------------

    /// Ctrl/Cmd+F toggles the floating find/replace dialog. A current
    /// single-line selection becomes the find query (fully selected) so typing
    /// replaces it; otherwise the last closed dialog state is restored. It jumps
    /// to the first match at or after the selection start.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    (find, replace, regex, case_insensitive, FindField::Find, true)
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
    fn open_find_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn record_find(&mut self) {
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
    fn find_history_prev(&mut self, cx: &mut Context<Self>) {
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
    fn find_history_next(&mut self, cx: &mut Context<Self>) {
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
    fn toggle_find_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn select_find_history(&mut self, index: usize, cx: &mut Context<Self>) {
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

    fn handle_find_key_down(
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

    fn toggle_find_field(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            state.field = match state.field {
                FindField::Find => FindField::Replace,
                FindField::Replace => FindField::Find,
            };
            cx.notify();
        }
    }

    fn toggle_find_regex(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            return;
        };
        state.regex = !state.regex;
        self.after_find_query_changed(cx);
    }

    fn toggle_find_case_insensitive(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            return;
        };
        state.case_insensitive = !state.case_insensitive;
        self.after_find_query_changed(cx);
    }

    // Native printable keys are handled here; on web they fall through to the
    // input handler instead, so this is only referenced by the native path.
    #[cfg(not(target_arch = "wasm32"))]
    fn push_find_text(&mut self, text: &str, cx: &mut Context<Self>) {
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
    fn backspace_find(&mut self, cx: &mut Context<Self>) {
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
                if start < end { Some((start, end)) } else { None },
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
    fn delete_forward_find(&mut self, cx: &mut Context<Self>) {
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
                if start < end { Some((start, end)) } else { None },
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
    fn move_find_caret(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
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

    fn move_find_caret_home(&mut self, extend: bool, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            state.set_caret(0);
            if !extend {
                state.set_anchor(0);
            }
            cx.notify();
        }
    }

    fn move_find_caret_end(&mut self, extend: bool, cx: &mut Context<Self>) {
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
    fn select_all_find(&mut self, cx: &mut Context<Self>) {
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
    fn ensure_find_caret_visible(&mut self, window: &Window) -> bool {
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
            let layout = window
                .text_system()
                .layout_line(text, px(FIND_FIELD_TEXT_SIZE), &[find_field_run(text)], None);
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
    fn pan_find_scroll(
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
            let layout = window
                .text_system()
                .layout_line(text, px(FIND_FIELD_TEXT_SIZE), &[find_field_run(text)], None);
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
    fn find_index_for_point(
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
    fn begin_find_drag(
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
    fn update_find_drag(
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
    fn after_find_query_changed(&mut self, cx: &mut Context<Self>) {
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
    fn refresh_find(&mut self, from: usize) {
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

    fn find_next_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn find_prev_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn select_match(
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
    fn reveal_offset(&self, offset: usize, window: &Window, cx: &App) {
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

    fn replace_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn replace_all_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            find::replace_all(self.document.text(), matcher, &state.replace, MAX_DOCUMENT_BYTES)
        };

        let Some(new_text) = outcome else {
            self.show_hint("Document limit reached (64 MB).", cx);
            return;
        };
        if new_text.as_str() == self.document.text() {
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

    /// Opens the modal for jumping to a 1-based line number.
    fn open_jump_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_prompt = Some(String::new());
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn push_jump_digits(&mut self, text: &str) {
        if let Some(value) = &mut self.jump_prompt {
            for ch in text.chars() {
                if ch.is_ascii_digit() && value.len() < 9 {
                    value.push(ch);
                }
            }
        }
    }

    /// Clamps the typed line number into the document and moves the caret to
    /// the start of that line.
    fn apply_jump_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = self.jump_prompt.take() else {
            return;
        };
        window.notify_text_input_changed(&self.focus_handle);
        let Ok(line_number) = value.parse::<usize>() else {
            cx.notify();
            return;
        };
        if line_number == 0 {
            cx.notify();
            return;
        }
        let line = (line_number - 1).min(self.document.line_count().saturating_sub(1));
        let offset = self.document.line_range(line).0;
        self.document.set_cursor(offset, false);
        self.caret_visible = true;
        self.set_editor_scroll_for_text_offset(offset, cx, window);
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    /// "L: x/y, C: z" for the status bar. Column counts grapheme clusters so the
    /// number matches the caret's character position.
    fn cursor_status_label(&self) -> String {
        let cursor = self.document.cursor();
        let line = self.document.line_of_offset(cursor);
        let line_count = self.document.line_count();
        let line_start = self.document.line_range(line).0.min(cursor);
        let column = self.document.text()[line_start..cursor]
            .graphemes(true)
            .count()
            + 1;
        format!("L: {}/{}, C: {}", line + 1, line_count, column)
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
    fn palette_matches(&self) -> Vec<PaletteEntry> {
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
        let mut recency: HashMap<&str, usize> = HashMap::new();
        for (position, name) in self.recent_transforms.iter().enumerate() {
            recency.entry(name.as_str()).or_insert(position);
        }

        let mut scored: Vec<(u32, usize, usize, PaletteEntry)> = Vec::new();
        for (index, transform) in transforms::TRANSFORMS.iter().enumerate() {
            if let Some(score) = transforms::match_score_parts(
                transform.name,
                transform.tags,
                transform.description,
                &query,
            ) {
                let rank = recency.get(transform.name).copied().unwrap_or(usize::MAX);
                scored.push((score, rank, index, PaletteEntry::Transform(index)));
            }
        }
        for (index, command) in commands::COMMANDS.iter().enumerate() {
            if let Some(score) = transforms::match_score_parts(
                command.name,
                command.tags,
                command.description,
                &query,
            ) {
                scored.push((score, usize::MAX, index, PaletteEntry::Command(index)));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        scored.into_iter().map(|(_, _, _, entry)| entry).collect()
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

    fn apply_selected_palette_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    // ---- Line endings ---------------------------------------------------

    /// Opens the line-ending dialog, preselecting the current style. A `Mixed`
    /// document preselects LF, since `Mixed` is not a conversion target.
    fn open_line_ending_prompt(&mut self, cx: &mut Context<Self>) {
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
    fn apply_line_ending(&mut self, target: LineEnding, cx: &mut Context<Self>) {
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

    fn on_hex_mouse_down(
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
    fn hex_byte_index_for_point(
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
        let Some(offset) = self.hex_offset_for_point(event.position, bounds) else {
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

    fn render_hex_pane(
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
        if let Some(state) = &self.find {
            let text = state.active().as_str();
            let start = byte_offset_for_utf16(text, range_utf16.start);
            let end = byte_offset_for_utf16(text, range_utf16.end);
            *adjusted_range = Some(utf16_offset(text, start)..utf16_offset(text, end));
            return Some(text[start..end].to_string());
        }
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
        if let Some(state) = &self.find {
            let text = state.active();
            let caret = snap_boundary(text, state.caret());
            let anchor = snap_boundary(text, state.anchor());
            let (start, end) = if caret <= anchor {
                (caret, anchor)
            } else {
                (anchor, caret)
            };
            return Some(UTF16Selection {
                range: utf16_offset(text, start)..utf16_offset(text, end),
                reversed: caret < anchor,
            });
        }
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
        // The line-ending dialog is modal; ignore IME/paste input while it is up.
        if self.line_ending_prompt.is_some() {
            return;
        }
        // The transform confirmation is modal; ignore input while it is up.
        if self.pending_transform.is_some() {
            return;
        }
        // Route typing/paste to the jump-to-line prompt when it is open.
        if self.jump_prompt.is_some() {
            self.push_jump_digits(text);
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        // Route typing to the inline byte editor when it is open.
        if self.byte_edit.is_some() {
            self.push_byte_edit(text);
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        // Route typing/paste to the active find/replace field when it is open.
        if self.find.is_some() {
            let editing_find = {
                let Some(state) = &mut self.find else {
                    return;
                };
                let field_text = state.active();
                let (start, end) = match replacement_range {
                    Some(range) => (
                        byte_offset_for_utf16(field_text, range.start),
                        byte_offset_for_utf16(field_text, range.end),
                    ),
                    None => state.selection(),
                };
                let len = field_text.len();
                let start = start.min(len);
                let end = end.min(len).max(start);
                let insert = field_input(text);
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
            }
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
        if self.find.is_some() || self.palette.is_some() {
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
        if self.find.is_some() || self.palette.is_some() {
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
            self.show_hint("Press ctrl+k to see the magic", cx);
        }

        // gpui rounds each authored row height to the device-pixel grid during
        // layout. Snap the row pitch here and reuse it for the caret/selection
        // overlay and hit-testing; otherwise a fractional line height makes the
        // overlay drift downward by a fraction of a pixel per row.
        self.metrics.line_height = window.pixel_snap(self.metrics.line_height);
        self.hex_line_height
            .set(f32::from(window.pixel_snap(px(self.hex_line_height.get()))));
        // The hex row is a flex sequence of per-cell widths and margins. If the
        // advance stays fractional, gpui snaps each element independently and
        // the painted cell origin creeps away from `offset + column * step`
        // used by hit-testing; double-clicking a cell then misses its byte. Snap
        // the advance here so layout and hit-testing share one pixel grid.
        self.hex_ascii_width
            .set(f32::from(window.pixel_snap(px(self.hex_ascii_advance(cx)))));

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
            hex_bytes_per_row(panel_width, &self.hex_metrics())
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
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        if let Some(palette) = &mut this.palette {
                            palette.selected = position;
                        }
                        this.apply_selected_palette_entry(window, cx);
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

        // Scroll the active field's caret into view only when the caret or text
        // changed since the last frame that did so; a manual wheel pan must not
        // be snapped back by the caret blink.
        let scroll_key = self
            .find
            .as_ref()
            .map(|state| (state.field, state.caret(), state.active().len()));
        if let Some(key) = scroll_key {
            if self.find_scroll_key != Some(key) && self.ensure_find_caret_visible(window) {
                self.find_scroll_key = Some(key);
            }
        }

        let find_overlay = self.find.as_ref().map(|state| {
            let field = state.field;
            let find_active = field == FindField::Find;
            let replace_active = field == FindField::Replace;
            let find_text = state.find.clone();
            let find_caret = state.find_caret;
            let find_anchor = state.find_anchor;
            let replace_text = state.replace.clone();
            let replace_caret = state.replace_caret;
            let replace_anchor = state.replace_anchor;
            let history_open = state.history_open;
            let recent_finds = self.recent_finds.clone();

            let (status_text, status_color) = if let Some(error) = &state.error {
                (format!("Invalid regex: {error}"), rgb(0xf38ba8))
            } else if state.matcher.is_none() {
                (String::new(), rgb(0x6c7086))
            } else if state.current.is_none() {
                ("No matches".to_string(), rgb(0x6c7086))
            } else if state.wrapped {
                ("Wrapped".to_string(), rgb(0xf9e2af))
            } else {
                (String::new(), rgb(0x6c7086))
            };

            let has_matcher = state.matcher.is_some();

            let find_scroll = state.find_scroll;
            let replace_scroll = state.replace_scroll;
            let (find_more_left, find_more_right) = find_field_overflow(
                &find_text,
                find_scroll,
                self.find_query_bounds.get(),
                FIND_FIELD_RIGHT_PADDING,
                window,
            );
            let (replace_more_left, replace_more_right) = find_field_overflow(
                &replace_text,
                replace_scroll,
                self.find_replace_bounds.get(),
                FIND_FIELD_PADDING,
                window,
            );

            let ellipsis_patch = |left: bool, right_padding: f32| {
                let patch = div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(FIND_OVERFLOW_PATCH_WIDTH))
                    .bg(rgb(0x313244))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(FIND_FIELD_TEXT_SIZE))
                    .text_color(rgb(0x6c7086))
                    .child("…");
                if left {
                    patch.left(px(FIND_FIELD_PADDING))
                } else {
                    patch.right(px(right_padding))
                }
            };

            let history_chevron = div()
                .absolute()
                .top_0()
                .right_0()
                .w(px(FIND_HISTORY_ARROW_WIDTH))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .id("find-history-toggle")
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _event, _window, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.toggle_find_history(window, cx);
                }))
                .child(
                    svg()
                        .data(CHEVRON_DOWN_SVG.as_bytes())
                        .w(px(12.0))
                        .h(px(12.0))
                        .text_color(rgb(0xa6adc8)),
                );

            let find_bounds = self.find_query_bounds.clone();
            let find_field = div()
                .relative()
                .ml(px(FIND_FIELD_PADDING))
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .whitespace_nowrap()
                .min_h(px(FIND_FIELD_MIN_HEIGHT))
                .pl(px(FIND_FIELD_PADDING))
                .pr(px(FIND_FIELD_RIGHT_PADDING))
                .py(px(4.0))
                .bg(rgb(0x313244))
                .font(font(".SystemUIFont"))
                .text_size(px(FIND_FIELD_TEXT_SIZE))
                .text_color(rgb(0xf5e0dc))
                .id("find-query-field")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        this.begin_find_drag(FindField::Find, event.position, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                    this.pan_find_scroll(FindField::Find, event, window, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .ml(px(-find_scroll))
                                .whitespace_nowrap()
                                .child(find_text.clone()),
                        ),
                )
                .children(if find_more_left {
                    Some(ellipsis_patch(true, FIND_FIELD_RIGHT_PADDING))
                } else {
                    None
                })
                .children(if find_more_right {
                    Some(ellipsis_patch(false, FIND_FIELD_RIGHT_PADDING))
                } else {
                    None
                })
                .children(if recent_finds.is_empty() {
                    None
                } else {
                    Some(history_chevron)
                })
                .child(
                    canvas(
                        move |_bounds, _window, _cx| {},
                        move |bounds, _state, window, _cx| {
                            find_bounds.set(Some(bounds));
                            paint_find_field_overlay(
                                bounds,
                                &find_text,
                                find_caret,
                                find_anchor,
                                find_scroll,
                                FIND_FIELD_RIGHT_PADDING,
                                find_active,
                                window,
                            );
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );

            let replace_bounds = self.find_replace_bounds.clone();
            let replace_field = div()
                .relative()
                .ml(px(FIND_FIELD_PADDING))
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .whitespace_nowrap()
                .min_h(px(FIND_FIELD_MIN_HEIGHT))
                .px(px(FIND_FIELD_PADDING))
                .py(px(4.0))
                .bg(rgb(0x313244))
                .font(font(".SystemUIFont"))
                .text_size(px(FIND_FIELD_TEXT_SIZE))
                .text_color(rgb(0xf5e0dc))
                .id("find-replace-field")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        this.begin_find_drag(FindField::Replace, event.position, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                    this.pan_find_scroll(FindField::Replace, event, window, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .ml(px(-replace_scroll))
                                .whitespace_nowrap()
                                .child(replace_text.clone()),
                        ),
                )
                .children(if replace_more_left {
                    Some(ellipsis_patch(true, FIND_FIELD_PADDING))
                } else {
                    None
                })
                .children(if replace_more_right {
                    Some(ellipsis_patch(false, FIND_FIELD_PADDING))
                } else {
                    None
                })
                .child(
                    canvas(
                        move |_bounds, _window, _cx| {},
                        move |bounds, _state, window, _cx| {
                            replace_bounds.set(Some(bounds));
                            paint_find_field_overlay(
                                bounds,
                                &replace_text,
                                replace_caret,
                                replace_anchor,
                                replace_scroll,
                                FIND_FIELD_PADDING,
                                replace_active,
                                window,
                            );
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );

            let mut history_items = Vec::new();
            for (index, query) in recent_finds.iter().cloned().enumerate() {
                let item = div()
                    .w_full()
                    .mb(px(1.0))
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(13.0))
                    .text_color(rgb(0xf5e0dc))
                    .bg(rgb(0x313244))
                    .truncate()
                    .id(("find-history-item", index))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.select_find_history(index, cx);
                    }))
                    .child(query);
                history_items.push(item.into_any_element());
            }
            let history_list = div()
                .id("find-history-list")
                .mt(px(4.0))
                // Start under the find field: label + wrapper inset (the arrow
                // now sits to the right of the field).
                .ml(px(FIND_LABEL_WIDTH + FIND_FIELD_PADDING))
                .bg(rgb(0x181825))
                .flex()
                .flex_col()
                .max_h(px(240.0))
                .overflow_y_scroll()
                .children(history_items);

            let regex_toggle = div()
                .ml(px(6.0))
                .px(px(6.0))
                .py(px(2.0))
                .text_size(px(12.0))
                .text_color(if state.regex {
                    rgb(0x11111b)
                } else {
                    rgb(0xa6adc8)
                })
                .bg(if state.regex {
                    rgb(0x94e2d5)
                } else {
                    rgb(0x313244)
                })
                .id("find-regex-toggle")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.toggle_find_regex(cx);
                }))
                .child(".*");

            let case_toggle = div()
                .ml(px(6.0))
                .px(px(6.0))
                .py(px(2.0))
                .text_size(px(12.0))
                .text_color(if state.case_insensitive {
                    rgb(0x11111b)
                } else {
                    rgb(0xa6adc8)
                })
                .bg(if state.case_insensitive {
                    rgb(0x94e2d5)
                } else {
                    rgb(0x313244)
                })
                .id("find-case-toggle")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.toggle_find_case_insensitive(cx);
                }))
                .child("Aa");

            let action_color = if has_matcher {
                rgb(0xcdd6f4)
            } else {
                rgb(0x585b70)
            };

            let prev_button = div()
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .text_color(action_color)
                .bg(rgb(0x313244))
                .id("find-prev")
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.find_prev_match(window, cx);
                }))
                .child("Prev");

            let next_button = div()
                .ml(px(6.0))
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .text_color(action_color)
                .bg(rgb(0x313244))
                .id("find-next")
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.find_next_match(window, cx);
                }))
                .child("Next");

            let replace_button = div()
                .ml(px(6.0))
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .text_color(action_color)
                .bg(rgb(0x313244))
                .id("find-replace-one")
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.replace_current(window, cx);
                }))
                .child("Replace");

            let replace_all_button = div()
                .ml(px(6.0))
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .text_color(action_color)
                .bg(rgb(0x313244))
                .id("find-replace-all")
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.replace_all_find(window, cx);
                }))
                .child("Replace all");

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
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, window, cx| {
                        this.close_find(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .w(px(560.0))
                        .p(px(10.0))
                        .bg(rgb(0x1e1e2e))
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            |this, event: &MouseMoveEvent, window, cx| {
                                this.update_find_drag(event, window, cx);
                            },
                        ))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                                if let Some(state) = &mut this.find {
                                    state.field_drag = None;
                                }
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .w(px(FIND_LABEL_WIDTH))
                                        .text_color(rgb(0x6c7086))
                                        .child("Find"),
                                )
                                .child(find_field)
                                .child(regex_toggle)
                                .child(case_toggle),
                        )
                        .children(if history_open && !recent_finds.is_empty() {
                            Some(history_list)
                        } else {
                            None
                        })
                        .child(
                            div()
                                .mt(px(6.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .w(px(FIND_LABEL_WIDTH))
                                        .text_color(rgb(0x6c7086))
                                        .child("Replace"),
                                )
                                .child(replace_field),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(prev_button)
                                .child(next_button)
                                .child(replace_button)
                                .child(replace_all_button)
                                .child(
                                    div()
                                        .flex_1()
                                        .ml(px(8.0))
                                        .text_size(px(12.0))
                                        .text_color(status_color)
                                        .child(status_text),
                                ),
                        )
                        .children(if recent_finds.is_empty() {
                            None
                        } else {
                            Some(
                                div()
                                    .mt(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x6c7086))
                                    .child(format!(
                                        "Up/Down: {} recent find{}",
                                        recent_finds.len(),
                                        if recent_finds.len() == 1 { "" } else { "s" },
                                    )),
                            )
                        }),
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
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_color(rgb(0xcdd6f4))
                                .child(shown),
                        )
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
            .children(find_overlay)
            .children(encoding_overlay)
            .children(line_ending_overlay)
            .children(byte_edit_overlay)
            .children(jump_overlay)
            .children(pending_overlay)
    }
}
