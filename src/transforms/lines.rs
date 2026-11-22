//! Line-oriented transforms, ported from Boop's `Sort.js`, `NatSort.js`,
//! `ReverseLines.js`, `ShuffleLines.js`, `RemoveDuplicates.js`, `Collapse.js`
//! and `MarkdownQuote.js`.

use std::cmp::Ordering;
use std::collections::HashSet;

use super::random::Rng;
use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Sort lines",
        description: "Sort lines alphabetically.",
        tags: "sort,alphabet",
        scope: Scope::SelectionOrFullText,
        apply: sort_lines,
    },
    Transform {
        name: "Natural Sort Lines",
        description: "Sort lines with smart handling of numbers.",
        tags: "sort,natural,natsort",
        scope: Scope::SelectionOrFullText,
        apply: natural_sort_lines,
    },
    Transform {
        name: "Reverse Lines",
        description: "Flips every line of your text.",
        tags: "reverse,order,invert,mirror,flip,upside,down",
        scope: Scope::SelectionOrFullText,
        apply: reverse_lines,
    },
    Transform {
        name: "Shuffle Lines",
        description: "Randomize each line of your text.",
        tags: "shuffle,random",
        scope: Scope::SelectionOrFullText,
        apply: shuffle_lines,
    },
    Transform {
        name: "Remove Duplicate Lines",
        description: "Ensures each line of your text is unique.",
        tags: "unique,duplicate",
        scope: Scope::SelectionOrFullText,
        apply: remove_duplicate_lines,
    },
    Transform {
        name: "Collapse lines",
        description: "Removes all linebreaks from your text",
        tags: "strip,remove,collapse,join",
        // Boop writes this transform back to `state.fullText`, so it always
        // collapses the whole document even when a selection exists.
        scope: Scope::FullText,
        apply: collapse_lines,
    },
    Transform {
        name: "Markdown Quote",
        description: "Adds > to the start of every line of your text.",
        tags: "quote,markdown",
        scope: Scope::SelectionOrFullText,
        apply: markdown_quote,
    },
];

/// Sort lines and, like Boop, toggle to reverse order when the input is
/// already sorted.
///
/// Boop uses `localeCompare`, which is locale-sensitive; we use Rust's
/// Unicode string ordering instead (byte order, which for UTF-8 is code point
/// order). This is a deliberate deviation so the transform is deterministic
/// and dependency-free.
fn sort_lines(input: &str) -> TransformResult {
    TransformResult::replace(sort_with(input, |a, b| a.cmp(b)))
}

/// Natural/numeric-aware counterpart to [`sort_lines`].
fn natural_sort_lines(input: &str) -> TransformResult {
    TransformResult::replace(sort_with(input, natural_cmp))
}

/// Shared Boop sort body:
///
/// ```js
/// let sorted = input.text.replace(/\n$/, '').split('\n')
///     .sort(cmp).join('\n');
/// if (sorted === input.text) sorted = sorted.split('\n').reverse().join('\n');
/// ```
///
/// Note that a single trailing newline is dropped, and because the comparison
/// is against the *original* text (which may still end in `\n`), a trailing
/// newline also prevents the reverse toggle from firing.
fn sort_with(input: &str, cmp: impl Fn(&str, &str) -> Ordering) -> String {
    let body = input.strip_suffix('\n').unwrap_or(input);
    let mut lines: Vec<&str> = body.split('\n').collect();
    lines.sort_by(|a, b| cmp(a, b));
    let sorted = lines.join("\n");

    if sorted == input {
        sorted.split('\n').rev().collect::<Vec<_>>().join("\n")
    } else {
        sorted
    }
}

/// Compare two strings with digit runs compared numerically.
///
/// Boop passes `{numeric: true, sensitivity: 'base'}` to `localeCompare`;
/// this reproduces the numeric part and approximates `sensitivity: 'base'`
/// with a case-insensitive comparison of non-digit runs. Accent folding is
/// not reproduced.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_chars = a.chars().peekable();
    let mut b_chars = b.chars().peekable();

    loop {
        match (a_chars.peek().copied(), b_chars.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a_char), Some(b_char)) => {
                if a_char.is_ascii_digit() && b_char.is_ascii_digit() {
                    let a_run = take_digits(&mut a_chars);
                    let b_run = take_digits(&mut b_chars);
                    let ord = compare_numeric_runs(&a_run, &b_run);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let a_lower = a_char.to_lowercase().next().unwrap_or(a_char);
                    let b_lower = b_char.to_lowercase().next().unwrap_or(b_char);
                    if a_lower != b_lower {
                        return a_lower.cmp(&b_lower);
                    }
                    a_chars.next();
                    b_chars.next();
                }
            }
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(&c) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        chars.next();
    }
    digits
}

/// Compare digit runs without parsing them, so arbitrarily long numbers cannot
/// overflow. Leading zeros do not affect the value, matching `numeric: true`.
fn compare_numeric_runs(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn reverse_lines(input: &str) -> TransformResult {
    let mut lines: Vec<&str> = input.split('\n').collect();
    lines.reverse();
    TransformResult::replace(lines.join("\n"))
}

fn shuffle_lines(input: &str) -> TransformResult {
    let mut lines: Vec<&str> = input.split('\n').collect();
    Rng::new().shuffle(&mut lines);
    TransformResult::replace(lines.join("\n"))
}

fn remove_duplicate_lines(input: &str) -> TransformResult {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(lines.len());
    for line in &lines {
        if seen.insert(*line) {
            unique.push(*line);
        }
    }
    let removed = lines.len() - unique.len();
    TransformResult::replace_and_report(unique.join("\n"), format!("{removed} lines removed"))
}

fn collapse_lines(input: &str) -> TransformResult {
    let lines = split_any_newline(input);
    let collapsed = lines.join(",");
    TransformResult::replace_and_report(collapsed, format!("{} lines collapsed", lines.len()))
}

/// Split on `/\r\n|\r|\n/` (CRLF as one delimiter, not two), preserving empty
/// leading/trailing fields the way `String.prototype.split` does.
fn split_any_newline(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&input[start..i]);
                i += if bytes.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                start = i;
            }
            b'\n' => {
                lines.push(&input[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }

    lines.push(&input[start..]);
    lines
}

fn markdown_quote(input: &str) -> TransformResult {
    let quoted = input
        .split('\n')
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    TransformResult::replace(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected TransformResult::Replace"),
        }
    }

    #[test]
    fn sort_sorts_lines() {
        assert_eq!(replaced(sort_lines("b\na")), "a\nb");
    }

    #[test]
    fn sort_toggles_to_reverse_when_already_sorted() {
        assert_eq!(replaced(sort_lines("a\nb")), "b\na");
    }

    #[test]
    fn sort_strips_one_trailing_newline() {
        // The trailing newline is stripped before splitting, so it is absent
        // from the output and the input no longer compares equal.
        assert_eq!(replaced(sort_lines("b\na\n")), "a\nb");
    }

    #[test]
    fn natural_sort_orders_numbers_numerically() {
        assert_eq!(
            replaced(natural_sort_lines("file10\nfile2\nfile1")),
            "file1\nfile2\nfile10"
        );
    }

    #[test]
    fn natural_sort_toggles_to_reverse_when_already_sorted() {
        assert_eq!(
            replaced(natural_sort_lines("file1\nfile2\nfile10")),
            "file10\nfile2\nfile1"
        );
    }

    #[test]
    fn reverse_lines_flips_order() {
        assert_eq!(replaced(reverse_lines("a\nb\nc")), "c\nb\na");
    }

    #[test]
    fn reverse_lines_keeps_trailing_empty_line() {
        assert_eq!(replaced(reverse_lines("a\nb\n")), "\nb\na");
    }

    #[test]
    fn remove_duplicates_keeps_first_occurrence_and_reports_count() {
        match remove_duplicate_lines("a\nb\na\nc\nb") {
            TransformResult::ReplaceAndReport { text, message } => {
                assert_eq!(text, "a\nb\nc");
                assert_eq!(message, "2 lines removed");
            }
            _ => panic!("expected TransformResult::ReplaceAndReport"),
        }
    }

    #[test]
    fn collapse_joins_with_commas_and_reports_count() {
        match collapse_lines("a\r\nb\nc") {
            TransformResult::ReplaceAndReport { text, message } => {
                assert_eq!(text, "a,b,c");
                assert_eq!(message, "3 lines collapsed");
            }
            _ => panic!("expected TransformResult::ReplaceAndReport"),
        }
    }

    #[test]
    fn collapse_treats_crlf_as_one_delimiter() {
        assert!(matches!(
            collapse_lines("a\r\nb"),
            TransformResult::ReplaceAndReport { message, .. } if message == "2 lines collapsed"
        ));
    }

    #[test]
    fn markdown_quote_prefixes_every_line() {
        assert_eq!(replaced(markdown_quote("a\n\nb")), "> a\n> \n> b");
    }
}
