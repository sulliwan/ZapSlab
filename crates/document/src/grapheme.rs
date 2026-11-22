//! Grapheme cluster boundaries.
//!
//! The editor treats a "character" as a Unicode extended grapheme cluster: a
//! base code point plus any combining marks, emoji ZWJ sequences, regional
//! indicator pairs (flags), and Hangul jamo sequences. Stepping by grapheme
//! instead of by scalar value is what makes arrow keys and backspace behave the
//! way users expect: one press removes a whole `é` or family emoji, not just
//! the last code point.

use unicode_segmentation::UnicodeSegmentation;

/// Byte ranges of each extended grapheme cluster in `text`, in order.
pub fn grapheme_ranges(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| (start, start + grapheme.len()))
}

/// Start of the grapheme cluster before `offset`.
///
/// If `offset` is already a boundary this is the previous boundary; if it falls
/// inside a cluster it snaps down to that cluster's start.
pub fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut boundary = 0;
    for (start, end) in grapheme_ranges(text) {
        if end >= offset {
            return start;
        }
        boundary = end;
    }
    boundary
}

/// End of the grapheme cluster after `offset`.
///
/// If `offset` is already a boundary this is the next boundary; if it falls
/// inside a cluster it snaps forward to that cluster's end.
pub fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    for (_, end) in grapheme_ranges(text) {
        if end > offset {
            return end;
        }
    }
    text.len()
}

/// Largest grapheme boundary at or before `offset` (clamped to the text).
pub fn snap_to_grapheme(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut boundary = 0;
    for (_, end) in grapheme_ranges(text) {
        if end > offset {
            break;
        }
        boundary = end;
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACUTE: &str = "e\u{301}";

    #[test]
    fn boundaries_respect_combining_marks() {
        let text = ACUTE;
        // 0 and 3 are the only boundaries; byte 1 sits inside the cluster.
        assert_eq!(previous_grapheme_boundary(text, 3), 0);
        assert_eq!(previous_grapheme_boundary(text, 1), 0);
        assert_eq!(next_grapheme_boundary(text, 0), 3);
        assert_eq!(next_grapheme_boundary(text, 1), 3);
    }

    #[test]
    fn snap_only_moves_down_to_a_boundary() {
        assert_eq!(snap_to_grapheme(ACUTE, 0), 0);
        assert_eq!(snap_to_grapheme(ACUTE, 1), 0);
        assert_eq!(snap_to_grapheme(ACUTE, 3), 3);
        assert_eq!(snap_to_grapheme(ACUTE, usize::MAX), 3);
    }

    #[test]
    fn flag_is_a_single_cluster() {
        let flag = "\u{1F1FA}\u{1F1F8}"; // regional indicators U, S
        assert_eq!(grapheme_ranges(flag).count(), 1);
        assert_eq!(next_grapheme_boundary(flag, 0), flag.len());
    }

    #[test]
    fn zwj_sequence_is_a_single_cluster() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(grapheme_ranges(family).count(), 1);
        assert_eq!(previous_grapheme_boundary(family, family.len()), 0);
    }

    #[test]
    fn empty_text_has_no_clusters() {
        assert_eq!(grapheme_ranges("").count(), 0);
        assert_eq!(previous_grapheme_boundary("", 0), 0);
        assert_eq!(next_grapheme_boundary("", 0), 0);
        assert_eq!(snap_to_grapheme("", 10), 0);
    }
}
