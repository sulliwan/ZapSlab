//! Android/iOS localization string converters, ported from Boop's
//! `AndroidIOSStrings.js` and `IOSAndroidStrings.js`.
//!
//! Both scripts read and write `input.fullText`, so both use [`Scope::FullText`].

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Android Strings to iOS Localizables",
        description: "Convert Android <string name=\"k\">v</string> lines to \"k\" = \"v\"; only the first '>' on each line is rewritten.",
        tags: "string,android,ios",
        scope: Scope::FullText,
        apply: android_to_ios,
    },
    Transform {
        name: "iOS Localizables to Android Strings",
        description: "Convert \"k\" = \"v\"; lines to <string> entries; empty lines are dropped and missing quoted values become undefined.",
        tags: "string,android,ios",
        scope: Scope::FullText,
        apply: ios_to_android,
    },
];

/// JS stringifies a `undefined` capture group as `"undefined"` when the input
/// line has fewer than two quoted segments; mirror that instead of dropping the
/// line.
const JS_UNDEFINED: &str = "undefined";

/// `element.replace(...)` with a string pattern replaces only the first match,
/// and the three passes run in this order, so the first `>` seen after the tag
/// prefix is stripped is the one closing the `name` attribute.
fn android_to_ios(input: &str) -> TransformResult {
    let converted = input
        .split('\n')
        .map(|line| {
            let line = line.replacen("<string name=", "", 1);
            let line = line.replacen("</string>", "\";", 1);
            line.replacen(">", " = \"", 1)
        })
        .collect::<Vec<_>>()
        .join("\n");

    TransformResult::replace(converted)
}

fn ios_to_android(input: &str) -> TransformResult {
    let converted = input
        .split('\n')
        // Boop skips empty lines entirely rather than preserving them.
        .filter(|line| !line.is_empty())
        .map(|line| {
            let quoted = quoted_segments(line);
            let key = quoted.first().copied().unwrap_or(JS_UNDEFINED);
            let value = quoted.get(1).copied().unwrap_or(JS_UNDEFINED);
            format!("<string name=\"{key}\">{value}</string>")
        })
        .collect::<Vec<_>>()
        .join("\n");

    TransformResult::replace(converted)
}

/// Captured groups of JS `/"(.*?)"/g` against a single line.
///
/// The scanner advances past each closing quote, which reproduces the
/// non-greedy regex's left-to-right, non-overlapping matches without pulling in
/// a regex dependency. Escapes are not interpreted, matching the original.
fn quoted_segments(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut segments = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'"' {
            match line[index + 1..].find('"') {
                Some(offset) => {
                    let end = index + 1 + offset;
                    segments.push(&line[index + 1..end]);
                    index = end + 1;
                }
                None => break,
            }
        } else {
            index += 1;
        }
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected a replacement"),
        }
    }

    #[test]
    fn android_line_becomes_ios_localizable() {
        let input = r#"<string name="hello">world</string>"#;
        assert_eq!(text(android_to_ios(input)), r#""hello" = "world";"#);
    }

    #[test]
    fn android_conversion_keeps_line_structure() {
        // Every line goes through the three replacements, so non-string lines
        // are passed through untouched only when they contain no `>`.
        let input = concat!(
            "<string name=\"greeting\">Hello</string>\n",
            "plain text\n",
            "<string name=\"farewell\">Bye</string>"
        );
        assert_eq!(
            text(android_to_ios(input)),
            "\"greeting\" = \"Hello\";\nplain text\n\"farewell\" = \"Bye\";"
        );
    }

    #[test]
    fn android_rewrites_the_first_angle_bracket_on_every_line() {
        // Faithful to Boop: the `>` pass is unconditional, so an XML comment
        // loses its closing `>` to the ` = "` substitution.
        let input = "<!-- a comment -->";
        assert_eq!(text(android_to_ios(input)), "<!-- a comment -- = \"");
    }

    #[test]
    fn android_preserves_empty_lines() {
        let input = "<string name=\"a\">b</string>\n\n<string name=\"c\">d</string>";
        assert_eq!(text(android_to_ios(input)), "\"a\" = \"b\";\n\n\"c\" = \"d\";");
    }

    #[test]
    fn ios_line_becomes_android_string() {
        let input = r#""hello" = "world";"#;
        assert_eq!(text(ios_to_android(input)), r#"<string name="hello">world</string>"#);
    }

    #[test]
    fn ios_uses_first_two_quoted_segments() {
        let input = r#""key" = "value" ; // "note""#;
        assert_eq!(text(ios_to_android(input)), r#"<string name="key">value</string>"#);
    }

    #[test]
    fn ios_drops_empty_lines() {
        let input = "\"a\" = \"b\";\n\n\"c\" = \"d\";\n";
        assert_eq!(
            text(ios_to_android(input)),
            "<string name=\"a\">b</string>\n<string name=\"c\">d</string>"
        );
    }

    #[test]
    fn ios_fills_missing_segments_with_undefined() {
        assert_eq!(
            text(ios_to_android(r#""only-key""#)),
            r#"<string name="only-key">undefined</string>"#
        );
        assert_eq!(
            text(ios_to_android("no quotes here")),
            r#"<string name="undefined">undefined</string>"#
        );
    }

    #[test]
    fn quoted_segments_matches_non_greedy_regex() {
        assert_eq!(quoted_segments(r#""a" "b""#), vec!["a", "b"]);
        assert_eq!(quoted_segments(r#""""#), vec![""]);
        assert_eq!(quoted_segments("plain"), Vec::<&str>::new());
        // A dangling quote is not a match, same as `"(.*?)"` with no closer.
        assert_eq!(quoted_segments(r#"value "unfinished"#), Vec::<&str>::new());
    }
}
