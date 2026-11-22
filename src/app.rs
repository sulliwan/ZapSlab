//! The scratchpad editor.
//!
//! `Scratchpad` owns the gpui view for a single document. The pure text model
//! (text, cursor/anchor byte offsets, the incremental line index, bounded
//! inverse-edit undo, and UTF-16 mapping) lives in the `zapslab-document`
//! crate; this module holds the gpui state, geometry, input routing, and the
//! top-level `Render` layout.
//!
//! Rendering is virtualized: only the visible rows are built, positioned by
//! top/bottom spacers inside a content element with an explicit height. Caret
//! and selection are painted in a `canvas` overlay using glyph advances from
//! the text system, so geometry matches the rendered text without a full
//! text-layout pipeline.
//!
//! Input arrives two ways on web: printable characters are routed by the
//! platform through `EntityInputHandler::replace_text_in_range` (this is also
//! the paste and IME path), while editing commands (arrows, backspace, enter,
//! shortcuts) are handled in `on_key_down`.
//!
//! Cohesive method groups live in the `view` submodules (`find`, `hex`,
//! `mouse`, `palette`, `paint`, `scroll`). Each is an `impl Scratchpad` block;
//! because those modules are children of `app`, they can reach the view's
//! private fields and helpers.

use std::{
    cell::{Cell, Ref, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    time::Duration,
};

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, ElementInputHandler,
    EntityInputHandler, FocusHandle, FontId, HighlightStyle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, ScrollDelta, ScrollEvent, ScrollHandle, StatefulInteractiveElement, Styled,
    StyledText, Subscription, Task, TextRun, UTF16Selection, Window, canvas, div, fill, font,
    point, px, rgb, rgba, size, svg,
};
use unicode_segmentation::UnicodeSegmentation;
use zapslab_document::{
    Document, LineEnding, MAX_DOCUMENT_BYTES, byte_offset_for_utf16, display,
    display::DisplayOptions,
    encoding::{self, Encoding},
    hexdump::HexIndex,
    utf16_offset,
};

use crate::{commands, find, storage, transforms};

mod view;

use self::view::geom::ScrollbarGeometry;

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
pub(crate) enum FindField {
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

    /// Replace the active field's `start..end` with `insert`, clamping both ends
    /// into the field, snapping to char boundaries, collapsing the selection,
    /// and parking the caret after the insert. Resets history browsing when the
    /// Find query changes and reports which field was edited.
    fn replace_active(&mut self, start: usize, end: usize, insert: &str) -> FieldEdit {
        let len = self.active().len();
        let start = snap_boundary(self.active(), start.min(len));
        let end = snap_boundary(self.active(), end.min(len)).max(start);
        self.active_mut().replace_range(start..end, insert);
        let caret = start + insert.len();
        self.set_caret(caret);
        self.set_anchor(caret);
        if self.field == FindField::Find {
            self.reset_history();
            FieldEdit::Find
        } else {
            FieldEdit::Replace
        }
    }

    /// Delete the selection, or the grapheme before the caret.
    fn delete_backward(&mut self) -> FieldEdit {
        let (start, end) = {
            let text = self.active();
            let (sel_start, sel_end) = self.selection();
            if sel_start != sel_end {
                (sel_start, sel_end)
            } else {
                let caret = snap_boundary(text, self.caret());
                if caret == 0 {
                    return FieldEdit::Nothing;
                }
                (previous_grapheme_start(text, caret), caret)
            }
        };
        self.replace_active(start, end, "")
    }

    /// Delete the selection, or the grapheme after the caret.
    fn delete_forward(&mut self) -> FieldEdit {
        let (start, end) = {
            let text = self.active();
            let (sel_start, sel_end) = self.selection();
            if sel_start != sel_end {
                (sel_start, sel_end)
            } else {
                let caret = snap_boundary(text, self.caret());
                if caret >= text.len() {
                    return FieldEdit::Nothing;
                }
                (caret, next_grapheme_end(text, caret))
            }
        };
        self.replace_active(start, end, "")
    }
}

/// Which field an active find/replace field edit touched, so the caller knows
/// whether live matches must be recomputed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldEdit {
    /// Nothing changed (e.g. backspace at the start of the field).
    Nothing,
    /// The Find query changed.
    Find,
    /// Only the Replace field changed.
    Replace,
}

/// One row in the transform palette. Commands (which open UI) and transforms
/// (which rewrite text) share one list so the palette stays a single search.
#[derive(Clone, Copy)]
pub(crate) enum PaletteEntry {
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
    let layout = window.text_system().layout_line(
        text,
        px(FIND_FIELD_TEXT_SIZE),
        &[find_field_run(text)],
        None,
    );
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
    let layout = window.text_system().layout_line(
        text,
        px(FIND_FIELD_TEXT_SIZE),
        &[find_field_run(text)],
        None,
    );
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
    let layout = window.text_system().layout_line(
        text,
        px(FIND_FIELD_TEXT_SIZE),
        &[find_field_run(text)],
        None,
    );
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
            let count =
                display::wrap_count(text, start, end, wrap_width, options, &width_of) as u32;
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
            let insert = field_input(text);
            let outcome = {
                let Some(state) = &mut self.find else {
                    return;
                };
                let (start, end) = match replacement_range {
                    Some(range) => {
                        let field_text = state.active();
                        (
                            byte_offset_for_utf16(field_text, range.start),
                            byte_offset_for_utf16(field_text, range.end),
                        )
                    }
                    None => state.selection(),
                };
                state.replace_active(start, end, &insert)
            };
            self.apply_field_edit(outcome, cx);
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
        let (first_visible, last_visible) =
            self.visible_window(&layout, self.editor_height(window));
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
            let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
            let geometry = if self.word_wrap {
                None
            } else {
                ScrollbarGeometry::new(content_px, viewport_width, track_width)
            };
            if let Some(geometry) = geometry {
                let fraction =
                    geometry.fraction_from_offset(f32::from(self.scroll_handle.offset().x));
                let thumb_left = geometry.thumb_start(SCROLLBAR_MARGIN, fraction);
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
                                        .w(px(geometry.thumb))
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
            let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
            let geometry = ScrollbarGeometry::new(total_height, editor_height, track_height);
            if let Some(geometry) = geometry {
                let fraction =
                    geometry.fraction_from_offset(f32::from(self.scroll_handle.offset().y));
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
                                        this.begin_v_scroll_drag(event.position, true, window, cx);
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

        let status_bar = self.status_bar(cx);

        let palette_overlay = self.palette_overlay(viewport, cx);

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

        let find_overlay = self.find_overlay(window, cx);

        let encoding_overlay = self.encoding_overlay(cx);

        let line_ending_overlay = self.line_ending_overlay(cx);

        let byte_edit_overlay = self.byte_edit_overlay(cx);

        let jump_overlay = self.jump_overlay(cx);

        let pending_overlay = self.pending_overlay(cx);

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
