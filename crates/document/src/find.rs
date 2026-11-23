//! Pure find/replace engine for the editor.
//!
//! Everything here is a function over `&str` returning byte offsets or
//! `String`; there are no gpui, wasm, or I/O dependencies, so search and
//! replacement can be unit-tested without a window or a GPU.
//!
//! Literal queries are regex-escaped and compiled through the same engine as
//! regex queries. That is deliberate: it gives literal search the same
//! Unicode-aware case folding as regex search (an ASCII-only lowercase pass
//! would miss `É`/`é`) and keeps one matching code path instead of two sets of
//! boundary rules. It also means metacharacters a user types in a literal query
//! (`1+1`, `(x)`) are matched verbatim after escaping.
//!
//! Matching uses [`fancy_regex`] rather than the `regex` crate so patterns may
//! use look-around (`(?=…)`, `(?<=…)`, `(?!…)`, `(?<!…)`) and backreferences,
//! which the finite-automaton `regex` engine cannot express. `fancy_regex` runs
//! those features on a backtracking VM, so a search can fail at *runtime* with a
//! backtrack/stack limit error even though the pattern is valid. Such failures
//! are reported as [`RUNTIME_ERROR_MESSAGE`] instead of being treated as "no
//! match"; the `Result` returns below exist for exactly that distinction.
//!
//! `multi_line` enables the regex `m` flag, so `^`/`$` match at line boundaries
//! instead of only the document ends. `.` still does not match `\n` — that would
//! be the separate `s` (dot-matches-newline) flag.
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
//! handed back by the regex engine are always valid UTF-8 boundaries.

use fancy_regex::{Regex, RegexBuilder};

/// User-facing message for a valid pattern that is too expensive to run against
/// the current text (backtrack or stack limit exceeded).
pub const RUNTIME_ERROR_MESSAGE: &str = "Regex too complex";

/// A compiled search query.
///
/// `expand` records whether `$name` replacement expansion applies: regex
/// queries use the engine's capture expansion, while literal queries treat the
/// replacement template verbatim.
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
///
/// `multi_line` maps to the regex `m` flag (`^`/`$` at line boundaries); it is
/// ignored for literal queries, whose metacharacters are escaped.
pub fn compile(
    query: &str,
    is_regex: bool,
    case_insensitive: bool,
    multi_line: bool,
) -> Result<Option<Matcher>, String> {
    if query.is_empty() {
        return Ok(None);
    }

    let pattern = if is_regex {
        query.to_string()
    } else {
        fancy_regex::escape(query).into_owned()
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multi_line)
        .build()
        .map_err(|err| err.to_string())?;

    Ok(Some(Matcher {
        regex,
        expand: is_regex,
        literal: (!is_regex && !case_insensitive).then(|| query.to_string()),
    }))
}

/// Maps a failure raised while *running* a compiled pattern to a user-facing
/// message. A backtrack/stack limit means the pattern is valid but too
/// expensive for this text, which is a different situation from a parse error,
/// so it gets its own wording.
fn runtime_message(err: fancy_regex::Error) -> String {
    match err {
        fancy_regex::Error::RuntimeError(_) => RUNTIME_ERROR_MESSAGE.to_string(),
        other => other.to_string(),
    }
}

/// First non-empty match whose start is >= `from`.
///
/// `from` is clamped to the end of the text and snapped down to a char
/// boundary, so callers may pass `usize::MAX` or an offset inside a multibyte
/// character without risking a panic.
///
/// The search always runs over the full `text` rather than a slice starting at
/// `from`, so look-behind can see the characters immediately before the search
/// start.
pub fn find_next(
    text: &str,
    matcher: &Matcher,
    from: usize,
) -> Result<Option<(usize, usize)>, String> {
    let mut pos = from.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }

    if let Some(literal) = &matcher.literal {
        let Some(offset) = text[pos..].find(literal.as_str()) else {
            return Ok(None);
        };
        let start = pos + offset;
        return Ok(Some((start, start + literal.len())));
    }

    while pos <= text.len() {
        let Some(m) = matcher
            .regex
            .find_from_pos(text, pos)
            .map_err(runtime_message)?
        else {
            return Ok(None);
        };
        if m.end() > m.start() {
            return Ok(Some((m.start(), m.end())));
        }

        // A zero-width match would make the next search return the same
        // position forever, so step past the start of this match.
        let start = m.start();
        if start >= text.len() {
            return Ok(None);
        }
        let Some(ch) = text[start..].chars().next() else {
            return Ok(None);
        };
        pos = start + ch.len_utf8();
    }

    Ok(None)
}

/// Last non-empty match whose end is <= `before`.
///
/// `before` is clamped to the end of the text and snapped down to a char
/// boundary. Case-sensitive literal queries use a `str::rfind` fast path;
/// regex and case-insensitive searches scan forward because there is no
/// native reverse regex API.
pub fn find_prev(
    text: &str,
    matcher: &Matcher,
    before: usize,
) -> Result<Option<(usize, usize)>, String> {
    let mut limit = before.min(text.len());
    // Snap down to a char boundary so the literal fast path can slice. Match
    // offsets are always char boundaries, so `end <= limit` is unchanged.
    while limit > 0 && !text.is_char_boundary(limit) {
        limit -= 1;
    }
    if let Some(literal) = &matcher.literal {
        let Some(offset) = text[..limit].rfind(literal.as_str()) else {
            return Ok(None);
        };
        return Ok(Some((offset, offset + literal.len())));
    }
    let mut found = None;

    for m in matcher.regex.find_iter(text) {
        let m = m.map_err(runtime_message)?;
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

    Ok(found)
}

/// Replacement text for the match at `range`.
///
/// In regex mode this expands `$name`, `${name}`, `$1`, `${1}`, and `$$` with
/// the regex engine's capture expansion. In literal mode the template is
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
) -> Result<String, String> {
    if !matcher.expand {
        return Ok(template.to_string());
    }

    let (start, end) = range;
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return Ok(template.to_string());
    }

    // Search from `start` over the full text so a look-behind anchored at the
    // match start still sees the preceding characters.
    let Some(caps) = matcher
        .regex
        .captures_from_pos(text, start)
        .map_err(runtime_message)?
    else {
        return Ok(template.to_string());
    };
    let Some(m) = caps.get(0) else {
        return Ok(template.to_string());
    };
    if m.start() != start || m.end() != end {
        return Ok(template.to_string());
    }

    let mut out = String::new();
    // The expanded text is written into `out`; the return value (if any) only
    // reports whether the template was well-formed, which callers do not need.
    caps.expand(template, &mut out);
    Ok(out)
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
/// re-checked in that case). A runtime backtrack failure is returned as `Err`
/// so the caller can surface it instead of silently reporting "no matches".
pub fn replace_all(
    text: &str,
    matcher: &Matcher,
    template: &str,
    max_len: usize,
) -> Result<ReplaceAll, String> {
    let mut out = String::new();
    let mut last = 0;
    let mut replaced = false;

    for caps in matcher.regex.captures_iter(text) {
        let caps = caps.map_err(runtime_message)?;
        let Some(m) = caps.get(0) else {
            continue;
        };
        let (start, end) = (m.start(), m.end());
        if end <= start {
            continue;
        }

        out.push_str(&text[last..start]);
        if matcher.expand {
            caps.expand(template, &mut out);
        } else {
            out.push_str(template);
        }
        last = end;
        replaced = true;

        if out.len() > max_len {
            return Ok(ReplaceAll::OverLimit);
        }
    }

    if !replaced {
        return Ok(ReplaceAll::Unchanged);
    }

    out.push_str(&text[last..]);
    if out.len() > max_len {
        return Ok(ReplaceAll::OverLimit);
    }
    Ok(ReplaceAll::Replaced(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(query: &str) -> Matcher {
        compile(query, false, false, false)
            .expect("literal query must compile")
            .expect("query must be non-empty")
    }

    fn re(query: &str) -> Matcher {
        compile(query, true, false, false)
            .expect("regex query must compile")
            .expect("query must be non-empty")
    }

    fn re_multi(query: &str) -> Matcher {
        compile(query, true, false, true)
            .expect("multiline regex must compile")
            .expect("query must be non-empty")
    }

    fn next(text: &str, m: &Matcher, from: usize) -> Option<(usize, usize)> {
        find_next(text, m, from).expect("search must succeed")
    }

    fn prev(text: &str, m: &Matcher, before: usize) -> Option<(usize, usize)> {
        find_prev(text, m, before).expect("search must succeed")
    }

    fn replace(text: &str, m: &Matcher, template: &str, max_len: usize) -> ReplaceAll {
        replace_all(text, m, template, max_len).expect("replacement must succeed")
    }

    #[test]
    fn empty_query_yields_no_matcher() {
        assert!(
            compile("", false, false, false)
                .expect("empty literal compiles")
                .is_none()
        );
        assert!(
            compile("", true, false, false)
                .expect("empty regex compiles")
                .is_none()
        );
    }

    #[test]
    fn invalid_regex_reports_an_error() {
        let err = compile("(", true, false, false).expect_err("invalid regex must be rejected");
        assert!(!err.is_empty());

        // The same text is a valid literal query once escaped.
        assert!(
            compile("(", false, false, false)
                .expect("escaped literal compiles")
                .is_some()
        );
    }

    #[test]
    fn literal_finds_forward_and_backward() {
        let text = "one two one";
        let m = lit("one");

        assert_eq!(next(text, &m, 0), Some((0, 3)));
        assert_eq!(next(text, &m, 1), Some((8, 11)));
        assert_eq!(next(text, &m, 4), Some((8, 11)));
        assert_eq!(next(text, &m, 8), Some((8, 11)));
        assert_eq!(next(text, &m, 11), None);
        assert_eq!(next(text, &m, text.len() + 50), None);

        assert_eq!(prev(text, &m, text.len()), Some((8, 11)));
        assert_eq!(prev(text, &m, 9), Some((0, 3)));
        assert_eq!(prev(text, &m, 8), Some((0, 3)));
        assert_eq!(prev(text, &m, 3), Some((0, 3)));
        assert_eq!(prev(text, &m, 2), None);
        assert_eq!(prev(text, &m, 0), None);
        assert_eq!(prev(text, &m, text.len() + 50), Some((8, 11)));
    }

    #[test]
    fn case_sensitive_literal_backward_search_matches_existing_semantics() {
        let m = lit("one");
        let text = "one two one";
        assert_eq!(prev(text, &m, text.len()), Some((8, 11)));
        assert_eq!(prev(text, &m, 9), Some((0, 3)));
        assert_eq!(prev(text, &m, 8), Some((0, 3)));
        assert_eq!(prev(text, &m, 3), Some((0, 3)));
        assert_eq!(prev(text, &m, 2), None);
        // A non-boundary `before` must not panic and must snap safely.
        let accented = "aaaé";
        let mid = accented.len() - 1; // inside the two-byte é
        assert_eq!(prev(accented, &lit("a"), mid), Some((2, 3)));
    }

    #[test]
    fn case_insensitive_literal_still_uses_regex_folding() {
        let m = compile("é", false, true, false)
            .expect("literal compiles")
            .expect("non-empty");
        assert_eq!(prev("aÉb", &m, "aÉb".len()), Some((1, 3)));
        assert_eq!(next("aÉb", &m, 0), Some((1, 3)));
    }

    #[test]
    fn literal_not_found() {
        let m = lit("zzz");
        let text = "abc";

        assert_eq!(next(text, &m, 0), None);
        assert_eq!(prev(text, &m, text.len()), None);
        assert_eq!(replace(text, &m, "X", 0), ReplaceAll::Unchanged);
    }

    #[test]
    fn literal_query_treats_metacharacters_literally() {
        let m = lit("1+1=(2)");
        let text = "x 1+1=(2) y";
        let start = text.find("1+1=(2)").expect("substring is present");
        assert_eq!(next(text, &m, 0), Some((start, start + "1+1=(2)".len())));
    }

    #[test]
    fn case_sensitivity() {
        let sensitive = lit("abc");
        let insensitive = compile("abc", false, true, false)
            .expect("literal compiles")
            .expect("non-empty");
        let text = "ABC abc";
        assert_eq!(next(text, &sensitive, 0), Some((4, 7)));
        assert_eq!(next(text, &insensitive, 0), Some((0, 3)));

        let rx_sensitive = re("a+");
        let rx_insensitive = compile("a+", true, true, false)
            .expect("regex compiles")
            .expect("non-empty");
        assert_eq!(next("AAA", &rx_sensitive, 0), None);
        assert_eq!(next("AAA", &rx_insensitive, 0), Some((0, 3)));
    }

    #[test]
    fn case_insensitive_is_unicode_aware() {
        let m = compile("é", false, true, false)
            .expect("literal compiles")
            .expect("non-empty");
        assert_eq!(next("É", &m, 0), Some((0, 2)));
    }

    #[test]
    fn named_and_numbered_capture_expansion() {
        let m = re(r"(?P<first>\w+) (?P<last>\w+)");
        let text = "John Doe";
        let range = next(text, &m, 0).expect("match exists");
        assert_eq!(range, (0, 8));

        assert_eq!(
            replacement_for(text, &m, range, "$last, $first").expect("expand"),
            "Doe, John"
        );
        assert_eq!(
            replacement_for(text, &m, range, "${last}-${first}").expect("expand"),
            "Doe-John"
        );
        assert_eq!(
            replacement_for(text, &m, range, "$2 $1").expect("expand"),
            "Doe John"
        );
        assert_eq!(replacement_for(text, &m, range, "$$").expect("expand"), "$");
    }

    #[test]
    fn replacement_lines_up_with_a_later_match() {
        let m = re(r"\w+");
        let text = "one two";
        assert_eq!(
            replacement_for(text, &m, (4, 7), "[$0]").expect("expand"),
            "[two]"
        );
    }

    #[test]
    fn literal_mode_does_not_expand_dollar() {
        let m = lit("cat");
        let text = "cat";
        let range = next(text, &m, 0).expect("match exists");

        assert_eq!(
            replacement_for(text, &m, range, "$0 ${0} $name").expect("literal"),
            "$0 ${0} $name"
        );
        // The range is ignored entirely in literal mode.
        assert_eq!(
            replacement_for(text, &m, (999, 1000), "$0").expect("literal"),
            "$0"
        );
        assert_eq!(
            replace(text, &m, "$0", 100),
            ReplaceAll::Replaced("$0".to_string())
        );
    }

    #[test]
    fn replacement_falls_back_when_range_does_not_line_up() {
        let m = re(r"\w+");
        let text = "abc def";

        // A match exists at 0..3, so a shorter range must not be expanded.
        assert_eq!(
            replacement_for(text, &m, (0, 2), "$0!").expect("fallback"),
            "$0!"
        );
        // Out-of-bounds ranges must not panic.
        assert_eq!(
            replacement_for(text, &m, (999, 1000), "$0!").expect("fallback"),
            "$0!"
        );
        assert_eq!(
            replacement_for(text, &m, (5, 3), "$0!").expect("fallback"),
            "$0!"
        );
    }

    #[test]
    fn replace_all_literal_and_regex() {
        let text = "a b a";
        assert_eq!(
            replace(text, &lit("a"), "x", 100),
            ReplaceAll::Replaced("x b x".to_string())
        );

        let rx = re(r"(\w+)");
        assert_eq!(
            replace(text, &rx, "<$1>", 100),
            ReplaceAll::Replaced("<a> <b> <a>".to_string())
        );
    }

    #[test]
    fn replace_all_respects_max_len() {
        let text = "abc";
        let m = lit("b");

        assert_eq!(
            replace(text, &m, "XYZ", 5),
            ReplaceAll::Replaced("aXYZc".to_string())
        );
        assert_eq!(replace(text, &m, "XYZ", 4), ReplaceAll::OverLimit);

        // No matches: unchanged, even when the input itself is over the cap.
        assert_eq!(replace(text, &lit("z"), "XYZ", 0), ReplaceAll::Unchanged);
    }

    #[test]
    fn zero_width_matches_are_skipped() {
        let m = re("a*");
        let text = "bbb";
        assert_eq!(next(text, &m, 0), None);
        assert_eq!(prev(text, &m, text.len()), None);
        assert_eq!(replace(text, &m, "X", 100), ReplaceAll::Unchanged);

        let mixed = "ba";
        assert_eq!(next(mixed, &m, 0), Some((1, 2)));
        assert_eq!(prev(mixed, &m, mixed.len()), Some((1, 2)));
        assert_eq!(
            replace(mixed, &m, "X", 100),
            ReplaceAll::Replaced("bX".to_string())
        );
    }

    #[test]
    fn zero_width_anchor_terminates() {
        let m = re("^");
        assert_eq!(next("abc", &m, 0), None);
        assert_eq!(prev("abc", &m, 3), None);
        assert_eq!(replace("abc", &m, "X", 100), ReplaceAll::Unchanged);
    }

    #[test]
    fn multibyte_offsets_are_char_boundaries() {
        let text = "héllo 😀 world";
        let emoji_len = '😀'.len_utf8();
        let emoji = text.find('😀').expect("emoji is present");
        let m = lit("😀");

        let range = next(text, &m, emoji).expect("match exists");
        assert_eq!(range, (emoji, emoji + emoji_len));
        assert!(text.is_char_boundary(range.0));
        assert!(text.is_char_boundary(range.1));

        // A start inside the emoji snaps down to its start.
        assert_eq!(next(text, &m, emoji + 1), Some((emoji, emoji + emoji_len)));

        // Regex replacement over the emoji keeps whole characters intact.
        let rx = re("😀");
        let range = next(text, &rx, 0).expect("match exists");
        assert_eq!(
            replacement_for(text, &rx, range, "[$0]").expect("expand"),
            "[😀]"
        );

        // A mid-boundary range falls back instead of panicking.
        assert_eq!(
            replacement_for(text, &rx, (emoji + 1, emoji + 2), "[$0]").expect("fallback"),
            "[$0]"
        );

        assert_eq!(
            replace(text, &rx, "!", 100),
            ReplaceAll::Replaced("héllo ! world".to_string())
        );
    }

    #[test]
    fn lookaround_compiles() {
        // The `regex` engine rejects these outright; the fancy engine accepts
        // them.
        for pattern in [r"foo(?=bar)", r"foo(?!bar)", r"(?<=foo)bar", r"(?<!foo)bar"] {
            assert!(
                compile(pattern, true, false, false)
                    .expect("look-around must compile")
                    .is_some(),
                "pattern {pattern} must compile"
            );
        }
    }

    #[test]
    fn lookahead_and_lookbehind() {
        let text = "say foo and foobar";

        let ahead = re(r"foo(?=bar)");
        assert_eq!(next(text, &ahead, 0), Some((12, 15)));

        let behind = re(r"(?<=say )foo");
        assert_eq!(next(text, &behind, 0), Some((4, 7)));
        assert_eq!(prev(text, &behind, text.len()), Some((4, 7)));

        let negative = re(r"foo(?!bar)");
        assert_eq!(next(text, &negative, 0), Some((4, 7)));

        let negative_behind = re(r"(?<!say )foo");
        assert_eq!(next(text, &negative_behind, 0), Some((12, 15)));
    }

    #[test]
    fn case_insensitive_with_lookaround_is_unicode_aware() {
        // The main literal is still delegated to the regex engine's Unicode case
        // folding even when a look-ahead forces the backtracking VM.
        let m = compile("é(?=x)", true, true, false)
            .expect("regex compiles")
            .expect("non-empty");
        assert_eq!(next("Éx", &m, 0), Some((0, 2)));
    }

    #[test]
    fn lookbehind_sees_text_before_search_start() {
        // Searching a slice starting at `from` would lose the preceding `a`;
        // the engine must run over the full text so look-behind works.
        let m = re(r"(?<=a)b");
        assert_eq!(next("ab", &m, 0), Some((1, 2)));
        assert_eq!(next("ab", &m, 1), Some((1, 2)));

        let text = "ab ab";
        assert_eq!(next(text, &m, 0), Some((1, 2)));
        assert_eq!(next(text, &m, 2), Some((4, 5)));
    }

    #[test]
    fn backreferences_are_supported() {
        let m = re(r"(\w+) \1");
        let text = "mirror mirror on the wall";
        assert_eq!(next(text, &m, 0), Some((0, 13)));
        assert_eq!(prev(text, &m, text.len()), Some((0, 13)));
        assert_eq!(
            replacement_for(text, &m, (0, 13), "[$1]").expect("expand"),
            "[mirror]"
        );
    }

    #[test]
    fn multi_line_anchors_match_each_line() {
        let text = "foo\nbar\nfoo";
        let m = re_multi(r"^foo$");

        assert_eq!(next(text, &m, 0), Some((0, 3)));
        assert_eq!(next(text, &m, 1), Some((8, 11)));
        assert_eq!(prev(text, &m, text.len()), Some((8, 11)));
        assert_eq!(prev(text, &m, 3), Some((0, 3)));
        assert_eq!(
            replace(text, &m, "X", 100),
            ReplaceAll::Replaced("X\nbar\nX".to_string())
        );
    }

    #[test]
    fn multi_line_off_anchors_only_document_ends() {
        let text = "foo\nbar\nfoo";
        let m = re(r"^foo$");

        // Without `m`, `^`/`$` only match the document boundaries, so the
        // first line's "foo" is not a full-text match.
        assert_eq!(next(text, &m, 0), None);
        assert_eq!(prev(text, &m, text.len()), None);
        assert_eq!(replace(text, &m, "X", 100), ReplaceAll::Unchanged);
    }

    #[test]
    fn runtime_errors_surface_as_too_complex() {
        let err = fancy_regex::Error::RuntimeError(fancy_regex::RuntimeError::StackOverflow);
        assert_eq!(runtime_message(err), RUNTIME_ERROR_MESSAGE);

        // A tiny backtrack limit makes a real search fail deterministically,
        // proving the error propagates out of `find_next` instead of being
        // swallowed as "no match". `(a+)\1` needs a backreference (so the
        // backtracking VM runs) and backtracks several times before matching.
        let regex = RegexBuilder::new(r"(a+)\1")
            .backtrack_limit(1)
            .build()
            .expect("pattern compiles");
        let matcher = Matcher {
            regex,
            expand: true,
            literal: None,
        };
        let err = find_next("aaaaaaaaaaaaaaaaaaaa", &matcher, 0)
            .expect_err("the backtrack limit must be hit");
        assert_eq!(err, RUNTIME_ERROR_MESSAGE);
    }
}
