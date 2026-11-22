//! Pure document model for ZapSlab.
//!
//! Everything here is plain `std`: text, the byte-offset cursor/anchor, the
//! incremental line index, the bounded undo/redo history, and the UTF-16 ⇄ byte
//! offset conversion used at the platform input boundary. There are no `gpui`,
//! `wasm-bindgen`, or `web-sys` imports, so the whole model is unit-testable
//! offline with `cargo test`.
//!
//! Invariants:
//! - `text` never contains `\r`; CRLF/CR is normalized on the way in.
//! - `cursor` and `anchor` are always char boundaries into `text`.
//! - Every line but the last ends with `\n`; a trailing `\n` is a final empty
//!   line. The line index stores each line's byte length (including its
//!   newline) and keeps prefix sums incrementally.
//! - `utf16_len` always equals `text.encode_utf16().count()`.

pub mod display;
pub mod encoding;
pub mod escape;
pub mod grapheme;
pub mod hexdump;

mod line_index;

use std::{borrow::Cow, collections::VecDeque};

use crate::grapheme::{next_grapheme_boundary, previous_grapheme_boundary, snap_to_grapheme};
use crate::line_index::LineIndex;

/// Maximum number of undo steps retained. Older steps are evicted from the
/// front of the ring so the ends stay O(1).
pub const MAX_UNDO: usize = 200;

/// Hard cap on the document's UTF-8 byte length. Edits that would grow the
/// document past this are refused by [`Document::replace_range`]; the view
/// surfaces a message and refuses the input before it reaches the model.
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;

/// Cap on the bytes retained across undo steps (removed + inserted text). The
/// oldest steps are evicted first, but at least one is always kept so the most
/// recent edit stays undoable even if it alone exceeds the budget.
pub const MAX_UNDO_BYTES: usize = 128 * 1024 * 1024;

/// A document's line-ending style.
///
/// The buffer itself is always `\n`-only (a hard invariant); the style is kept
/// as metadata and only materialized at the persistence and clipboard
/// boundaries. `Mixed` records that the loaded text used more than one style;
/// it is never a conversion target; saving such a document writes LF bytes, and
/// the metadata stays `Mixed` until an explicit conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
    Mixed,
}

impl LineEnding {
    /// Styles the user can convert to. `Mixed` is excluded: it is a detected
    /// observation, not a target.
    pub const TARGETS: &'static [LineEnding] = &[LineEnding::Lf, LineEnding::Crlf, LineEnding::Cr];

    /// Short label for the status bar and dialog.
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
            LineEnding::Cr => "CR",
            LineEnding::Mixed => "Mixed",
        }
    }

    /// The escape notation rendered when line endings are made visible.
    pub fn display_escape(self) -> &'static str {
        match self {
            LineEnding::Lf | LineEnding::Mixed => "\\n",
            LineEnding::Crlf => "\\r\\n",
            LineEnding::Cr => "\\r",
        }
    }
}

/// One reversible edit: undoing replaces `start..start + inserted.len()` with
/// `removed`; redoing replaces `start..start + removed.len()` with `inserted`.
/// Its size tracks what the user actually changed, not the document size.
#[derive(Clone)]
struct Edit {
    start: usize,
    removed: String,
    inserted: String,
    /// Cursor/anchor to restore on undo. The post-edit caret is
    /// `start + inserted.len()` (undo/redo always leave a plain caret).
    before_cursor: usize,
    before_anchor: usize,
}

impl Edit {
    fn size(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

/// One entry in the undo/redo history: a text edit or a line-ending conversion.
///
/// Conversion is metadata-only (the buffer stays `\n`-only) but still occupies
/// a single undo step, so undo/redo stay uniform. A line-ending step costs no
/// retained text bytes.
enum HistoryStep {
    Text(Edit),
    LineEnding {
        before: LineEnding,
        after: LineEnding,
    },
}

impl HistoryStep {
    fn size(&self) -> usize {
        match self {
            HistoryStep::Text(edit) => edit.size(),
            HistoryStep::LineEnding { .. } => 0,
        }
    }
}

/// Tracks a run of contiguous single-character insertions so a typing burst
/// collapses into a single undo step.
#[derive(Clone, Copy)]
struct InsertRun {
    /// Byte offset just past the most recently inserted character.
    end: usize,
}

pub struct Document {
    text: String,
    cursor: usize,
    anchor: usize,
    lines: LineIndex,
    /// Total UTF-16 code units in `text`, maintained incrementally so the
    /// platform-boundary conversions can walk from the nearer end.
    utf16_len: usize,
    /// The document's line-ending style. `text` is always `\n`-only; this is
    /// materialized only at the persistence and clipboard boundaries.
    line_ending: LineEnding,
    undo: VecDeque<HistoryStep>,
    redo: VecDeque<HistoryStep>,
    /// Retained bytes in `undo`/`redo`, for the `MAX_UNDO_BYTES` budget.
    undo_bytes: usize,
    redo_bytes: usize,
    insert_run: Option<InsertRun>,
    /// Bumped whenever the text changes. The view uses it to invalidate cached
    /// layout that depends on the document's contents (e.g. soft wrapping).
    revision: u64,
}

impl Document {
    /// Builds a document from `text`, normalizing newlines and placing the
    /// cursor at the end.
    pub fn new(text: String) -> Self {
        // Detect before normalizing: the `\r` bytes are the only record of the
        // original style, and normalization removes them.
        let line_ending = detect_line_ending(&text);
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text
        };
        let cursor = text.len();
        let lines = LineIndex::new(&text);
        let utf16_len = text.encode_utf16().count();
        Self {
            text,
            cursor,
            anchor: cursor,
            lines,
            utf16_len,
            line_ending,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            undo_bytes: 0,
            redo_bytes: 0,
            insert_run: None,
            revision: 0,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Monotonic counter bumped on every text mutation (edits, undo, redo).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    // ---- Line endings ---------------------------------------------------

    /// The document's current line-ending style (metadata; `text` stays
    /// `\n`-only).
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Converts the whole document to `style`. This changes only the stored
    /// style — the buffer is already `\n`-only — so it is O(1) and cannot grow
    /// the document. `Mixed` is not a target and is rejected.
    ///
    /// Recorded as one undo step and clears redo, like any other edit.
    pub fn set_line_ending(&mut self, style: LineEnding) -> bool {
        if style == LineEnding::Mixed || style == self.line_ending {
            return false;
        }
        self.redo.clear();
        self.redo_bytes = 0;
        let before = self.line_ending;
        self.undo.push_back(HistoryStep::LineEnding {
            before,
            after: style,
        });
        self.evict_undo();
        self.line_ending = style;
        self.insert_run = None;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// The text with the document's line endings materialized, for persistence
    /// and clipboard export. `Lf` (and the `Mixed` fallback) borrow, so the
    /// common case copies nothing extra.
    pub fn text_with_line_endings(&self) -> Cow<'_, str> {
        match self.line_ending {
            LineEnding::Lf | LineEnding::Mixed => Cow::Borrowed(&self.text),
            LineEnding::Crlf => Cow::Owned(self.text.replace('\n', "\r\n")),
            LineEnding::Cr => Cow::Owned(self.text.replace('\n', "\r")),
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The selected byte range, ordered low-to-high regardless of direction.
    pub fn selection_range(&self) -> (usize, usize) {
        if self.cursor <= self.anchor {
            (self.cursor, self.anchor)
        } else {
            (self.anchor, self.cursor)
        }
    }

    // ---- Line model -----------------------------------------------------

    pub fn line_count(&self) -> usize {
        self.lines.line_count()
    }

    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.lines.line_of_offset(offset)
    }

    /// Byte range of a line, excluding its trailing newline.
    pub fn line_range(&self, line: usize) -> (usize, usize) {
        self.lines.line_range(line)
    }

    // ---- UTF-16 offsets -------------------------------------------------
    // The platform input handler works in UTF-16 code units while the model
    // works in byte offsets. Converting from the nearer end keeps the common
    // case (edits at the cursor) proportional to the distance, not the length.

    /// UTF-16 code-unit offset for `byte_offset`, snapping down to a char
    /// boundary first.
    pub fn utf16_offset(&self, byte_offset: usize) -> usize {
        let mut byte = byte_offset.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        if byte <= self.text.len() / 2 {
            self.text[..byte].encode_utf16().count()
        } else {
            self.utf16_len - self.text[byte..].encode_utf16().count()
        }
    }

    /// Byte offset for a UTF-16 code-unit offset. Mirrors
    /// [`byte_offset_for_utf16`] (an offset inside a surrogate pair snaps to the
    /// pair's start) but walks from the nearer end using the cached total.
    pub fn byte_offset_for_utf16(&self, utf16: usize) -> usize {
        if utf16 == 0 {
            return 0;
        }
        if utf16 >= self.utf16_len {
            return self.text.len();
        }
        if utf16 <= self.utf16_len / 2 {
            return byte_offset_for_utf16(&self.text, utf16);
        }
        let mut units_after = 0usize;
        for (index, ch) in self.text.char_indices().rev() {
            let units_before = self.utf16_len - units_after - ch.len_utf16();
            if units_before < utf16 {
                return if utf16 == self.utf16_len - units_after {
                    index + ch.len_utf8()
                } else {
                    index
                };
            }
            units_after += ch.len_utf16();
        }
        self.text.len()
    }

    // ---- Boundaries -----------------------------------------------------

    /// Clamps `offset` into the document and snaps it down to a char boundary.
    pub fn clamp_boundary(&self, mut offset: usize) -> usize {
        offset = offset.min(self.text.len());
        while offset > 0 && !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    // ---- Cursor ---------------------------------------------------------

    /// Moves the cursor, extending the selection when `extend` is set. The
    /// target snaps to a grapheme boundary so the caret never sits inside a
    /// combined character. Explicit movement breaks any in-progress typing run.
    /// Returns whether it moved.
    pub fn set_cursor(&mut self, offset: usize, extend: bool) -> bool {
        let offset = snap_to_grapheme(&self.text, offset);
        let moved = offset != self.cursor;
        self.cursor = offset;
        if !extend {
            self.anchor = offset;
        }
        if moved {
            self.insert_run = None;
        }
        moved
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
        self.insert_run = None;
    }

    /// Selects the byte range `start..end`, clamping both ends into the
    /// document. Used by the view after a transform so the transformed output
    /// stays selected. Returns whether the selection changed.
    pub fn select_range(&mut self, start: usize, end: usize) -> bool {
        let start = self.clamp_boundary(start);
        let end = self.clamp_boundary(end).max(start);
        let changed = self.anchor != start || self.cursor != end;
        self.anchor = start;
        self.cursor = end;
        if changed {
            // Explicit selection breaks any in-progress typing run.
            self.insert_run = None;
        }
        changed
    }

    // ---- Navigation -----------------------------------------------------

    pub fn move_left(&mut self, extend: bool) -> bool {
        let (start, end) = self.selection_range();
        let target = if !extend && start != end {
            start
        } else if self.cursor == 0 {
            0
        } else {
            previous_grapheme_boundary(&self.text, self.cursor)
        };
        self.set_cursor(target, extend)
    }

    pub fn move_right(&mut self, extend: bool) -> bool {
        let (start, end) = self.selection_range();
        let target = if !extend && start != end {
            end
        } else {
            next_grapheme_boundary(&self.text, self.cursor)
        };
        self.set_cursor(target, extend)
    }

    pub fn move_home(&mut self, extend: bool) -> bool {
        let line = self.line_of_offset(self.cursor);
        let start = self.lines.line_start(line);
        self.set_cursor(start, extend)
    }

    pub fn move_end(&mut self, extend: bool) -> bool {
        let line = self.line_of_offset(self.cursor);
        let (_, end) = self.line_range(line);
        self.set_cursor(end, extend)
    }

    /// Byte range of the "word" around `offset` for double-click selection.
    ///
    /// Graphemes are grouped into runs of word characters (alphanumeric or `_`),
    /// whitespace, and everything else. The run is expanded over whole clusters
    /// so a base character and its combining marks are never split.
    pub fn word_range_at(&self, offset: usize) -> (usize, usize) {
        let offset = snap_to_grapheme(&self.text, offset);
        if self.text.is_empty() {
            return (0, 0);
        }
        let class_at = |start: usize| -> u8 {
            match self.text[start..].chars().next() {
                Some(c) if c.is_alphanumeric() || c == '_' => 0,
                Some(c) if c.is_whitespace() => 1,
                _ => 2,
            }
        };
        // Snap onto the cluster containing `offset`; at end-of-text select the
        // last cluster. Expansion then walks whole clusters outward, so the
        // cost is the size of the run, not the size of the document.
        let current = if offset == self.text.len() {
            previous_grapheme_boundary(&self.text, offset)
        } else {
            offset
        };
        let target = class_at(current);

        let mut first = current;
        while first > 0 {
            let previous = previous_grapheme_boundary(&self.text, first);
            if class_at(previous) != target {
                break;
            }
            first = previous;
        }

        let mut last = next_grapheme_boundary(&self.text, current);
        while last < self.text.len() && class_at(last) == target {
            last = next_grapheme_boundary(&self.text, last);
        }
        (first, last)
    }

    /// Counts the characters and words in `start..end`, as `(chars, words)`.
    ///
    /// A character is an extended grapheme cluster, matching the cursor
    /// column's unit. Words use the same classification as [`Self::word_range_at`]:
    /// maximal runs of alphanumeric or `_` clusters, so whitespace and
    /// punctuation separate words but are not themselves words. Callers should
    /// cache the result for a selection; counting is proportional to the
    /// selected text, not the document.
    pub fn selection_stats(&self, start: usize, end: usize) -> (usize, usize) {
        let start = self.clamp_boundary(start);
        let end = self.clamp_boundary(end.max(start));
        let selection = &self.text[start..end];
        let mut chars = 0;
        let mut words = 0;
        let mut in_word = false;
        for (cluster_start, cluster_end) in crate::grapheme::grapheme_ranges(selection) {
            chars += 1;
            let is_word = selection[cluster_start..cluster_end]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if is_word && !in_word {
                words += 1;
            }
            in_word = is_word;
        }
        (chars, words)
    }

    // ---- Editing --------------------------------------------------------

    /// Replaces `start..end` with `insert`. Returns whether the text changed.
    ///
    /// A run of contiguous single-character insertions (i.e. typing) collapses
    /// into one undo step: only the first keystroke records an edit, and later
    /// keystrokes append to its inserted text.
    pub fn replace_range(&mut self, start: usize, end: usize, insert: &str) -> bool {
        let start = self.clamp_boundary(start);
        let end = self.clamp_boundary(end.max(start));
        if start == end && insert.is_empty() {
            return false;
        }

        let insert = normalize_newlines(insert);
        let inserted = insert.as_ref();
        // Enforce the hard size cap. `start`/`end` are clamped above, so
        // `end - start <= text.len()` and the subtraction cannot underflow.
        if self.text.len() - (end - start) + inserted.len() > MAX_DOCUMENT_BYTES {
            return false;
        }
        let single_char = start == end && !inserted.contains('\n') && inserted.chars().count() == 1;

        // Replacing a range with the same bytes is not an edit: leave history
        // (including redo) untouched and report no change.
        if &self.text[start..end] == inserted {
            self.insert_run = None;
            self.cursor = start + inserted.len();
            self.anchor = self.cursor;
            return false;
        }

        let coalesce = single_char
            && self.insert_run.is_some_and(|run| run.end == start)
            && self.cursor == start
            && self.anchor == start
            && matches!(self.undo.back(), Some(HistoryStep::Text(_)));

        // Any successful edit invalidates the redo history.
        self.redo.clear();
        self.redo_bytes = 0;

        if coalesce {
            // Extend the current typing run's inserted text. `coalesce` above
            // guarantees the back step is a text edit.
            if let Some(HistoryStep::Text(edit)) = self.undo.back_mut() {
                edit.inserted.push_str(inserted);
                self.undo_bytes += inserted.len();
            }
            if let Some(run) = &mut self.insert_run {
                run.end = start + inserted.len();
            }
            self.evict_undo();
        } else {
            let removed = self.text[start..end].to_string();
            self.push_edit(Edit {
                start,
                removed,
                inserted: inserted.to_string(),
                before_cursor: self.cursor,
                before_anchor: self.anchor,
            });
            self.insert_run = if single_char {
                Some(InsertRun {
                    end: start + inserted.len(),
                })
            } else {
                None
            };
        }

        self.apply_text_edit(start, end, inserted);
        self.cursor = start + inserted.len();
        self.anchor = self.cursor;
        true
    }

    pub fn insert_text(&mut self, text: &str) -> bool {
        let (start, end) = self.selection_range();
        self.replace_range(start, end, text)
    }

    pub fn backspace(&mut self) -> bool {
        let (start, end) = self.selection_range();
        if start != end {
            return self.replace_range(start, end, "");
        }
        if self.cursor == 0 {
            return false;
        }
        let previous = previous_grapheme_boundary(&self.text, self.cursor);
        self.replace_range(previous, self.cursor, "")
    }

    pub fn delete_forward(&mut self) -> bool {
        let (start, end) = self.selection_range();
        if start != end {
            return self.replace_range(start, end, "");
        }
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.replace_range(self.cursor, next, "")
    }

    // ---- History --------------------------------------------------------

    /// Record a new undo step and evict old ones to stay within the count and
    /// byte budgets.
    fn push_edit(&mut self, edit: Edit) {
        self.undo_bytes += edit.size();
        self.undo.push_back(HistoryStep::Text(edit));
        self.evict_undo();
    }

    fn evict_undo(&mut self) {
        while self.undo.len() > MAX_UNDO
            || (self.undo_bytes > MAX_UNDO_BYTES && self.undo.len() > 1)
        {
            let Some(front) = self.undo.pop_front() else {
                break;
            };
            self.undo_bytes = self.undo_bytes.saturating_sub(front.size());
        }
    }

    /// Apply a text replacement, keeping the line index, UTF-16 length, and
    /// revision in sync. Does not touch history or the cursor.
    fn apply_text_edit(&mut self, start: usize, end: usize, insert: &str) {
        let removed_utf16 = self.text[start..end].encode_utf16().count();
        let inserted_utf16 = insert.encode_utf16().count();
        self.text.replace_range(start..end, insert);
        self.lines.apply_edit(&self.text, start, end, insert.len());
        self.utf16_len = self.utf16_len - removed_utf16 + inserted_utf16;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop_back() else {
            return false;
        };
        self.undo_bytes = self.undo_bytes.saturating_sub(step.size());

        match step {
            HistoryStep::Text(edit) => {
                let inserted_len = edit.inserted.len();
                debug_assert_eq!(
                    self.text.get(edit.start..edit.start + inserted_len),
                    Some(edit.inserted.as_str()),
                    "undo step does not match the current text"
                );
                self.apply_text_edit(edit.start, edit.start + inserted_len, &edit.removed);
                self.cursor = self.clamp_boundary(edit.before_cursor);
                self.anchor = self.clamp_boundary(edit.before_anchor);
                self.insert_run = None;

                self.redo_bytes += edit.size();
                self.redo.push_back(HistoryStep::Text(edit));
            }
            HistoryStep::LineEnding { before, after } => {
                // Metadata-only: text, cursor, and anchor are unchanged.
                self.line_ending = before;
                self.revision = self.revision.wrapping_add(1);
                self.insert_run = None;
                self.redo
                    .push_back(HistoryStep::LineEnding { before, after });
            }
        }
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(step) = self.redo.pop_back() else {
            return false;
        };
        self.redo_bytes = self.redo_bytes.saturating_sub(step.size());

        match step {
            HistoryStep::Text(edit) => {
                let removed_len = edit.removed.len();
                debug_assert_eq!(
                    self.text.get(edit.start..edit.start + removed_len),
                    Some(edit.removed.as_str()),
                    "redo step does not match the current text"
                );
                self.apply_text_edit(edit.start, edit.start + removed_len, &edit.inserted);
                self.cursor = edit.start + edit.inserted.len();
                self.anchor = self.cursor;
                self.insert_run = None;

                self.undo_bytes += edit.size();
                self.undo.push_back(HistoryStep::Text(edit));
            }
            HistoryStep::LineEnding { before, after } => {
                self.line_ending = after;
                self.revision = self.revision.wrapping_add(1);
                self.insert_run = None;
                self.undo
                    .push_back(HistoryStep::LineEnding { before, after });
            }
        }
        true
    }
}

/// Normalizes CRLF/CR to `\n`, borrowing when there is nothing to change.
fn normalize_newlines(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Detects the line-ending style of raw text before it is normalized.
///
/// Returns the single style when every terminator agrees, `Mixed` when several
/// styles appear, and `Lf` when there are no terminators at all (the common
/// default). `\r\n` is disambiguated from a lone `\r` by looking one byte
/// ahead.
pub fn detect_line_ending(text: &str) -> LineEnding {
    if !text.contains('\r') {
        return LineEnding::Lf;
    }
    let bytes = text.as_bytes();
    let mut seen: Option<LineEnding> = None;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                let kind = if bytes.get(index + 1) == Some(&b'\n') {
                    LineEnding::Crlf
                } else {
                    LineEnding::Cr
                };
                if seen.is_some_and(|existing| existing != kind) {
                    return LineEnding::Mixed;
                }
                seen = Some(kind);
                index += if kind == LineEnding::Crlf { 2 } else { 1 };
            }
            b'\n' => {
                if seen.is_some_and(|existing| existing != LineEnding::Lf) {
                    return LineEnding::Mixed;
                }
                seen = Some(LineEnding::Lf);
                index += 1;
            }
            _ => index += 1,
        }
    }
    seen.unwrap_or(LineEnding::Lf)
}

// ---- UTF-16 helpers (the input handler works in UTF-16 offsets) ---------

/// Converts a byte offset into a UTF-16 code-unit offset, snapping down to a
/// char boundary first.
pub fn utf16_offset(text: &str, byte_offset: usize) -> usize {
    let mut boundary = byte_offset.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].encode_utf16().count()
}

/// Converts a UTF-16 code-unit offset into a byte offset. An offset that lands
/// inside a surrogate pair snaps to the start of that character.
pub fn byte_offset_for_utf16(text: &str, utf16: usize) -> usize {
    if utf16 == 0 {
        return 0;
    }
    let mut count = 0;
    for (index, ch) in text.char_indices() {
        count += ch.len_utf16();
        if count > utf16 {
            return index;
        }
        if count == utf16 {
            return index + ch.len_utf8();
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::new(text.to_string())
    }

    #[test]
    fn select_range_selects_the_replaced_output() {
        let mut document = doc("hello");
        assert!(document.replace_range(0, 5, "HELLO"));
        assert!(document.select_range(0, 5));
        assert_eq!(document.selection_range(), (0, 5));
        assert_eq!(document.anchor(), 0);
        assert_eq!(document.cursor(), 5);
    }

    #[test]
    fn select_range_clamps_into_the_document() {
        let mut document = doc("aé");
        document.select_range(0, 99);
        assert_eq!(document.selection_range(), (0, 3));
        assert!(!document.select_range(0, 3));
    }

    #[test]
    fn new_normalizes_crlf_and_places_cursor_at_end() {
        let document = doc("a\r\nb\rc");
        assert_eq!(document.text(), "a\nb\nc");
        assert_eq!(document.cursor(), document.len());
        assert_eq!(document.anchor(), document.len());
    }

    #[test]
    fn new_detects_the_line_ending_style() {
        assert_eq!(doc("a\nb\n").line_ending(), LineEnding::Lf);
        assert_eq!(doc("a\r\nb\r\n").line_ending(), LineEnding::Crlf);
        assert_eq!(doc("a\rb\r").line_ending(), LineEnding::Cr);
        assert_eq!(doc("a\r\nb\rc").line_ending(), LineEnding::Mixed);
        // No terminators defaults to LF.
        assert_eq!(doc("abc").line_ending(), LineEnding::Lf);
        assert_eq!(doc("").line_ending(), LineEnding::Lf);
    }

    #[test]
    fn detect_line_ending_distinguishes_crlf_from_lone_cr() {
        assert_eq!(detect_line_ending("\r\n\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("\r\r"), LineEnding::Cr);
        assert_eq!(detect_line_ending("\n\r"), LineEnding::Mixed);
        assert_eq!(detect_line_ending("\r\n\n"), LineEnding::Mixed);
    }

    #[test]
    fn set_line_ending_is_undoable_and_preserves_selection() {
        let mut document = doc("a\nb");
        assert_eq!(document.line_ending(), LineEnding::Lf);
        document.set_cursor(1, false);
        document.set_cursor(3, true);
        let selection = document.selection_range();

        assert!(document.set_line_ending(LineEnding::Crlf));
        assert_eq!(document.line_ending(), LineEnding::Crlf);
        // Metadata-only: the selection is untouched.
        assert_eq!(document.selection_range(), selection);

        assert!(document.undo());
        assert_eq!(document.line_ending(), LineEnding::Lf);
        assert_eq!(document.selection_range(), selection);

        assert!(document.redo());
        assert_eq!(document.line_ending(), LineEnding::Crlf);
    }

    #[test]
    fn set_line_ending_rejects_no_op_and_mixed() {
        let mut document = doc("a\nb");
        assert!(!document.set_line_ending(LineEnding::Lf));
        assert!(!document.set_line_ending(LineEnding::Mixed));
        assert_eq!(document.line_ending(), LineEnding::Lf);
    }

    #[test]
    fn conversion_clears_redo() {
        let mut document = doc("a\nb");
        assert!(document.insert_text("x"));
        assert!(document.undo());
        // A conversion after undo makes the old edit unreachable forward.
        assert!(document.set_line_ending(LineEnding::Cr));
        assert!(!document.redo());
        assert_eq!(document.line_ending(), LineEnding::Cr);
    }

    #[test]
    fn text_with_line_endings_materializes_the_style() {
        let mut document = doc("a\nb\n");
        assert_eq!(&*document.text_with_line_endings(), "a\nb\n");
        assert!(document.set_line_ending(LineEnding::Crlf));
        assert_eq!(&*document.text_with_line_endings(), "a\r\nb\r\n");
        assert!(document.set_line_ending(LineEnding::Cr));
        assert_eq!(&*document.text_with_line_endings(), "a\rb\r");
        // The model's own text stays `\n`-only regardless of the style.
        assert_eq!(document.text(), "a\nb\n");
    }

    #[test]
    fn mixed_line_endings_export_as_plain_lf() {
        let document = doc("a\r\nb\nc");
        assert_eq!(document.line_ending(), LineEnding::Mixed);
        assert_eq!(&*document.text_with_line_endings(), "a\nb\nc");
    }

    #[test]
    fn line_model_handles_trailing_newline() {
        let document = doc("ab\ncd\n");
        assert_eq!(document.line_count(), 3);
        assert_eq!(document.line_of_offset(0), 0);
        assert_eq!(document.line_of_offset(3), 1);
        assert_eq!(document.line_of_offset(6), 2);
        assert_eq!(document.line_range(0), (0, 2));
        assert_eq!(document.line_range(1), (3, 5));
        // A trailing newline produces an empty final line.
        assert_eq!(document.line_range(2), (6, 6));
    }

    #[test]
    fn selection_range_is_ordered_regardless_of_direction() {
        let mut document = doc("abcdef");
        document.select_all();
        assert_eq!(document.selection_range(), (0, 6));

        // Re-seat the cursor at the end, then extend leftward so the cursor is
        // before the anchor; selection_range still reports it low-to-high.
        document.set_cursor(6, false);
        document.set_cursor(1, true);
        assert_eq!(document.cursor(), 1);
        assert_eq!(document.anchor(), 6);
        assert_eq!(document.selection_range(), (1, 6));
    }

    #[test]
    fn clamp_boundary_snaps_into_char() {
        let document = doc("a😀b");
        // Byte 2 is inside the emoji's 4-byte encoding.
        assert_eq!(document.clamp_boundary(2), 1);
        assert_eq!(document.clamp_boundary(usize::MAX), document.len());
    }

    #[test]
    fn utf16_round_trips_at_char_boundaries() {
        let text = "a😀b";
        assert_eq!(utf16_offset(text, 0), 0);
        assert_eq!(utf16_offset(text, 1), 1);
        assert_eq!(utf16_offset(text, 5), 3);
        assert_eq!(utf16_offset(text, 6), 4);

        assert_eq!(byte_offset_for_utf16(text, 0), 0);
        assert_eq!(byte_offset_for_utf16(text, 1), 1);
        assert_eq!(byte_offset_for_utf16(text, 3), 5);
        assert_eq!(byte_offset_for_utf16(text, 4), 6);
        // Inside the surrogate pair snaps back to the character start.
        assert_eq!(byte_offset_for_utf16(text, 2), 1);
    }

    #[test]
    fn backspace_removes_whole_astral_char() {
        let mut document = doc("a😀");
        assert!(document.backspace());
        assert_eq!(document.text(), "a");
        assert_eq!(document.cursor(), 1);
    }

    #[test]
    fn delete_forward_removes_whole_astral_char() {
        let mut document = doc("😀b");
        document.set_cursor(0, false);
        assert!(document.delete_forward());
        assert_eq!(document.text(), "b");
    }

    #[test]
    fn replace_range_normalizes_input_newlines() {
        let mut document = doc("ab");
        document.set_cursor(1, false);
        assert!(document.replace_range(1, 1, "\r\n"));
        assert_eq!(document.text(), "a\nb");
    }

    #[test]
    fn typing_run_collapses_to_one_undo_step() {
        let mut document = doc("");
        assert!(document.insert_text("a"));
        assert!(document.insert_text("b"));
        assert!(document.insert_text("c"));
        assert_eq!(document.text(), "abc");

        // One undo removes the whole burst.
        assert!(document.undo());
        assert_eq!(document.text(), "");
        assert_eq!(document.cursor(), 0);

        // Redo restores it.
        assert!(document.redo());
        assert_eq!(document.text(), "abc");
    }

    #[test]
    fn cursor_move_breaks_the_typing_run() {
        let mut document = doc("");
        document.insert_text("a");
        document.insert_text("b");

        document.set_cursor(0, false);
        document.insert_text("x");

        assert_eq!(document.text(), "xab");
        // First undo removes only the post-move character...
        assert!(document.undo());
        assert_eq!(document.text(), "ab");
        // ...and a second undo removes the earlier burst.
        assert!(document.undo());
        assert_eq!(document.text(), "");
    }

    #[test]
    fn multi_char_insert_is_a_single_step() {
        let mut document = doc("ab");
        assert!(document.insert_text("XYZ"));
        assert_eq!(document.text(), "abXYZ");
        assert!(document.undo());
        assert_eq!(document.text(), "ab");
    }

    #[test]
    fn replacing_a_selection_is_one_step() {
        let mut document = doc("abcdef");
        document.select_all();
        assert!(document.insert_text("Z"));
        assert_eq!(document.text(), "Z");
        assert!(document.undo());
        assert_eq!(document.text(), "abcdef");
    }

    #[test]
    fn no_op_edit_reports_no_change() {
        let mut document = doc("abc");
        assert!(!document.replace_range(1, 1, ""));
        assert!(!document.insert_text(""));
        assert_eq!(document.text(), "abc");
    }

    #[test]
    fn undo_history_is_bounded() {
        let mut document = doc("");
        // Each two-character insert is its own (non-coalesced) step.
        for _ in 0..(MAX_UNDO + 5) {
            assert!(document.replace_range(0, 0, "xy"));
        }
        assert_eq!(document.undo.len(), MAX_UNDO);

        let mut undos = 0;
        while document.undo() {
            undos += 1;
        }
        assert_eq!(undos, MAX_UNDO);
    }

    fn assert_undo_accounting(document: &Document) {
        let undo_sum: usize = document.undo.iter().map(HistoryStep::size).sum();
        assert_eq!(document.undo_bytes, undo_sum);
        let redo_sum: usize = document.redo.iter().map(HistoryStep::size).sum();
        assert_eq!(document.redo_bytes, redo_sum);
    }

    #[test]
    fn undo_stores_only_the_edit_bytes() {
        let mut document = doc(&"a".repeat(1_000_000));
        // Move into the middle so this is not an append, then type one character.
        document.set_cursor(500_000, false);
        assert!(document.insert_text("b"));
        let edit = document.undo.back().expect("one undo step");
        assert_eq!(edit.size(), 1, "edit stored {} bytes", edit.size());
    }

    #[test]
    fn undo_accounting_matches_retained_edits() {
        let mut document = doc("abcdef");
        assert_undo_accounting(&document);
        document.select_all();
        assert!(document.insert_text("X"));
        assert_undo_accounting(&document);
        document.set_cursor(1, false);
        assert!(document.insert_text("hi"));
        assert_undo_accounting(&document);
        assert!(document.undo());
        assert_undo_accounting(&document);
        assert!(document.undo());
        assert_undo_accounting(&document);
        assert!(document.redo());
        assert_undo_accounting(&document);
    }

    #[test]
    fn undo_redo_round_trip_through_mixed_edits() {
        let mut document = doc("hello world\nsecond line");
        document.set_cursor(5, false);
        assert!(document.insert_text(","));
        assert!(document.replace_range(0, 1, "H"));
        document.set_cursor(document.len(), false);
        assert!(document.backspace());
        let after = document.text().to_string();

        while document.undo() {}
        assert_eq!(document.text(), "hello world\nsecond line");

        while document.redo() {}
        assert_eq!(document.text(), after);
    }

    #[test]
    fn editing_after_undo_clears_redo() {
        let mut document = doc("");
        assert!(document.insert_text("a"));
        assert!(document.insert_text("bc"));
        assert!(document.undo());
        assert!(!document.redo.is_empty());
        assert!(document.insert_text("d"));
        assert!(document.redo.is_empty());
    }

    #[test]
    fn identical_replacement_preserves_redo_and_reports_no_change() {
        let mut document = Document::new("ab".to_string());
        document.set_cursor(2, false);
        assert!(document.insert_text("c"));
        assert!(document.undo());
        // Replacing `ab` with `ab` changes nothing.
        assert!(!document.replace_range(0, 2, "ab"));
        // Redo from before the no-op is still available.
        assert!(document.redo());
        assert_eq!(document.text(), "abc");
    }

    #[test]
    fn home_and_end_follow_the_current_line() {
        let mut document = doc("ab\ncd");
        document.set_cursor(4, false); // inside the second line
        assert!(document.move_home(false));
        assert_eq!(document.cursor(), 3);
        assert!(document.move_end(false));
        assert_eq!(document.cursor(), 5);
    }

    #[test]
    fn move_left_collapses_selection_without_extending() {
        let mut document = doc("abcdef");
        document.select_all();
        assert!(document.move_left(false));
        assert_eq!(document.cursor(), 0);
        assert_eq!(document.anchor(), 0);
    }

    #[test]
    fn backspace_with_selection_deletes_the_selection() {
        let mut document = doc("abcdef");
        document.select_all();
        assert!(document.backspace());
        assert_eq!(document.text(), "");
    }

    #[test]
    fn backspace_removes_whole_grapheme_cluster() {
        let mut document = doc("e\u{301}");
        assert!(document.backspace());
        assert!(document.text().is_empty());
    }

    #[test]
    fn delete_forward_removes_whole_grapheme_cluster() {
        let mut document = doc("e\u{301}x");
        document.set_cursor(0, false);
        assert!(document.delete_forward());
        assert_eq!(document.text(), "x");
    }

    #[test]
    fn movement_steps_over_grapheme_clusters() {
        let mut document = doc("e\u{301}x");
        document.set_cursor(document.len(), false);
        assert!(document.move_left(false));
        assert_eq!(document.cursor(), "e\u{301}".len());
        assert!(document.move_left(false));
        assert_eq!(document.cursor(), 0);
        assert!(document.move_right(false));
        assert_eq!(document.cursor(), "e\u{301}".len());
    }

    #[test]
    fn set_cursor_snaps_into_a_grapheme() {
        let mut document = doc("e\u{301}x");
        // Byte 1 is between the base character and its combining mark.
        document.set_cursor(1, false);
        assert_eq!(document.cursor(), 0);
    }

    #[test]
    fn flag_and_zwj_sequences_are_single_characters() {
        let flag = "\u{1F1FA}\u{1F1F8}";
        let mut document = doc(&format!("a{flag}"));
        assert!(document.backspace());
        assert_eq!(document.text(), "a");

        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let mut document = doc(&format!("a{family}"));
        assert!(document.backspace());
        assert_eq!(document.text(), "a");
    }

    #[test]
    fn word_range_groups_words_whitespace_and_punctuation() {
        let document = doc("foo bar,baz");
        assert_eq!(document.word_range_at(1), (0, 3));
        assert_eq!(document.word_range_at(3), (3, 4));
        assert_eq!(document.word_range_at(5), (4, 7));
        assert_eq!(document.word_range_at(7), (7, 8));
        assert_eq!(document.word_range_at(8), (8, 11));
    }

    #[test]
    fn word_range_respects_grapheme_clusters() {
        let document = doc("e\u{301}x y");
        assert_eq!(document.word_range_at(0), (0, 4));
        assert_eq!(document.word_range_at(1), (0, 4));
    }

    #[test]
    fn word_range_of_empty_document_is_empty() {
        assert_eq!(doc("").word_range_at(0), (0, 0));
    }

    #[test]
    fn selection_stats_counts_graphemes_and_words() {
        let document = doc("foo bar,baz");
        assert_eq!(document.selection_stats(0, 11), (11, 3));
        assert_eq!(document.selection_stats(4, 7), (3, 1));
        // Whitespace and punctuation split words but are not words.
        assert_eq!(document.selection_stats(3, 8), (5, 1));
        assert_eq!(document.selection_stats(7, 8), (1, 0));
    }

    #[test]
    fn selection_stats_counts_clusters_not_scalars() {
        // Combined mark: one character, one word.
        let document = doc("e\u{301}x y");
        assert_eq!(document.selection_stats(0, 6), (4, 2));
        // Astral emoji: one character but not a word.
        let document = doc("a😀b");
        assert_eq!(document.selection_stats(0, 6), (3, 2));
    }

    #[test]
    fn selection_stats_of_empty_and_clamped_ranges() {
        let document = doc("hello");
        assert_eq!(document.selection_stats(2, 2), (0, 0));
        assert_eq!(document.selection_stats(0, 99), (5, 1));
        assert_eq!(document.selection_stats(3, 1), (0, 0));
    }

    #[test]
    fn revision_advances_on_edits_and_undo() {
        let mut document = doc("abc");
        let before = document.revision();
        assert!(document.insert_text("d"));
        let after_edit = document.revision();
        assert!(after_edit > before);
        assert!(document.undo());
        assert!(document.revision() > after_edit);
    }

    #[test]
    fn edits_beyond_the_size_cap_are_refused() {
        let mut document = doc(&"a".repeat(MAX_DOCUMENT_BYTES));
        let before = document.revision();
        // One more byte would exceed the cap.
        assert!(!document.insert_text("b"));
        assert_eq!(document.len(), MAX_DOCUMENT_BYTES);
        assert_eq!(document.revision(), before);
        // A shrinking edit is still allowed at the cap.
        assert!(document.backspace());
    }

    #[test]
    fn document_utf16_matches_free_functions() {
        for text in [
            "",
            "abc",
            "héllo",
            "😀",
            "a😀b",
            "e\u{301}x",
            "😀😀",
            "a\nb😀c",
            "😀x😀",
        ] {
            let document = doc(text);
            let utf16_len = text.encode_utf16().count();
            for byte in 0..=text.len() {
                assert_eq!(
                    document.utf16_offset(byte),
                    utf16_offset(text, byte),
                    "utf16_offset({byte}) for {text:?}"
                );
            }
            for unit in 0..=(utf16_len + 1) {
                assert_eq!(
                    document.byte_offset_for_utf16(unit),
                    byte_offset_for_utf16(text, unit),
                    "byte_offset_for_utf16({unit}) for {text:?}"
                );
            }
        }
    }

    #[test]
    fn document_utf16_tracks_edits() {
        let mut document = doc("start\n");
        assert!(document.insert_text("😀"));
        assert!(document.replace_range(0, 0, "héllo "));
        let text = document.text().to_string();
        let utf16_len = text.encode_utf16().count();
        for byte in 0..=text.len() {
            assert_eq!(document.utf16_offset(byte), utf16_offset(&text, byte));
        }
        for unit in 0..=(utf16_len + 1) {
            assert_eq!(
                document.byte_offset_for_utf16(unit),
                byte_offset_for_utf16(&text, unit)
            );
        }
    }

    #[test]
    fn local_word_range_matches_full_scan() {
        fn reference(text: &str, offset: usize) -> (usize, usize) {
            let offset = snap_to_grapheme(text, offset);
            let graphemes: Vec<(usize, usize)> = crate::grapheme::grapheme_ranges(text).collect();
            if graphemes.is_empty() {
                return (0, 0);
            }
            let class = |start: usize| -> u8 {
                match text[start..].chars().next() {
                    Some(c) if c.is_alphanumeric() || c == '_' => 0,
                    Some(c) if c.is_whitespace() => 1,
                    _ => 2,
                }
            };
            let index = graphemes
                .iter()
                .rposition(|&(start, _)| start <= offset)
                .unwrap_or(0);
            let target = class(graphemes[index].0);
            let mut first = index;
            while first > 0 && class(graphemes[first - 1].0) == target {
                first -= 1;
            }
            let mut last = index;
            while last + 1 < graphemes.len() && class(graphemes[last + 1].0) == target {
                last += 1;
            }
            (graphemes[first].0, graphemes[last].1)
        }

        for text in [
            "",
            "foo bar,baz",
            "  spaced  out ",
            "e\u{301}x y",
            "a😀b 😀",
            "line one\nline two\n",
            "   \n\t",
        ] {
            let document = doc(text);
            for offset in 0..=(text.len() + 2) {
                assert_eq!(
                    document.word_range_at(offset),
                    reference(text, offset),
                    "word_range_at({offset}) for {text:?}"
                );
            }
        }
    }
}
