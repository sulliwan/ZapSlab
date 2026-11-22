//! Grapheme cluster boundaries.
//!
//! The editor treats a "character" as a Unicode extended grapheme cluster: a
//! base code point plus any combining marks, emoji ZWJ sequences, regional
//! indicator pairs (flags), and Hangul jamo sequences. Stepping by grapheme
//! instead of by scalar value is what makes arrow keys and backspace behave the
//! way users expect: one press removes a whole `é` or family emoji, not just
//! the last code point.
//!
//! Stepping is done locally with [`GraphemeCursor`] so the cost is proportional
//! to the cluster being stepped over rather than to `text.len()`. Every entry
//! point keeps a full-scan fallback for the (never expected) case where the
//! cursor cannot answer without more context.

use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete, UnicodeSegmentation};

/// Byte ranges of each extended grapheme cluster in `text`, in order.
pub fn grapheme_ranges(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| (start, start + grapheme.len()))
}

/// Largest offset at or below `offset` that lies on a UTF-8 char boundary.
///
/// [`GraphemeCursor`] slices the text at its offset, so it requires a char
/// boundary even though callers may pass any byte offset. A UTF-8 sequence is
/// at most four bytes, so this walks back at most three steps.
fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Largest grapheme boundary at or before the char boundary `at`.
///
/// `at` must be a char boundary. The start and end of the text are always
/// boundaries, which lets us answer those cases without asking the cursor
/// (where the start would request pre-context that does not exist and the end
/// would be rejected as out of range).
fn largest_boundary_at_or_before(text: &str, at: usize) -> Result<usize, GraphemeIncomplete> {
    debug_assert!(text.is_char_boundary(at));
    if at == 0 || at == text.len() {
        return Ok(at);
    }
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    if cursor.is_boundary(text, 0)? {
        Ok(at)
    } else {
        // `at` sits inside a cluster: step back to its start.
        let mut cursor = GraphemeCursor::new(at, text.len(), true);
        Ok(cursor.prev_boundary(text, 0)?.unwrap_or(0))
    }
}

/// Start of the grapheme cluster before `offset`.
///
/// If `offset` is already a boundary this is the previous boundary; if it falls
/// inside a cluster it snaps down to that cluster's start.
pub fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let at = floor_char_boundary(text, offset);
    let found = if at == offset {
        // A boundary at `at`: the answer is the boundary before it. Walking the
        // cursor from `at` only visits the preceding cluster.
        let mut cursor = GraphemeCursor::new(at, text.len(), true);
        cursor
            .prev_boundary(text, 0)
            .map(|boundary| boundary.unwrap_or(0))
    } else {
        // `offset` is inside a multi-byte char, so no grapheme boundary can
        // lie between `at` and `offset`: the answer is the largest boundary at
        // or before `at`.
        largest_boundary_at_or_before(text, at)
    };
    match found {
        Ok(boundary) => boundary,
        Err(_) => previous_grapheme_boundary_scan(text, offset),
    }
}

/// End of the grapheme cluster after `offset`.
///
/// If `offset` is already a boundary this is the next boundary; if it falls
/// inside a cluster it snaps forward to that cluster's end.
pub fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    // No boundary can hide inside a multi-byte char, so stepping from the char
    // boundary at or below `offset` finds the same next boundary.
    let at = floor_char_boundary(text, offset);
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    match cursor.next_boundary(text, 0) {
        Ok(Some(boundary)) => boundary,
        Ok(None) => text.len(),
        Err(_) => next_grapheme_boundary_scan(text, offset),
    }
}

/// Largest grapheme boundary at or before `offset` (clamped to the text).
pub fn snap_to_grapheme(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let at = floor_char_boundary(text, offset);
    match largest_boundary_at_or_before(text, at) {
        Ok(boundary) => boundary,
        Err(_) => snap_to_grapheme_scan(text, offset),
    }
}

// ---- Full-scan fallbacks ---------------------------------------------------
//
// These are the original O(offset) implementations. They stay private as a
// correctness safety net: the cursor path should answer without needing more
// context, but if `GraphemeCursor` ever reports `GraphemeIncomplete` we fall
// back instead of returning a wrong boundary.

fn previous_grapheme_boundary_scan(text: &str, offset: usize) -> usize {
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

fn next_grapheme_boundary_scan(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    for (_, end) in grapheme_ranges(text) {
        if end > offset {
            return end;
        }
    }
    text.len()
}

fn snap_to_grapheme_scan(text: &str, offset: usize) -> usize {
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

    // ---- Full-scan references, written independently of the cursor path ----

    fn scan_previous(text: &str, offset: usize) -> usize {
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

    fn scan_next(text: &str, offset: usize) -> usize {
        let offset = offset.min(text.len());
        for (_, end) in grapheme_ranges(text) {
            if end > offset {
                return end;
            }
        }
        text.len()
    }

    fn scan_snap(text: &str, offset: usize) -> usize {
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

    fn corpus() -> Vec<(&'static str, &'static str)> {
        vec![
            ("empty", ""),
            ("ascii", "abc"),
            ("latin", "héllo"),
            ("combining", "e\u{301}"),
            ("zwj family", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
            ("flag", "\u{1F1FA}\u{1F1F8}"),
            ("hangul", "\u{1100}\u{1161}\u{11A8}\u{1100}\u{1161}"),
            ("emoji", "\u{1F600}"),
            (
                "mixed",
                "a\u{301}b\u{1F600}\u{1F1FA}\u{1F1F8}\u{1100}\u{1161}c\n",
            ),
        ]
    }

    #[test]
    fn local_stepping_matches_full_scan_at_every_offset() {
        for (name, text) in corpus() {
            // Include a couple of offsets past the end so clamping is covered.
            for offset in 0..=(text.len() + 2) {
                assert_eq!(
                    previous_grapheme_boundary(text, offset),
                    scan_previous(text, offset),
                    "previous_grapheme_boundary at {offset} in {name}"
                );
                assert_eq!(
                    next_grapheme_boundary(text, offset),
                    scan_next(text, offset),
                    "next_grapheme_boundary at {offset} in {name}"
                );
                assert_eq!(
                    snap_to_grapheme(text, offset),
                    scan_snap(text, offset),
                    "snap_to_grapheme at {offset} in {name}"
                );
            }
        }
    }

    #[test]
    fn local_stepping_matches_scan_fallbacks() {
        // The fallbacks are the previous implementation; the cursor path must
        // agree with them on every offset too.
        for (name, text) in corpus() {
            for offset in 0..=(text.len() + 2) {
                assert_eq!(
                    previous_grapheme_boundary(text, offset),
                    previous_grapheme_boundary_scan(text, offset),
                    "previous fallback at {offset} in {name}"
                );
                assert_eq!(
                    next_grapheme_boundary(text, offset),
                    next_grapheme_boundary_scan(text, offset),
                    "next fallback at {offset} in {name}"
                );
                assert_eq!(
                    snap_to_grapheme(text, offset),
                    snap_to_grapheme_scan(text, offset),
                    "snap fallback at {offset} in {name}"
                );
            }
        }
    }

    #[test]
    fn offsets_inside_a_multibyte_char_step_by_cluster() {
        // `é` spans bytes 1..3, so offset 2 is inside the character but on no
        // char boundary. It must behave like any other in-cluster offset.
        let text = "aé";
        assert_eq!(previous_grapheme_boundary(text, 2), 1);
        assert_eq!(next_grapheme_boundary(text, 2), 3);
        assert_eq!(snap_to_grapheme(text, 2), 1);

        // Offset 1 of a combining sequence is a char boundary inside a cluster.
        let text = ACUTE;
        assert_eq!(previous_grapheme_boundary(text, 1), 0);
        assert_eq!(next_grapheme_boundary(text, 1), text.len());
        assert_eq!(snap_to_grapheme(text, 1), 0);
        // Offset 2 is inside the combining mark's two bytes.
        assert_eq!(previous_grapheme_boundary(text, 2), 0);
        assert_eq!(next_grapheme_boundary(text, 2), text.len());
        assert_eq!(snap_to_grapheme(text, 2), 0);
    }

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
