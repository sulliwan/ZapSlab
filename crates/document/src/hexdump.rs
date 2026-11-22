//! Byte layout for the hex panel.
//!
//! The panel shows the document text encoded with the selected [`Encoding`],
//! laid out in rows of `bytes_per_row` bytes. The row length is chosen by the
//! view from the available panel width, so the bytes flow to fill the pane.
//! Each displayed byte keeps the range of *text* bytes it came from, so
//! selection (which lives in text offsets) can be highlighted over the bytes.

use std::ops::Range;

use crate::encoding::{self, EncodeError, Encoding};
use crate::grapheme::grapheme_ranges;

/// One grapheme's encoded bytes.
pub struct HexSpan {
    /// Byte range in the document text.
    pub text_range: Range<usize>,
    /// Byte range in the encoded output.
    pub encoded_range: Range<usize>,
}

/// A single encoded byte in the hex columns.
pub struct HexCell {
    pub byte: u8,
    /// Index into [`HexLayout::spans`].
    pub span: usize,
    /// Absolute byte offset within the encoded stream.
    pub encoded_offset: usize,
}

pub struct HexRow {
    /// Encoded byte offset of the first byte in this row.
    pub encoded_offset: usize,
    pub cells: Vec<HexCell>,
}

pub struct HexLayout {
    pub spans: Vec<HexSpan>,
    pub rows: Vec<HexRow>,
}

impl HexLayout {
    /// Encodes `text` and lays it out into rows of `bytes_per_row` bytes. Fails
    /// when `encoding` cannot represent the text (e.g. Latin-1 with an emoji).
    pub fn build(
        text: &str,
        encoding: Encoding,
        bytes_per_row: usize,
    ) -> Result<Self, EncodeError> {
        let bytes_per_row = bytes_per_row.max(1);
        let mut spans = Vec::new();
        let mut encoded = Vec::new();
        let mut owner = Vec::new();

        for (start, end) in grapheme_ranges(text) {
            let bytes = encoding::encode(&text[start..end], encoding)?;
            let encoded_start = encoded.len();
            encoded.extend_from_slice(&bytes);
            let encoded_end = encoded.len();
            let span_index = spans.len();
            spans.push(HexSpan {
                text_range: start..end,
                encoded_range: encoded_start..encoded_end,
            });
            owner.extend(std::iter::repeat(span_index).take(bytes.len()));
        }

        let rows = build_rows(&encoded, &owner, bytes_per_row);
        Ok(Self { spans, rows })
    }
}

fn build_rows(encoded: &[u8], owner: &[usize], bytes_per_row: usize) -> Vec<HexRow> {
    if encoded.is_empty() {
        return vec![HexRow {
            encoded_offset: 0,
            cells: Vec::new(),
        }];
    }

    let mut rows = Vec::new();
    let mut start = 0;
    while start < encoded.len() {
        let end = (start + bytes_per_row).min(encoded.len());
        let cells = (start..end)
            .map(|index| HexCell {
                byte: encoded[index],
                span: owner[index],
                encoded_offset: index,
            })
            .collect();
        rows.push(HexRow {
            encoded_offset: start,
            cells,
        });
        start = end;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const BYTES: usize = 16;

    #[test]
    fn utf8_rows_map_bytes_back_to_text() {
        let layout = HexLayout::build("aé", Encoding::Utf8, BYTES).unwrap();
        assert_eq!(layout.spans.len(), 2);
        assert_eq!(layout.rows.len(), 1);
        let row = &layout.rows[0];
        assert_eq!(row.cells.len(), 3);
        assert_eq!(row.cells[0].byte, b'a');
        assert_eq!(row.cells[1].byte, 0xC3);
        assert_eq!(row.cells[2].byte, 0xA9);
        // Both UTF-8 bytes of `é` belong to the same two text bytes.
        assert_eq!(layout.spans[row.cells[1].span].text_range, 1..3);
        assert_eq!(layout.spans[row.cells[2].span].text_range, 1..3);
    }

    #[test]
    fn empty_buffer_is_one_empty_row() {
        let layout = HexLayout::build("", Encoding::Utf8, BYTES).unwrap();
        assert_eq!(layout.rows.len(), 1);
        assert!(layout.rows[0].cells.is_empty());
    }

    #[test]
    fn bytes_per_row_controls_wrapping_and_offsets() {
        let layout = HexLayout::build(&"x".repeat(20), Encoding::Utf8, 8).unwrap();
        assert_eq!(layout.rows.len(), 3);
        assert_eq!(layout.rows[0].cells.len(), 8);
        assert_eq!(layout.rows[1].encoded_offset, 8);
        assert_eq!(layout.rows[2].cells.len(), 4);
        assert_eq!(layout.rows[2].encoded_offset, 16);
    }

    #[test]
    fn latin1_encoding_is_one_byte_per_character() {
        let layout = HexLayout::build("é", Encoding::Latin1, BYTES).unwrap();
        assert_eq!(layout.rows[0].cells.len(), 1);
        assert_eq!(layout.rows[0].cells[0].byte, 0xE9);
    }

    #[test]
    fn unrepresentable_text_fails() {
        assert!(HexLayout::build("😀", Encoding::Latin1, BYTES).is_err());
    }
}
