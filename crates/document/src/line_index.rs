//! Incremental byte-length line index.
//!
//! The document used to hold a `Vec<usize>` of absolute line-start offsets and
//! rebuild it from scratch — O(text length) — on every edit, so a single
//! keystroke cost the size of the whole buffer. This index instead stores each
//! line's **byte length** and keeps prefix sums in a Fenwick tree:
//!
//! - `line_of_offset`, `line_start`, and `line_range` are O(log lines);
//! - an edit confined to a single existing line is one O(log lines) point
//!   update;
//! - an edit spanning multiple lines, or one that adds or removes lines,
//!   re-splices the length vector and rebuilds the tree: O(lines) (a `Vec`
//!   splice) but never a rescan of the text bytes.
//!
//! Line semantics match the document invariant: every line but the last ends
//! with `\n`; a trailing `\n` produces a final empty line. A line's stored
//! length includes its trailing newline when present.

/// Per-line byte lengths plus a Fenwick tree of prefix sums.
pub(crate) struct LineIndex {
    lengths: Vec<u32>,
    /// 1-indexed Fenwick tree over `lengths`.
    tree: Vec<u32>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let lengths = split_line_lengths(text.as_bytes(), true);
        let tree = build_tree(&lengths);
        Self { lengths, tree }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.lengths.len()
    }

    /// Byte offset where `line` starts.
    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.prefix_sum(line) as usize
    }

    /// Offset just past `line`'s trailing newline: the start of the next line,
    /// or the end of the text for the last line.
    pub(crate) fn line_end_including_newline(&self, line: usize) -> usize {
        self.prefix_sum(line + 1) as usize
    }

    /// Byte range of `line`, excluding its trailing newline.
    pub(crate) fn line_range(&self, line: usize) -> (usize, usize) {
        let start = self.line_start(line);
        let length = self.lengths[line] as usize;
        // A non-final line's trailing newline is not part of its range; the
        // final line has no trailing newline in its range either.
        let content = if line + 1 < self.lengths.len() {
            length.saturating_sub(1)
        } else {
            length
        };
        (start, start + content)
    }

    /// Line containing `offset` (clamped to the last line).
    pub(crate) fn line_of_offset(&self, offset: usize) -> usize {
        let target = offset.min(u32::MAX as usize) as u32;
        self.lower_bound(target)
            .min(self.lengths.len().saturating_sub(1))
    }

    /// Update the index after `text` was mutated from `start..end` (old offsets)
    /// to the current `text` with `inserted_len` bytes of replacement.
    ///
    /// `text` must already be the post-edit string; this index still describes
    /// the pre-edit string. Only the lines touched by the edit are recomputed.
    pub(crate) fn apply_edit(&mut self, text: &str, start: usize, end: usize, inserted_len: usize) {
        let first_line = self.line_of_offset(start);
        let last_old_line = self.line_of_offset(end);
        let region_start = self.line_start(first_line);
        let last_old_line_end = self.line_end_including_newline(last_old_line);
        // The tail of `last_old_line` after `end` now sits after the inserted
        // text, so the edited region in the new text ends here.
        let region_end = start + inserted_len + (last_old_line_end - end);
        debug_assert!(region_start <= region_end && region_end <= text.len());

        // A trailing newline in the region only implies a final empty line when
        // the region replaces the document's last line. If a following line
        // already exists (e.g. the empty line after a trailing newline), that
        // line is outside the region and must not be duplicated.
        let final_region = last_old_line + 1 == self.line_count();
        let new_lengths =
            split_line_lengths(&text.as_bytes()[region_start..region_end], final_region);
        self.replace_lines(first_line, last_old_line - first_line + 1, &new_lengths);
    }

    /// Replace `removed` line lengths starting at `first` with `new_lengths`.
    fn replace_lines(&mut self, first: usize, removed: usize, new_lengths: &[u32]) {
        debug_assert!(removed >= 1 && first + removed <= self.lengths.len());
        debug_assert!(!new_lengths.is_empty());

        // The common typing case touches exactly one line: a point update keeps
        // the tree in sync without rebuiling it.
        if removed == 1 && new_lengths.len() == 1 {
            let old = self.lengths[first];
            let new = new_lengths[0];
            if old != new {
                self.lengths[first] = new;
                self.add(first, new as i64 - old as i64);
            }
            return;
        }

        self.lengths
            .splice(first..first + removed, new_lengths.iter().copied());
        self.tree = build_tree(&self.lengths);
    }

    fn prefix_sum(&self, end: usize) -> u32 {
        let mut sum = 0u32;
        let mut i = end;
        while i > 0 {
            sum += self.tree[i];
            i -= i & i.wrapping_neg();
        }
        sum
    }

    fn add(&mut self, index: usize, delta: i64) {
        let n = self.lengths.len();
        let mut i = index + 1;
        while i <= n {
            self.tree[i] = (self.tree[i] as i64 + delta) as u32;
            i += i & i.wrapping_neg();
        }
    }

    /// Largest position `p` in `0..=lines` with `prefix_sum(p) <= target`.
    fn lower_bound(&self, target: u32) -> usize {
        let n = self.lengths.len();
        let mut position = 0usize;
        let mut remaining = target;
        // Highest power of two <= n (n is always >= 1).
        let mut bit = 1usize << (usize::BITS - 1 - n.leading_zeros());
        while bit > 0 {
            let next = position + bit;
            if next <= n && self.tree[next] <= remaining {
                remaining -= self.tree[next];
                position = next;
            }
            bit >>= 1;
        }
        position
    }
}

/// Split `bytes` into line lengths, each including its trailing newline.
///
/// `final_region` distinguishes a region that runs to the end of the text (where
/// a trailing newline yields a final empty line) from a region that stops at a
/// line boundary in the middle (where the following line is outside the region
/// and must not be emitted).
fn split_line_lengths(bytes: &[u8], final_region: bool) -> Vec<u32> {
    let mut lengths = Vec::new();
    let mut length = 0u32;
    for &byte in bytes {
        length += 1;
        if byte == b'\n' {
            lengths.push(length);
            length = 0;
        }
    }
    if final_region || length > 0 {
        lengths.push(length);
    }
    if lengths.is_empty() {
        lengths.push(0);
    }
    lengths
}

fn build_tree(lengths: &[u32]) -> Vec<u32> {
    let n = lengths.len();
    let mut tree = vec![0u32; n + 1];
    tree[1..].copy_from_slice(lengths);
    for i in 1..=n {
        let parent = i + (i & i.wrapping_neg());
        if parent <= n {
            let value = tree[i];
            tree[parent] += value;
        }
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_starts(text: &str) -> Vec<usize> {
        let mut starts = vec![0];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(index + 1);
            }
        }
        starts
    }

    fn naive_line_of_offset(starts: &[usize], offset: usize) -> usize {
        match starts.partition_point(|&start| start <= offset) {
            0 => 0,
            count => count - 1,
        }
    }

    fn naive_line_range(text: &str, starts: &[usize], line: usize) -> (usize, usize) {
        let start = starts[line];
        let end = if line + 1 < starts.len() {
            starts[line + 1] - 1
        } else {
            text.len()
        };
        (start, end)
    }

    /// Assert the index agrees with a from-scratch reference over all queries.
    fn check(index: &LineIndex, text: &str) {
        let starts = naive_starts(text);
        assert_eq!(index.line_count(), starts.len(), "line_count for {text:?}");
        // Only the final line may be empty; interior zero-length lines would make
        // prefix sums non-increasing and break the Fenwick lower_bound.
        for (line, &length) in index.lengths.iter().enumerate() {
            if line + 1 < index.lengths.len() {
                assert!(length > 0, "interior line {line} is empty for {text:?}");
            }
        }
        for offset in 0..=text.len() {
            assert_eq!(
                index.line_of_offset(offset),
                naive_line_of_offset(&starts, offset),
                "line_of_offset({offset}) for {text:?}"
            );
        }
        for line in 0..starts.len() {
            assert_eq!(
                index.line_range(line),
                naive_line_range(text, &starts, line),
                "line_range({line}) for {text:?}"
            );
            assert_eq!(index.line_start(line), starts[line], "line_start({line})");
            let end_including_newline = if line + 1 < starts.len() {
                starts[line + 1]
            } else {
                text.len()
            };
            assert_eq!(
                index.line_end_including_newline(line),
                end_including_newline,
                "line_end_including_newline({line}) for {text:?}"
            );
        }
        assert_eq!(
            index.line_end_including_newline(starts.len() - 1),
            text.len(),
            "final line end for {text:?}"
        );
    }

    fn char_boundaries(text: &str) -> Vec<usize> {
        (0..=text.len())
            .filter(|&offset| text.is_char_boundary(offset))
            .collect()
    }

    /// Apply `start..end -> insert` to `text` (which must be the pre-edit string),
    /// update `index`, and assert it still matches a full rebuild.
    fn edit_checked(
        text: &mut String,
        index: &mut LineIndex,
        start: usize,
        end: usize,
        insert: &str,
    ) {
        let old = text.as_bytes();
        let mut bytes = Vec::with_capacity(old.len() + insert.len() - (end - start));
        bytes.extend_from_slice(&old[..start]);
        bytes.extend_from_slice(insert.as_bytes());
        bytes.extend_from_slice(&old[end..]);
        *text = String::from_utf8(bytes).expect("edit at char boundaries stays valid UTF-8");
        index.apply_edit(text, start, end, insert.len());
        check(index, text);
    }

    #[test]
    fn exhaustive_single_edits_match_rebuild() {
        let bases = [
            "",
            "a",
            "\n",
            "a\n",
            "\n\n",
            "\n\n\n",
            "ab",
            "a\nb",
            "a\n\nb",
            "a\nb\n",
            "a\n\nb\n",
            "x\n\n\n",
            "ab\ncd",
            "ab\ncd\n",
            "abc\ndef\nghi",
            "abc\ndef\nghi\n",
            "é\n😀\n",
            "a\nb\nc\nd\ne\n",
        ];
        let inserts = [
            "", "a", "\n", "\n\n", "x\n", "\na", "a\nb", "é", "😀", "\n\n\n",
        ];
        for base in bases {
            let boundaries = char_boundaries(base);
            for &start in &boundaries {
                for &end in boundaries.iter().filter(|&&end| end >= start) {
                    for insert in inserts {
                        let mut text = base.to_string();
                        let mut index = LineIndex::new(base);
                        edit_checked(&mut text, &mut index, start, end, insert);
                    }
                }
            }
        }
    }

    #[test]
    fn exhaustive_two_edits_match_rebuild() {
        // Replaying both edits on a fresh index every time keeps this test to a
        // pure state check: the second edit must not depend on stale tree state
        // left behind by the first (point update or splice).
        let bases = ["", "a", "\n", "a\n", "\n\n", "ab"];
        let inserts = ["", "a", "\n", "x\n", "\n\n"];
        for base in bases {
            let first = char_boundaries(base);
            for &start1 in &first {
                for &end1 in first.iter().filter(|&&end| end >= start1) {
                    for insert1 in inserts {
                        let mut once = base.to_string();
                        let mut once_index = LineIndex::new(base);
                        edit_checked(&mut once, &mut once_index, start1, end1, insert1);

                        let second = char_boundaries(&once);
                        for &start2 in &second {
                            for &end2 in second.iter().filter(|&&end| end >= start2) {
                                for insert2 in inserts {
                                    let mut text = base.to_string();
                                    let mut index = LineIndex::new(base);
                                    edit_checked(&mut text, &mut index, start1, end1, insert1);
                                    edit_checked(&mut text, &mut index, start2, end2, insert2);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn seeded_fuzz_matches_rebuild() {
        let bases = [
            "",
            "\n",
            "\n\n\n",
            "a\nb\n",
            "é\n😀\n\n",
            "line one\nline two\n\n",
            "x",
        ];
        let inserts = [
            "",
            "a",
            "b",
            " ",
            "\n",
            "\n\n",
            "x\n",
            "\né",
            "😀",
            "abc\n",
            "line\nline2\n",
        ];
        for (base_index, base) in bases.iter().enumerate() {
            for seed in 0..150u32 {
                let mut state = (seed
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(base_index as u32 + 1))
                    | 1;
                let mut rand = move || {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    state
                };

                let mut text = base.to_string();
                let mut index = LineIndex::new(&text);
                check(&index, &text);

                for _ in 0..50 {
                    let boundaries = char_boundaries(&text);
                    let a = boundaries[(rand() as usize) % boundaries.len()];
                    let b = boundaries[(rand() as usize) % boundaries.len()];
                    let (start, end) = if a <= b { (a, b) } else { (b, a) };
                    let insert = inserts[(rand() as usize) % inserts.len()];
                    edit_checked(&mut text, &mut index, start, end, insert);
                }
            }
        }
    }

    #[test]
    fn large_newline_heavy_document_matches_rebuild() {
        // A line count that is not a power of two exercises every Fenwick level,
        // and the trailing newline leaves a duplicated final prefix sum.
        let mut text = "line\n".repeat(200);
        let mut index = LineIndex::new(&text);
        check(&index, &text);

        let inserts = ["", "a", "\n", "\n\n", "xyz\n", "\n\n\n"];

        let mut state = 0x51ed_270bu32 | 1;
        let mut rand = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        for _ in 0..120 {
            let boundaries = char_boundaries(&text);
            let a = boundaries[(rand() as usize) % boundaries.len()];
            let b = boundaries[(rand() as usize) % boundaries.len()];
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let insert = inserts[(rand() as usize) % inserts.len()];
            edit_checked(&mut text, &mut index, start, end, insert);
        }
    }

    fn check_document(document: &crate::Document) {
        let text = document.text();
        let starts = naive_starts(text);
        assert_eq!(
            document.line_count(),
            starts.len(),
            "line_count for {text:?}"
        );
        for offset in 0..=text.len() {
            assert_eq!(
                document.line_of_offset(offset),
                naive_line_of_offset(&starts, offset),
                "document line_of_offset({offset}) for {text:?}"
            );
        }
        for line in 0..starts.len() {
            assert_eq!(
                document.line_range(line),
                naive_line_range(text, &starts, line),
                "document line_range({line}) for {text:?}"
            );
        }
    }

    #[test]
    fn document_edits_match_rebuild() {
        let bases = ["", "\n", "\n\n\n", "a\nb\n", "abc\ndef\n", "é\n😀\n", "x"];
        let inserts = ["", "a", "\n", "\n\n", "é", "😀", "x\ny", "line\n"];
        for (base_index, base) in bases.iter().enumerate() {
            let mut state = (base_index as u32 + 1).wrapping_mul(2_246_822_519) | 1;
            let mut rand = move || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state
            };

            let mut document = crate::Document::new(base.to_string());
            check_document(&document);
            for _ in 0..250 {
                let boundaries = char_boundaries(document.text());
                let a = boundaries[(rand() as usize) % boundaries.len()];
                let b = boundaries[(rand() as usize) % boundaries.len()];
                let (start, end) = if a <= b { (a, b) } else { (b, a) };
                let insert = inserts[(rand() as usize) % inserts.len()];
                document.replace_range(start, end, insert);
                check_document(&document);
            }
        }
    }

    #[test]
    fn fresh_index_matches_reference() {
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\n\nb",
            "\n\n",
            "abc\ndef\n",
            "a\r\nb",
            &"x".repeat(1000),
            &format!("{}\n", "line\n".repeat(500)),
        ] {
            let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
            check(&LineIndex::new(&cleaned), &cleaned);
        }
    }

    #[test]
    fn apply_edit_matches_rebuild_over_many_edits() {
        // Deterministic pseudo-random edits over ASCII (so every offset is a
        // char boundary), comparing the incrementally-updated index against a
        // full rebuild after each step.
        let inserts = ["", "a", "b", "\n", "x\n", "\n\n", "hello\nworld"];
        let mut state = 0x1234_5678u32;
        let mut rand = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };

        let mut text = String::from("alpha\nbeta\ngamma\ndelta\n");
        let mut index = LineIndex::new(&text);
        check(&index, &text);

        for _ in 0..400 {
            let len = text.len();
            let start = (rand() as usize) % (len + 1);
            let end = start + (rand() as usize) % (len - start + 1);
            let insert = inserts[(rand() as usize) % inserts.len()];

            text.replace_range(start..end, insert);
            index.apply_edit(&text, start, end, insert.len());
            check(&index, &text);
        }
    }

    #[test]
    fn point_update_path_survives_a_typing_run() {
        let mut text = String::new();
        let mut index = LineIndex::new(&text);
        for byte in "typing on a single line".bytes() {
            let at = text.len();
            text.push(byte as char);
            index.apply_edit(&text, at, at, 1);
        }
        check(&index, &text);
        assert_eq!(index.line_count(), 1);
    }
}
