//! Virtualized byte layout for the hex panel.
//!
//! The panel shows the document text encoded with the selected [`Encoding`],
//! laid out in rows of `bytes_per_row` encoded bytes. Materializing a cell per
//! encoded byte (plus a span per grapheme) costs tens of bytes per text byte,
//! which is untenable for large documents. This module instead keeps only a
//! sparse checkpoint index and materializes the rows that are actually visible
//! on demand.
//!
//! Each encoded byte remembers the text byte range it came from, so selection
//! (which lives in text offsets) can be highlighted over the bytes and a byte
//! can be mapped back to text.

use std::ops::Range;

use crate::encoding::{self, EncodeError, Encoding};
use crate::grapheme::grapheme_ranges;

/// Spacing between checkpoints in encoded bytes. A visible window is resolved
/// by scanning from the nearest preceding checkpoint, so this bounds per-frame
/// work while keeping the index tiny (a few hundred KiB at most).
const CHECKPOINT_ENCODED_BYTES: usize = 8 * 1024;

/// A grapheme boundary recorded at a known encoded offset.
#[derive(Clone, Copy)]
struct Checkpoint {
    encoded: usize,
    text: usize,
}

/// Compact, virtualized layout of the encoded buffer.
pub struct HexIndex {
    encoding: Encoding,
    bytes_per_row: usize,
    encoded_len: usize,
    checkpoints: Vec<Checkpoint>,
}

/// One encoded byte and the text byte range it came from.
pub struct HexCell {
    pub byte: u8,
    pub text_range: Range<usize>,
}

/// A run of encoded bytes making up one row.
pub struct HexRow {
    /// Encoded byte offset of the first cell.
    pub encoded_offset: usize,
    pub cells: Vec<HexCell>,
}

impl HexIndex {
    /// Build the checkpoint index. Fails when `encoding` cannot represent the
    /// text (e.g. Latin-1 with an emoji). O(text), with no per-byte allocation.
    pub fn build(
        text: &str,
        encoding: Encoding,
        bytes_per_row: usize,
    ) -> Result<Self, EncodeError> {
        let bytes_per_row = bytes_per_row.max(1);
        let mut checkpoints = vec![Checkpoint {
            encoded: 0,
            text: 0,
        }];
        let mut encoded = 0usize;
        let mut next_checkpoint = CHECKPOINT_ENCODED_BYTES;

        for (start, end) in grapheme_ranges(text) {
            encoded += encoding::encoded_len(&text[start..end], encoding)?;
            if encoded >= next_checkpoint {
                checkpoints.push(Checkpoint { encoded, text: end });
                while next_checkpoint <= encoded {
                    next_checkpoint += CHECKPOINT_ENCODED_BYTES;
                }
            }
        }

        Ok(Self {
            encoding,
            bytes_per_row,
            encoded_len: encoded,
            checkpoints,
        })
    }

    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    /// Total encoded length in bytes.
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Number of rows: at least one, so an empty buffer is one empty row.
    pub fn row_count(&self) -> usize {
        if self.encoded_len == 0 {
            1
        } else {
            self.encoded_len.div_ceil(self.bytes_per_row)
        }
    }

    fn checkpoint_at_or_before(&self, encoded_offset: usize) -> Checkpoint {
        let target = encoded_offset.min(self.encoded_len);
        let index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.encoded <= target);
        // `checkpoints[0].encoded == 0 <= target`, so `index >= 1`.
        self.checkpoints[index - 1]
    }

    /// Materialize rows `first..=last` (clamped). Only the bytes in that window
    /// are encoded; graphemes before it contribute just their length.
    pub fn rows(&self, text: &str, first: usize, last: usize) -> Vec<HexRow> {
        let row_count = self.row_count();
        if first > last || first >= row_count {
            return Vec::new();
        }
        let last = last.min(row_count - 1);
        if self.encoded_len == 0 {
            return vec![HexRow {
                encoded_offset: 0,
                cells: Vec::new(),
            }];
        }

        let bytes_per_row = self.bytes_per_row;
        let start_encoded = first * bytes_per_row;
        let end_encoded = ((last + 1) * bytes_per_row).min(self.encoded_len);
        let checkpoint = self.checkpoint_at_or_before(start_encoded);

        let mut rows = Vec::new();
        let mut row_index = first;
        let mut row = HexRow {
            encoded_offset: start_encoded,
            cells: Vec::with_capacity(bytes_per_row),
        };
        let mut encoded = checkpoint.encoded;

        for (relative_start, relative_end) in grapheme_ranges(&text[checkpoint.text..]) {
            let start = checkpoint.text + relative_start;
            let end = checkpoint.text + relative_end;
            let Ok(length) = encoding::encoded_len(&text[start..end], self.encoding) else {
                break;
            };
            let grapheme_start = encoded;
            let grapheme_end = encoded + length;
            encoded = grapheme_end;

            if grapheme_end <= start_encoded {
                continue;
            }
            if grapheme_start >= end_encoded {
                break;
            }

            // Only the overlapping window needs the actual bytes.
            let Ok(bytes) = encoding::encode(&text[start..end], self.encoding) else {
                break;
            };
            let from = grapheme_start.max(start_encoded);
            let to = grapheme_end.min(end_encoded);
            for &byte in &bytes[(from - grapheme_start)..(to - grapheme_start)] {
                row.cells.push(HexCell {
                    byte,
                    text_range: start..end,
                });
                if row.cells.len() == bytes_per_row {
                    rows.push(row);
                    row_index += 1;
                    if row_index > last {
                        return rows;
                    }
                    row = HexRow {
                        encoded_offset: row_index * bytes_per_row,
                        cells: Vec::with_capacity(bytes_per_row),
                    };
                }
            }
        }

        if !row.cells.is_empty() {
            rows.push(row);
        }
        rows
    }

    /// Text byte range that produced the encoded byte at `encoded_offset`.
    pub fn text_range_at(&self, text: &str, encoded_offset: usize) -> Option<Range<usize>> {
        if encoded_offset >= self.encoded_len {
            return None;
        }
        let checkpoint = self.checkpoint_at_or_before(encoded_offset);
        let mut encoded = checkpoint.encoded;
        for (relative_start, relative_end) in grapheme_ranges(&text[checkpoint.text..]) {
            let start = checkpoint.text + relative_start;
            let end = checkpoint.text + relative_end;
            let length = encoding::encoded_len(&text[start..end], self.encoding).ok()?;
            if encoded_offset < encoded + length {
                return Some(start..end);
            }
            encoded += length;
        }
        None
    }

    /// Encoded byte offset of the grapheme that starts at (or contains)
    /// `text_offset`.
    ///
    /// This is the inverse of [`Self::text_range_at`]. Offsets inside a grapheme
    /// snap to its start, so the two directions always agree and callers never
    /// land mid-codepoint. The scan starts from the nearest checkpoint, keeping
    /// it local rather than whole-buffer.
    pub fn encoded_offset_at_text(&self, text: &str, text_offset: usize) -> usize {
        let text_offset = text_offset.min(text.len());
        if text_offset == 0 {
            return 0;
        }
        // Checkpoints record the text offset just past a grapheme, so the last
        // one at or before the target begins the local scan.
        let index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.text <= text_offset);
        // `checkpoints[0].text == 0 <= text_offset`, so `index >= 1`.
        let checkpoint = self.checkpoints[index - 1];
        let mut encoded = checkpoint.encoded;
        for (relative_start, relative_end) in grapheme_ranges(&text[checkpoint.text..]) {
            let start = checkpoint.text + relative_start;
            let end = checkpoint.text + relative_end;
            if text_offset < end {
                return encoded;
            }
            // The index only exists for encodable text; treat a failure as the
            // end of what can be represented rather than panicking.
            let Ok(length) = encoding::encoded_len(&text[start..end], self.encoding) else {
                return encoded;
            };
            encoded += length;
        }
        encoded
    }

    /// Row whose encoded byte window contains the grapheme at `text_offset`.
    ///
    /// Clamped to the last row so the end-of-buffer offset stays in range.
    pub fn row_of_text_offset(&self, text: &str, text_offset: usize) -> usize {
        let row = self.encoded_offset_at_text(text, text_offset) / self.bytes_per_row;
        row.min(self.row_count() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference layout: every encoded byte with its text range, chunked into
    /// rows. Used to check the virtualized index.
    fn reference(
        text: &str,
        encoding: Encoding,
        bytes_per_row: usize,
    ) -> Vec<(usize, Vec<(u8, Range<usize>)>)> {
        let mut flat: Vec<(u8, Range<usize>)> = Vec::new();
        for (start, end) in grapheme_ranges(text) {
            let bytes = encoding::encode(&text[start..end], encoding).expect("encodable");
            for byte in bytes {
                flat.push((byte, start..end));
            }
        }
        if flat.is_empty() {
            return vec![(0, Vec::new())];
        }
        let mut rows = Vec::new();
        let mut offset = 0;
        while offset < flat.len() {
            let end = (offset + bytes_per_row).min(flat.len());
            rows.push((offset, flat[offset..end].to_vec()));
            offset = end;
        }
        rows
    }

    fn check(text: &str, encoding: Encoding, bytes_per_row: usize) {
        let index = HexIndex::build(text, encoding, bytes_per_row).expect("encodable");
        // Per-grapheme lengths must add up to the whole-string encoding, since
        // the byte-edit path encodes the buffer in one piece and indexes it by
        // these offsets.
        assert_eq!(
            index.encoded_len(),
            encoding::encode(text, encoding).expect("encodable").len(),
            "encoded_len for {text:?}"
        );
        let expected = reference(text, encoding, bytes_per_row);
        assert_eq!(index.row_count(), expected.len(), "row_count for {text:?}");

        // Every window (including the whole thing) matches the reference.
        for first in 0..expected.len() {
            for last in first..expected.len() {
                let rows = index.rows(text, first, last);
                assert_eq!(
                    rows.len(),
                    last - first + 1,
                    "rows({first},{last}) for {text:?}"
                );
                for (offset, row) in rows.iter().enumerate() {
                    let (expected_offset, expected_cells) = &expected[first + offset];
                    assert_eq!(row.encoded_offset, *expected_offset);
                    assert_eq!(row.cells.len(), expected_cells.len());
                    for (cell, (byte, range)) in row.cells.iter().zip(expected_cells) {
                        assert_eq!(cell.byte, *byte);
                        assert_eq!(&cell.text_range, range);
                    }
                }
            }
        }

        // Every encoded byte maps back to its grapheme's text range.
        for (offset, cells) in expected.iter().flat_map(|(_, cells)| cells).enumerate() {
            assert_eq!(
                index.text_range_at(text, offset),
                Some(cells.1.clone()),
                "text_range_at({offset}) for {text:?}"
            );
        }
        assert_eq!(index.text_range_at(text, index.encoded_len()), None);
    }

    #[test]
    fn empty_buffer_is_one_empty_row() {
        let index = HexIndex::build("", Encoding::Utf8, 16).unwrap();
        assert_eq!(index.row_count(), 1);
        let rows = index.rows("", 0, 0);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].cells.is_empty());
    }

    #[test]
    fn matches_reference_across_encodings_and_widths() {
        for text in [
            "hello",
            "héllo",
            "a😀b",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "line one\nline two\n",
            "e\u{301}x",
        ] {
            for encoding in [
                Encoding::Utf8,
                Encoding::Utf16Le,
                Encoding::Utf16Be,
                Encoding::Utf32Le,
                Encoding::Utf32Be,
                Encoding::Latin1,
            ] {
                if encoding == Encoding::Latin1 && text.chars().any(|ch| ch as u32 > 0xFF) {
                    continue;
                }
                for bytes_per_row in [1usize, 3, 4, 7, 16] {
                    check(text, encoding, bytes_per_row);
                }
            }
        }
    }

    #[test]
    fn windows_across_checkpoints_match_reference() {
        // Longer than CHECKPOINT_ENCODED_BYTES so multiple checkpoints exist.
        let text = "abcd😀efgh\n".repeat(4000);
        for bytes_per_row in [5usize, 16, 64] {
            let index = HexIndex::build(&text, Encoding::Utf16Le, bytes_per_row).unwrap();
            let expected = reference(&text, Encoding::Utf16Le, bytes_per_row);
            assert_eq!(index.row_count(), expected.len());
            // Sample windows spread across the buffer, including near the end.
            for &first in &[
                0,
                1,
                37,
                expected.len() / 2,
                expected.len() - 3,
                expected.len() - 1,
            ] {
                let last = (first + 5).min(expected.len() - 1);
                let rows = index.rows(&text, first, last);
                assert_eq!(rows.len(), last - first + 1);
                for (offset, row) in rows.iter().enumerate() {
                    let (expected_offset, expected_cells) = &expected[first + offset];
                    assert_eq!(row.encoded_offset, *expected_offset);
                    assert_eq!(row.cells.len(), expected_cells.len());
                    for (cell, (byte, range)) in row.cells.iter().zip(expected_cells) {
                        assert_eq!(cell.byte, *byte);
                        assert_eq!(&cell.text_range, range);
                    }
                }
            }
        }
    }

    #[test]
    fn straddling_grapheme_spans_rows() {
        // "€" is three UTF-8 bytes; with two-byte rows the grapheme straddles
        // rows 0 and 1.
        let text = "€";
        let index = HexIndex::build(text, Encoding::Utf8, 2).unwrap();
        assert_eq!(index.row_count(), 2);
        let rows = index.rows(text, 0, 1);
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[1].cells.len(), 1);
        // Every byte maps back to the whole grapheme.
        for row in &rows {
            for cell in &row.cells {
                assert_eq!(cell.text_range, 0..3);
            }
        }
    }

    #[test]
    fn text_offset_maps_back_to_its_grapheme() {
        for text in [
            "",
            "hello",
            "line one\nline two\n",
            "a😀b",
            "héllo",
            "e\u{301}x",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        ] {
            for encoding in [
                Encoding::Utf8,
                Encoding::Utf16Le,
                Encoding::Utf16Be,
                Encoding::Utf32Le,
                Encoding::Utf32Be,
                Encoding::Latin1,
            ] {
                if encoding == Encoding::Latin1 && text.chars().any(|ch| ch as u32 > 0xFF) {
                    continue;
                }
                for bytes_per_row in [1usize, 3, 4, 16] {
                    let index = HexIndex::build(text, encoding, bytes_per_row).unwrap();

                    let mut boundaries: Vec<usize> =
                        grapheme_ranges(text).map(|(start, _)| start).collect();
                    boundaries.push(text.len());

                    for &boundary in &boundaries {
                        let encoded = index.encoded_offset_at_text(text, boundary);
                        assert!(encoded <= index.encoded_len());
                        if boundary < text.len() {
                            let range = index
                                .text_range_at(text, encoded)
                                .expect("encoded offset inside the buffer");
                            assert_eq!(range.start, boundary, "{text:?} at {boundary}");
                        } else {
                            assert_eq!(encoded, index.encoded_len());
                        }

                        let row = index.row_of_text_offset(text, boundary);
                        assert!(row < index.row_count(), "{text:?} at {boundary}");
                        // The row never starts after the grapheme it contains.
                        assert!(row * bytes_per_row <= encoded);
                    }

                    // Offsets inside a grapheme snap to its start.
                    for (start, end) in grapheme_ranges(text) {
                        let expected = index.encoded_offset_at_text(text, start);
                        for inside in (start + 1)..end {
                            assert_eq!(
                                index.encoded_offset_at_text(text, inside),
                                expected,
                                "{text:?} inside {start}..{end}",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn row_of_text_offset_clamps_the_end_of_buffer() {
        // Four UTF-8 bytes over two-byte rows: the end offset must not report a
        // phantom row past the last one.
        let text = "abcd";
        let index = HexIndex::build(text, Encoding::Utf8, 2).unwrap();
        assert_eq!(index.row_count(), 2);
        assert_eq!(index.row_of_text_offset(text, text.len()), 1);
        assert_eq!(index.row_of_text_offset(text, 0), 0);
    }

    #[test]
    fn unrepresentable_text_fails() {
        assert!(HexIndex::build("😀", Encoding::Latin1, 16).is_err());
        assert!(HexIndex::build("é", Encoding::Ascii, 16).is_err());
    }
}
