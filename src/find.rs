//! Pure find/replace engine for the editor.
//!
//! Everything here is a function over `&str` returning byte offsets or
//! `String`; there are no gpui, wasm, or I/O dependencies, so search and
//! replacement can be unit-tested without a window or a GPU.
//!
//! Literal queries are regex-escaped and compiled through the same
//! [`regex::Regex`] as regex queries. That is deliberate: it gives literal
//! search the same Unicode-aware case folding as regex search (an ASCII-only
//! lowercase pass would miss `É`/`é`) and keeps one matching code path instead
//! of two sets of boundary rules. It also means metacharacters a user types in
//! a literal query (`1+1`, `(x)`) are matched verbatim after escaping.
//!
//! Case-sensitive literal backward search uses `str::rfind`; case-insensitive
//! literals and regexes scan matches (there is no native reverse regex API).
//!
//! Zero-width matches are skipped in navigation and replacement. A pattern such
//! as `a*` matches the empty string at every position; returning those would
//! make "find next" never advance and "replace all" insert between every
//! character forever. Skipping them keeps navigation monotonic and replacement
//! finite.
//!
//! Offsets are clamped and snapped to `str::is_char_boundary` before they reach
//! the regex API, which panics on out-of-bounds or mid-character starts. Ranges
//! handed back by the regex crate are always valid UTF-8 boundaries.

use regex::{Regex, RegexBuilder};

/// A compiled search query.
///
/// `expand` records whether `$name` replacement expansion applies: regex
/// queries use the regex crate's capture expansion, while literal queries
/// treat the replacement template verbatim.
#[derive(Debug)]
pub struct Matcher {
    regex: Regex,
    expand: bool,
    /// Raw query for case-sensitive literal searches, enabling `str::find` /
    /// `str::rfind` fast paths. `None` for regex queries and case-insensitive
    /// literals, which need the regex engine's Unicode case folding.
    literal: Option<String>,
}

/// Compiles a query.
///
/// Returns `Ok(None)` for an empty query (nothing to search for),
/// `Ok(Some(matcher))` otherwise, and `Err(message)` when `is_regex` is true
/// and the pattern is invalid. Invalid literal queries cannot occur because
/// they are escaped before compilation.
pub fn compile(
    query: &str,
    is_regex: bool,
    case_insensitive: bool,
) -> Result<Option<Matcher>, String> {
    if query.is_empty() {
        return Ok(None);
    }

    let pattern = if is_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|err| err.to_string())?;

    Ok(Some(Matcher {
        regex,
        expand: is_regex,
        literal: (!is_regex && !case_insensitive).then(|| query.to_string()),
    }))
}

/// First non-empty match whose start is >= `from`.
///
/// `from` is clamped to the end of the text and snapped down to a char
/// boundary, so callers may pass `usize::MAX` or an offset inside a multibyte
/// character without risking a panic.
pub fn find_next(text: &str, matcher: &Matcher, from: usize) -> Option<(usize, usize)> {
    let mut pos = from.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }

    if let Some(literal) = &matcher.literal {
        let offset = text[pos..].find(literal.as_str())?;
        let start = pos + offset;
        return Some((start, start + literal.len()));
    }

    while pos <= text.len() {
        let m = matcher.regex.find_at(text, pos)?;
        if m.end() > m.start() {
            return Some((m.start(), m.end()));
        }

        // A zero-width match would make the next `find_at` return the same
        // position forever, so step past the start of this match.
        let start = m.start();
        if start >= text.len() {
            return None;
        }
        let Some(ch) = text[start..].chars().next() else {
            return None;
        };
        pos = start + ch.len_utf8();
    }

    None
}

/// Last non-empty match whose end is <= `before`.
///
/// `before` is clamped to the end of the text and snapped down to a char
/// boundary. Case-sensitive literal queries use a `str::rfind` fast path;
/// regex and case-insensitive searches scan forward because there is no
/// native reverse regex API.
pub fn find_prev(text: &str, matcher: &Matcher, before: usize) -> Option<(usize, usize)> {
    let mut limit = before.min(text.len());
    // Snap down to a char boundary so the literal fast path can slice. Match
    // offsets are always char boundaries, so `end <= limit` is unchanged.
    while limit > 0 && !text.is_char_boundary(limit) {
        limit -= 1;
    }
    if let Some(literal) = &matcher.literal {
        let offset = text[..limit].rfind(literal.as_str())?;
        return Some((offset, offset + literal.len()));
    }
    let mut found = None;

    for m in matcher.regex.find_iter(text) {
        let (start, end) = (m.start(), m.end());
        if end <= start {
            continue;
        }
        // Matches are non-overlapping, so once a match starts at or after the
        // limit, every later match does too and none can end before it.
        if start >= limit {
            break;
        }
        if end <= limit {
            found = Some((start, end));
        }
    }

    found
}

/// Replacement text for the match at `range`.
///
/// In regex mode this expands `$name`, `${name}`, `$1`, `${1}`, and `$$` with
/// the regex crate's capture expansion. In literal mode the template is
/// returned verbatim, so a `$` is just a character.
///
/// If `range` is out of bounds, sits on a non-boundary, or no longer lines up
/// with a match at that offset (for example because the text changed since the
/// match was found), the template is returned verbatim rather than panicking.
pub fn replacement_for(
    text: &str,
    matcher: &Matcher,
    range: (usize, usize),
    template: &str,
) -> String {
    if !matcher.expand {
        return template.to_string();
    }

    let (start, end) = range;
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return template.to_string();
    }

    let Some(caps) = matcher.regex.captures_at(text, start) else {
        return template.to_string();
    };
    let Some(m) = caps.get(0) else {
        return template.to_string();
    };
    if m.start() != start || m.end() != end {
        return template.to_string();
    }

    let mut out = String::new();
    // The expanded text is written into `out`; the return value (if any) only
    // reports whether the template was well-formed, which callers do not need.
    let _ = caps.expand(template, &mut out);
    out
}

/// Outcome of [`replace_all`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceAll {
    /// The replacement would push the result past `max_len`.
    OverLimit,
    /// No non-empty match exists, so the text is unchanged.
    Unchanged,
    /// The fully replaced text.
    Replaced(String),
}

/// Replaces every non-empty match in `text`.
///
/// Returns [`ReplaceAll::OverLimit`] as soon as the growing result exceeds
/// `max_len`, so the caller can refuse before mutating a document with a hard
/// size cap. When no non-empty match exists it returns
/// [`ReplaceAll::Unchanged`] without copying the input (the caller's cap is not
/// re-checked in that case).
pub fn replace_all(text: &str, matcher: &Matcher, template: &str, max_len: usize) -> ReplaceAll {
    let mut out = String::new();
    let mut last = 0;
    let mut replaced = false;

    for caps in matcher.regex.captures_iter(text) {
        let Some(m) = caps.get(0) else {
            continue;
        };
        let (start, end) = (m.start(), m.end());
        if end <= start {
            continue;
        }

        out.push_str(&text[last..start]);
        if matcher.expand {
            let _ = caps.expand(template, &mut out);
        } else {
            out.push_str(template);
        }
        last = end;
        replaced = true;

        if out.len() > max_len {
            return ReplaceAll::OverLimit;
        }
    }

    if !replaced {
        return ReplaceAll::Unchanged;
    }

    out.push_str(&text[last..]);
    if out.len() > max_len {
        return ReplaceAll::OverLimit;
    }
    ReplaceAll::Replaced(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(query: &str) -> Matcher {
        compile(query, false, false)
            .expect("literal query must compile")
            .expect("query must be non-empty")
    }

    fn re(query: &str) -> Matcher {
        compile(query, true, false)
            .expect("regex query must compile")
            .expect("query must be non-empty")
    }

    #[test]
    fn empty_query_yields_no_matcher() {
        assert!(
            compile("", false, false)
                .expect("empty literal compiles")
                .is_none()
        );
        assert!(
            compile("", true, false)
                .expect("empty regex compiles")
                .is_none()
        );
    }

    #[test]
    fn invalid_regex_reports_an_error() {
        let err = compile("(", true, false).expect_err("invalid regex must be rejected");
        assert!(!err.is_empty());

        // The same text is a valid literal query once escaped.
        assert!(
            compile("(", false, false)
                .expect("escaped literal compiles")
                .is_some()
        );
    }

    #[test]
    fn literal_finds_forward_and_backward() {
        let text = "one two one";
        let m = lit("one");

        assert_eq!(find_next(text, &m, 0), Some((0, 3)));
        assert_eq!(find_next(text, &m, 1), Some((8, 11)));
        assert_eq!(find_next(text, &m, 4), Some((8, 11)));
        assert_eq!(find_next(text, &m, 8), Some((8, 11)));
        assert_eq!(find_next(text, &m, 11), None);
        assert_eq!(find_next(text, &m, text.len() + 50), None);

        assert_eq!(find_prev(text, &m, text.len()), Some((8, 11)));
        assert_eq!(find_prev(text, &m, 9), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 8), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 3), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 2), None);
        assert_eq!(find_prev(text, &m, 0), None);
        assert_eq!(find_prev(text, &m, text.len() + 50), Some((8, 11)));
    }

    #[test]
    fn case_sensitive_literal_backward_search_matches_existing_semantics() {
        let m = lit("one");
        let text = "one two one";
        assert_eq!(find_prev(text, &m, text.len()), Some((8, 11)));
        assert_eq!(find_prev(text, &m, 9), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 8), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 3), Some((0, 3)));
        assert_eq!(find_prev(text, &m, 2), None);
        // A non-boundary `before` must not panic and must snap safely.
        let accented = "aaaé";
        let mid = accented.len() - 1; // inside the two-byte é
        assert_eq!(find_prev(accented, &lit("a"), mid), Some((2, 3)));
    }

    #[test]
    fn case_insensitive_literal_still_uses_regex_folding() {
        let m = compile("é", false, true)
            .expect("literal compiles")
            .expect("non-empty");
        assert_eq!(find_prev("aÉb", &m, "aÉb".len()), Some((1, 3)));
        assert_eq!(find_next("aÉb", &m, 0), Some((1, 3)));
    }

    #[test]
    fn literal_not_found() {
        let m = lit("zzz");
        let text = "abc";

        assert_eq!(find_next(text, &m, 0), None);
        assert_eq!(find_prev(text, &m, text.len()), None);
        assert_eq!(replace_all(text, &m, "X", 0), ReplaceAll::Unchanged);
    }

    #[test]
    fn literal_query_treats_metacharacters_literally() {
        let m = lit("1+1=(2)");
        let text = "x 1+1=(2) y";
        let start = text.find("1+1=(2)").expect("substring is present");
        assert_eq!(
            find_next(text, &m, 0),
            Some((start, start + "1+1=(2)".len()))
        );
    }

    #[test]
    fn case_sensitivity() {
        let sensitive = lit("abc");
        let insensitive = compile("abc", false, true)
            .expect("literal compiles")
            .expect("non-empty");
        let text = "ABC abc";
        assert_eq!(find_next(text, &sensitive, 0), Some((4, 7)));
        assert_eq!(find_next(text, &insensitive, 0), Some((0, 3)));

        let rx_sensitive = re("a+");
        let rx_insensitive = compile("a+", true, true)
            .expect("regex compiles")
            .expect("non-empty");
        assert_eq!(find_next("AAA", &rx_sensitive, 0), None);
        assert_eq!(find_next("AAA", &rx_insensitive, 0), Some((0, 3)));
    }

    #[test]
    fn case_insensitive_is_unicode_aware() {
        let m = compile("é", false, true)
            .expect("literal compiles")
            .expect("non-empty");
        assert_eq!(find_next("É", &m, 0), Some((0, 2)));
    }

    #[test]
    fn named_and_numbered_capture_expansion() {
        let m = re(r"(?P<first>\w+) (?P<last>\w+)");
        let text = "John Doe";
        let range = find_next(text, &m, 0).expect("match exists");
        assert_eq!(range, (0, 8));

        assert_eq!(
            replacement_for(text, &m, range, "$last, $first"),
            "Doe, John"
        );
        assert_eq!(
            replacement_for(text, &m, range, "${last}-${first}"),
            "Doe-John"
        );
        assert_eq!(replacement_for(text, &m, range, "$2 $1"), "Doe John");
        assert_eq!(replacement_for(text, &m, range, "$$"), "$");
    }

    #[test]
    fn replacement_lines_up_with_a_later_match() {
        let m = re(r"\w+");
        let text = "one two";
        assert_eq!(replacement_for(text, &m, (4, 7), "[$0]"), "[two]");
    }

    #[test]
    fn literal_mode_does_not_expand_dollar() {
        let m = lit("cat");
        let text = "cat";
        let range = find_next(text, &m, 0).expect("match exists");

        assert_eq!(
            replacement_for(text, &m, range, "$0 ${0} $name"),
            "$0 ${0} $name"
        );
        // The range is ignored entirely in literal mode.
        assert_eq!(replacement_for(text, &m, (999, 1000), "$0"), "$0");
        assert_eq!(
            replace_all(text, &m, "$0", 100),
            ReplaceAll::Replaced("$0".to_string())
        );
    }

    #[test]
    fn replacement_falls_back_when_range_does_not_line_up() {
        let m = re(r"\w+");
        let text = "abc def";

        // A match exists at 0..3, so a shorter range must not be expanded.
        assert_eq!(replacement_for(text, &m, (0, 2), "$0!"), "$0!");
        // Out-of-bounds ranges must not panic.
        assert_eq!(replacement_for(text, &m, (999, 1000), "$0!"), "$0!");
        assert_eq!(replacement_for(text, &m, (5, 3), "$0!"), "$0!");
    }

    #[test]
    fn replace_all_literal_and_regex() {
        let text = "a b a";
        assert_eq!(
            replace_all(text, &lit("a"), "x", 100),
            ReplaceAll::Replaced("x b x".to_string())
        );

        let rx = re(r"(\w+)");
        assert_eq!(
            replace_all(text, &rx, "<$1>", 100),
            ReplaceAll::Replaced("<a> <b> <a>".to_string())
        );
    }

    #[test]
    fn replace_all_respects_max_len() {
        let text = "abc";
        let m = lit("b");

        assert_eq!(
            replace_all(text, &m, "XYZ", 5),
            ReplaceAll::Replaced("aXYZc".to_string())
        );
        assert_eq!(replace_all(text, &m, "XYZ", 4), ReplaceAll::OverLimit);

        // No matches: unchanged, even when the input itself is over the cap.
        assert_eq!(
            replace_all(text, &lit("z"), "XYZ", 0),
            ReplaceAll::Unchanged
        );
    }

    #[test]
    fn zero_width_matches_are_skipped() {
        let m = re("a*");
        let text = "bbb";
        assert_eq!(find_next(text, &m, 0), None);
        assert_eq!(find_prev(text, &m, text.len()), None);
        assert_eq!(replace_all(text, &m, "X", 100), ReplaceAll::Unchanged);

        let mixed = "ba";
        assert_eq!(find_next(mixed, &m, 0), Some((1, 2)));
        assert_eq!(find_prev(mixed, &m, mixed.len()), Some((1, 2)));
        assert_eq!(
            replace_all(mixed, &m, "X", 100),
            ReplaceAll::Replaced("bX".to_string())
        );
    }

    #[test]
    fn zero_width_anchor_terminates() {
        let m = re("^");
        assert_eq!(find_next("abc", &m, 0), None);
        assert_eq!(find_prev("abc", &m, 3), None);
        assert_eq!(replace_all("abc", &m, "X", 100), ReplaceAll::Unchanged);
    }

    #[test]
    fn multibyte_offsets_are_char_boundaries() {
        let text = "héllo 😀 world";
        let emoji_len = '😀'.len_utf8();
        let emoji = text.find('😀').expect("emoji is present");
        let m = lit("😀");

        let range = find_next(text, &m, emoji).expect("match exists");
        assert_eq!(range, (emoji, emoji + emoji_len));
        assert!(text.is_char_boundary(range.0));
        assert!(text.is_char_boundary(range.1));

        // A start inside the emoji snaps down to its start.
        assert_eq!(
            find_next(text, &m, emoji + 1),
            Some((emoji, emoji + emoji_len))
        );

        // Regex replacement over the emoji keeps whole characters intact.
        let rx = re("😀");
        let range = find_next(text, &rx, 0).expect("match exists");
        assert_eq!(replacement_for(text, &rx, range, "[$0]"), "[😀]");

        // A mid-boundary range falls back instead of panicking.
        assert_eq!(
            replacement_for(text, &rx, (emoji + 1, emoji + 2), "[$0]"),
            "[$0]"
        );

        assert_eq!(
            replace_all(text, &rx, "!", 100),
            ReplaceAll::Replaced("héllo ! world".to_string())
        );
    }
}
