use super::random::Rng;
use super::{Scope, Transform, TransformResult};
use unicode_segmentation::UnicodeSegmentation;

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Reverse String",
        description: "!seod ti tahw sseuG",
        tags: "flip,mirror,invert",
        scope: Scope::SelectionOrFullText,
        apply: reverse_string,
    },
    Transform {
        name: "Rot13",
        description: "Applies the Rot13 cypher to your text.",
        tags: "spoilers,encryption,plaintext",
        scope: Scope::SelectionOrFullText,
        apply: rot13,
    },
    Transform {
        name: "Trim",
        description: "Trims leading and trailing whitespace.",
        tags: "trim,whitespace,empty,space",
        scope: Scope::SelectionOrFullText,
        apply: trim,
    },
    Transform {
        name: "Lorem Ipsum",
        description: "Generates Lorem Ipsum placeholder text.",
        tags: "generate,lorem,ipsum,text",
        scope: Scope::SelectionOrFullText,
        apply: lorem_ipsum,
    },
];

/// The word pool used by Boop's Lorem Ipsum generator. The final entry is
/// unreachable there (see [`lorem_ipsum`]) but kept for parity.
const LOREM_WORDS: &[&str] = &[
    "ad",
    "adipisicing",
    "aliqua",
    "aliquip",
    "amet",
    "anim",
    "aute",
    "cillum",
    "commodo",
    "consectetur",
    "consequat",
    "culpa",
    "cupidatat",
    "deserunt",
    "do",
    "dolor",
    "dolore",
    "duis",
    "ea",
    "eiusmod",
    "elit",
    "enim",
    "esse",
    "est",
    "et",
    "eu",
    "ex",
    "excepteur",
    "exercitation",
    "fugiat",
    "id",
    "in",
    "incididunt",
    "ipsum",
    "irure",
    "labore",
    "laboris",
    "laborum",
    "Lorem",
    "magna",
    "minim",
    "mollit",
    "nisi",
    "non",
    "nostrud",
    "nulla",
    "occaecat",
    "officia",
    "pariatur",
    "proident",
    "qui",
    "quis",
    "reprehenderit",
    "sint",
    "sit",
    "sunt",
    "tempor",
    "ullamco",
    "ut",
    "velit",
    "veniam",
    "voluptate",
];

fn reverse_string(input: &str) -> TransformResult {
    // Reverse grapheme clusters so combining marks and emoji stay intact.
    let reversed: String = input.graphemes(true).rev().collect();
    TransformResult::replace(reversed)
}

fn rot13(input: &str) -> TransformResult {
    let out: String = input
        .chars()
        .map(|ch| match ch {
            'a'..='m' | 'A'..='M' => char::from(ch as u8 + 13),
            'n'..='z' | 'N'..='Z' => char::from(ch as u8 - 13),
            other => other,
        })
        .collect();
    TransformResult::replace(out)
}

fn trim(input: &str) -> TransformResult {
    TransformResult::replace(input.trim())
}

fn lorem_ipsum(_input: &str) -> TransformResult {
    let mut rng = Rng::new();
    let mut sentence = String::new();
    for _ in 0..100 {
        // Boop picks `random() * (len - 1)`, so the last word is never chosen.
        let word = LOREM_WORDS[rng.below(LOREM_WORDS.len() - 1)];
        sentence.push_str(word);
        sentence.push(' ');
    }

    // Mirrors `charAt(0).toUpperCase() + slice(1).trim() + "."`.
    let mut chars = sentence.chars();
    let first = chars
        .next()
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_default();
    let rest = chars.as_str().trim();
    TransformResult::replace(format!("{first}{rest}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            TransformResult::ReplaceAndReport { text, .. } => text,
            TransformResult::Report(message) | TransformResult::Error(message) => {
                panic!("unexpected non-replacement result: {message}")
            }
        }
    }

    #[test]
    fn reverses_graphemes_not_bytes() {
        assert_eq!(text(reverse_string("abc")), "cba");
        assert_eq!(text(reverse_string("a😀b")), "b😀a");
        // "e" plus a combining acute accent is a single grapheme.
        assert_eq!(text(reverse_string("e\u{301}x")), "xe\u{301}");
    }

    #[test]
    fn rot13_rotates_only_ascii_letters() {
        assert_eq!(text(rot13("Hello, World!")), "Uryyb, Jbeyq!");
        assert_eq!(text(rot13("abcXYZ")), "nopKLM");
        assert_eq!(text(rot13("café 123")), "pnsé 123");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(text(trim("  hello \n\t")), "hello");
        assert_eq!(text(trim("")), "");
    }

    #[test]
    fn lorem_ipsum_shape() {
        let out = text(lorem_ipsum("ignored"));
        assert_eq!(out.split_whitespace().count(), 100);
        assert!(out.ends_with('.'));
        assert!(out.chars().next().is_some_and(|c| c.is_uppercase()));
        assert!(!out.contains("  "));
        // The last word in the pool is never selected by Boop's generator.
        assert!(!out
            .split_whitespace()
            .any(|word| word.trim_end_matches('.') == "voluptate"));
    }
}
