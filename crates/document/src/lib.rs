//! Pure document model for Zapslab.
//!
//! Everything here is plain `std`: text, the byte-offset cursor/anchor, the
//! line-start index, the bounded undo/redo history, and the UTF-16 ⇄ byte
//! offset conversion used at the platform input boundary. There are no `gpui`,
//! `wasm-bindgen`, or `web-sys` imports, so the whole model is unit-testable
//! offline with `cargo test`.
//!
//! Invariants:
//! - `text` never contains `\r`; CRLF/CR is normalized on the way in.
//! - `cursor` and `anchor` are always char boundaries into `text`.
//! - `line_starts[0] == 0` and every entry points just past a `\n`.

pub mod display;
pub mod encoding;
pub mod escape;
pub mod grapheme;
pub mod hexdump;

use std::{borrow::Cow, collections::VecDeque};

use crate::grapheme::{
    grapheme_ranges, next_grapheme_boundary, previous_grapheme_boundary, snap_to_grapheme,
};

/// Maximum number of undo snapshots retained. Older snapshots are evicted
/// from the front of the ring so the ends stay O(1).
pub const MAX_UNDO: usize = 200;

#[derive(Clone)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: usize,
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
    line_starts: Vec<usize>,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    insert_run: Option<InsertRun>,
}

impl Document {
    /// Builds a document from `text`, normalizing newlines and placing the
    /// cursor at the end.
    pub fn new(text: String) -> Self {
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text
        };
        let cursor = text.len();
        let mut document = Self {
            text,
            cursor,
            anchor: cursor,
            line_starts: Vec::new(),
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            insert_run: None,
        };
        document.rebuild_line_starts();
        document
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

    fn rebuild_line_starts(&mut self) {
        self.line_starts.clear();
        self.line_starts.push(0);
        for (index, byte) in self.text.bytes().enumerate() {
            if byte == b'\n' {
                self.line_starts.push(index + 1);
            }
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_of_offset(&self, offset: usize) -> usize {
        match self.line_starts.partition_point(|&start| start <= offset) {
            0 => 0,
            count => count - 1,
        }
    }

    /// Byte range of a line, excluding its trailing newline.
    pub fn line_range(&self, line: usize) -> (usize, usize) {
        let start = self.line_starts[line];
        let end = if line + 1 < self.line_starts.len() {
            self.line_starts[line + 1] - 1
        } else {
            self.text.len()
        };
        (start, end)
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
        let start = self.line_starts[line];
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
        let graphemes: Vec<(usize, usize)> = grapheme_ranges(&self.text).collect();
        if graphemes.is_empty() {
            return (0, 0);
        }
        let class = |start: usize| -> u8 {
            match self.text[start..].chars().next() {
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

    // ---- Editing --------------------------------------------------------

    /// Replaces `start..end` with `insert`. Returns whether the text changed.
    ///
    /// A run of contiguous single-character insertions (i.e. typing) collapses
    /// into one undo step: only the first keystroke of the run pushes a
    /// snapshot, and the snapshot captures the state before the run began.
    pub fn replace_range(&mut self, start: usize, end: usize, insert: &str) -> bool {
        let start = self.clamp_boundary(start);
        let end = self.clamp_boundary(end.max(start));
        if start == end && insert.is_empty() {
            return false;
        }

        let insert = normalize_newlines(insert);
        let inserted = insert.as_ref();
        let single_char = start == end && !inserted.contains('\n') && inserted.chars().count() == 1;

        let coalesce = single_char
            && self.insert_run.is_some_and(|run| run.end == start)
            && self.cursor == start
            && self.anchor == start;

        if coalesce {
            if let Some(run) = &mut self.insert_run {
                run.end = start + inserted.len();
            }
        } else {
            self.push_undo();
            self.insert_run = if single_char {
                Some(InsertRun {
                    end: start + inserted.len(),
                })
            } else {
                None
            };
        }

        self.text.replace_range(start..end, inserted);
        self.cursor = start + inserted.len();
        self.anchor = self.cursor;
        self.rebuild_line_starts();
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

    fn push_undo(&mut self) {
        let unchanged = self.undo.back().is_some_and(|snapshot| {
            snapshot.text == self.text
                && snapshot.cursor == self.cursor
                && snapshot.anchor == self.anchor
        });
        if !unchanged {
            self.undo.push_back(Snapshot {
                text: self.text.clone(),
                cursor: self.cursor,
                anchor: self.anchor,
            });
            if self.undo.len() > MAX_UNDO {
                self.undo.pop_front();
            }
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop_back() else {
            return false;
        };
        self.redo.push_back(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        });
        self.restore(snapshot);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop_back() else {
            return false;
        };
        self.undo.push_back(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        });
        self.restore(snapshot);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = self.clamp_boundary(snapshot.cursor);
        self.anchor = self.clamp_boundary(snapshot.anchor);
        self.insert_run = None;
        self.rebuild_line_starts();
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
}
