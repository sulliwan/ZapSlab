use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Count Characters",
        description: "Report the UTF-16 code-unit length (astral characters count as two); the text is left unchanged.",
        tags: "count,length,size,character",
        scope: Scope::SelectionOrFullText,
        apply: count_characters,
    },
    Transform {
        name: "Count Lines",
        description: "Report the line count (a trailing newline adds a final empty line); the text is left unchanged.",
        tags: "count,length,size,line",
        scope: Scope::SelectionOrFullText,
        apply: count_lines,
    },
    Transform {
        name: "Count Words",
        description: "Report the number of whitespace-separated words; the text is left unchanged.",
        tags: "count,length,size,words",
        scope: Scope::SelectionOrFullText,
        apply: count_words,
    },
];

fn count_characters(input: &str) -> TransformResult {
    // Mirrors JS string length: UTF-16 code units, so astral characters count 2.
    TransformResult::report(format!("{} characters", input.encode_utf16().count()))
}

fn count_lines(input: &str) -> TransformResult {
    // JS `split('\n').length` is one more than the number of newlines.
    TransformResult::report(format!("{} lines", input.split('\n').count()))
}

fn count_words(input: &str) -> TransformResult {
    TransformResult::report(format!("{} words", input.split_whitespace().count()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(result: TransformResult) -> String {
        match result {
            TransformResult::Report(message) => message,
            TransformResult::Error(message) => message,
            TransformResult::Replace(_)
            | TransformResult::ReplaceAndReport { .. } => {
                panic!("expected a report, got a replacement")
            }
        }
    }

    #[test]
    fn counts_characters_as_utf16_units() {
        assert_eq!(message(count_characters("abc")), "3 characters");
        assert_eq!(message(count_characters("😀")), "2 characters");
        assert_eq!(message(count_characters("")), "0 characters");
    }

    #[test]
    fn counts_lines_including_trailing_newline() {
        assert_eq!(message(count_lines("a\nb")), "2 lines");
        assert_eq!(message(count_lines("a\n")), "2 lines");
        assert_eq!(message(count_lines("")), "1 lines");
    }

    #[test]
    fn counts_whitespace_separated_words() {
        assert_eq!(message(count_words("  hello   world \n")), "2 words");
        assert_eq!(message(count_words("   \n\t")), "0 words");
    }
}
