//! Rendering of non-printing and invisible characters.
//!
//! When "show invisibles" is on, a cluster that would render as nothing (a
//! control code, a format character such as ZWJ or a zero-width space, or a
//! lone combining mark) is replaced by a visible escape sequence so it can be
//! inspected and edited. Printable clusters pass through untouched.
//!
//! Escape notation follows Rust-ish conventions:
//! - common controls use mnemonics (`\n`, `\t`, `\r`, `\0`);
//! - other ASCII/C1 controls use a byte escape (`\x18`);
//! - everything else uses a code point escape (`\u{200b}`).

use std::borrow::Cow;

use unicode_properties::general_category::{GeneralCategory, UnicodeGeneralCategory};

/// Whether `c` contributes no visible glyph on its own.
///
/// Combining marks are included because a cluster made only of marks (i.e. with
/// no base character) renders as nothing; a cluster that has a base is visible
/// as a whole and is never escaped.
pub fn is_invisible_char(c: char) -> bool {
    // A normal space occupies a cell, so it is "visible" for our purposes even
    // though it prints nothing. Other space separators (NBSP and friends) are
    // escaped so they can be told apart from a plain space.
    if c == ' ' {
        return false;
    }
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::SpaceSeparator
            | GeneralCategory::NonspacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// The visible escape sequence for a single non-printing character.
pub fn escape_char(c: char) -> String {
    match c {
        '\n' => "\\n".to_string(),
        '\t' => "\\t".to_string(),
        '\r' => "\\r".to_string(),
        '\0' => "\\0".to_string(),
        c if c.is_control() => {
            let scalar = c as u32;
            if scalar <= 0xFF {
                format!("\\x{scalar:02x}")
            } else {
                format!("\\u{{{scalar:x}}}")
            }
        }
        c => format!("\\u{{{:x}}}", c as u32),
    }
}

/// Escapes every code point in a cluster.
pub fn escape_cluster(cluster: &str) -> String {
    cluster.chars().map(escape_char).collect()
}

/// The display form of a cluster: its escape sequence when it is entirely
/// invisible and `escape_invisibles` is set, otherwise the cluster itself.
///
/// Returns whether the text was escaped so the view can color it differently.
pub fn display_for_cluster(cluster: &str, escape_invisibles: bool) -> (Cow<'_, str>, bool) {
    let invisible = !cluster.is_empty() && cluster.chars().all(is_invisible_char);
    if escape_invisibles && invisible {
        (Cow::Owned(escape_cluster(cluster)), true)
    } else {
        (Cow::Borrowed(cluster), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_controls_use_byte_escapes() {
        assert_eq!(escape_char('\u{18}'), "\\x18");
        assert_eq!(escape_char('\u{7f}'), "\\x7f");
    }

    #[test]
    fn whitespace_uses_mnemonics() {
        assert_eq!(escape_char('\n'), "\\n");
        assert_eq!(escape_char('\t'), "\\t");
        assert_eq!(escape_char('\r'), "\\r");
        assert_eq!(escape_char('\0'), "\\0");
    }

    #[test]
    fn non_ascii_invisibles_use_code_point_escapes() {
        assert_eq!(escape_char('\u{200b}'), "\\u{200b}");
        assert_eq!(escape_char('\u{fe0f}'), "\\u{fe0f}");
        assert_eq!(escape_char('\u{a0}'), "\\u{a0}");
    }

    #[test]
    fn printable_cluster_is_left_alone() {
        let (display, escaped) = display_for_cluster("e\u{301}", true);
        assert!(!escaped);
        assert_eq!(&*display, "e\u{301}");
    }

    #[test]
    fn lone_combining_mark_is_escaped() {
        let (display, escaped) = display_for_cluster("\u{301}", true);
        assert!(escaped);
        assert_eq!(&*display, "\\u{301}");
    }

    #[test]
    fn zwj_emoji_sequence_is_not_escaped() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let (_, escaped) = display_for_cluster(family, true);
        assert!(!escaped);
    }

    #[test]
    fn space_is_visible_but_non_breaking_space_is_not() {
        assert!(!display_for_cluster(" ", true).1);
        assert!(display_for_cluster("\u{a0}", true).1);
    }

    #[test]
    fn turning_the_setting_off_keeps_raw_text() {
        let (display, escaped) = display_for_cluster("\u{18}", false);
        assert!(!escaped);
        assert_eq!(&*display, "\u{18}");
    }
}
