//! Display-aware text layout.
//!
//! When invisibles are escaped, the on-screen width of a grapheme is no longer
//! the width of its glyph: `\x18` occupies four cells. Soft wrapping, caret x,
//! and hit-testing therefore have to work in *display* width, not glyph width.
//!
//! This module keeps that math pure. The view injects a `width_of` function so
//! the same code can be exercised in tests with a synthetic metric.

use std::borrow::Cow;

use crate::escape::display_for_cluster;
use crate::grapheme::grapheme_ranges;

/// A grapheme plus the text that should actually be drawn for it.
pub struct DisplayGrapheme<'a> {
    /// Byte range within the text passed to [`display_graphemes`].
    pub text_range: (usize, usize),
    /// The cluster itself, or an escape sequence such as `\x18`.
    pub display: Cow<'a, str>,
    /// Whether `display` is an escape (draw it in the non-printing color).
    pub escaped: bool,
}

/// Iterates the display form of each grapheme in `text`.
pub fn display_graphemes(
    text: &str,
    escape_invisibles: bool,
) -> impl Iterator<Item = DisplayGrapheme<'_>> {
    grapheme_ranges(text).map(move |(start, end)| {
        let (display, escaped) = display_for_cluster(&text[start..end], escape_invisibles);
        DisplayGrapheme {
            text_range: (start, end),
            display,
            escaped,
        }
    })
}

/// Splits `text[start..end]` (a single source line) into wrapped chunks at
/// grapheme boundaries whose display width fits `wrap_width`.
///
/// Always returns at least one chunk, so an empty line stays an empty row.
pub fn wrap_line<F: Fn(&str) -> f32>(
    text: &str,
    start: usize,
    end: usize,
    wrap_width: f32,
    escape_invisibles: bool,
    width_of: &F,
) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut chunk_start = start;
    let mut chunk_width = 0.0f32;

    for grapheme in display_graphemes(&text[start..end], escape_invisibles) {
        let grapheme_start = start + grapheme.text_range.0;
        let width = width_of(&grapheme.display);
        if chunk_width > 0.0 && chunk_width + width > wrap_width {
            rows.push((chunk_start, grapheme_start));
            chunk_start = grapheme_start;
            chunk_width = 0.0;
        }
        chunk_width += width;
    }

    rows.push((chunk_start, end));
    rows
}

/// Display width from the start of the row to `offset`.
///
/// An offset inside a grapheme is treated as the start of that grapheme, so the
/// result never lands in the middle of a drawn escape.
pub fn x_in_row<F: Fn(&str) -> f32>(
    text: &str,
    start: usize,
    offset: usize,
    escape_invisibles: bool,
    width_of: &F,
) -> f32 {
    let mut x = 0.0f32;
    if start >= offset {
        return x;
    }
    for grapheme in display_graphemes(&text[start..], escape_invisibles) {
        let grapheme_start = start + grapheme.text_range.0;
        if grapheme_start >= offset {
            break;
        }
        let grapheme_end = start + grapheme.text_range.1;
        if grapheme_end <= offset {
            x += width_of(&grapheme.display);
        } else {
            // The cursor is inside this cluster; report the position at its
            // start rather than after its (possibly escaped) display text.
            break;
        }
    }
    x
}

/// Byte offset nearest to display x `target_x` within `text[start..end]`.
pub fn offset_for_x_in_row<F: Fn(&str) -> f32>(
    text: &str,
    start: usize,
    end: usize,
    target_x: f32,
    escape_invisibles: bool,
    width_of: &F,
) -> usize {
    let mut x = 0.0f32;
    for grapheme in display_graphemes(&text[start..end], escape_invisibles) {
        let width = width_of(&grapheme.display);
        if target_x < x + width * 0.5 {
            return start + grapheme.text_range.0;
        }
        x += width;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per displayed character, so an escape like `\x18` is 4 wide.
    fn unit_width(display: &str) -> f32 {
        display.chars().count() as f32
    }

    #[test]
    fn wrapping_uses_display_width_for_escapes() {
        // Four escaped controls are four cells wide each, but one glyph wide raw.
        let text = "\u{18}\u{18}\u{18}\u{18}";
        let rows = wrap_line(text, 0, text.len(), 5.0, true, &unit_width);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn wrapping_without_escapes_uses_glyph_width() {
        let text = "abcd";
        let rows = wrap_line(text, 0, text.len(), 2.0, false, &unit_width);
        assert_eq!(rows, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn empty_line_is_one_row() {
        assert_eq!(wrap_line("", 0, 0, 10.0, true, &unit_width), vec![(0, 0)]);
    }

    #[test]
    fn x_in_row_accounts_for_escape_width() {
        let text = "a\u{18}b";
        let after_a = 1;
        let after_control = 2;
        assert_eq!(x_in_row(text, 0, after_a, true, &unit_width), 1.0);
        assert_eq!(x_in_row(text, 0, after_control, true, &unit_width), 5.0);
    }

    #[test]
    fn offset_round_trips_at_grapheme_starts() {
        let text = "a\u{18}b";
        let target = x_in_row(text, 0, 2, true, &unit_width);
        assert_eq!(
            offset_for_x_in_row(text, 0, text.len(), target, true, &unit_width),
            2
        );
        assert_eq!(
            offset_for_x_in_row(text, 0, text.len(), 0.0, true, &unit_width),
            0
        );
        assert_eq!(
            offset_for_x_in_row(text, 0, text.len(), 100.0, true, &unit_width),
            text.len()
        );
    }
}
