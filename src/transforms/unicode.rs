//! Unicode normalization forms (NFC/NFD/NFKC/NFKD).
//!
//! Backed by the pure-Rust `unicode-normalization` crate, which implements the
//! four normalization forms from UAX #15 over Unicode scalar values, so
//! combining marks, ligatures, and compatibility characters are handled without
//! any surrogate arithmetic.

use super::{Scope, Transform, TransformResult};
use unicode_normalization::UnicodeNormalization;

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Unicode Normalize NFC",
        description: "Compose characters into canonical form (NFC); e + combining accent becomes the precomposed character.",
        tags: "unicode,normalize,nfc,compose,canonical,accent",
        scope: Scope::SelectionOrFullText,
        apply: normalize_nfc,
    },
    Transform {
        name: "Unicode Normalize NFD",
        description: "Decompose characters into canonical form (NFD); precomposed characters become base plus combining marks.",
        tags: "unicode,normalize,nfd,decompose,canonical,accent",
        scope: Scope::SelectionOrFullText,
        apply: normalize_nfd,
    },
    Transform {
        name: "Unicode Normalize NFKC",
        description: "Compatibility compose (NFKC); also converts ligatures, full-width, and other compatibility characters to their equivalents.",
        tags: "unicode,normalize,nfkc,compose,compatibility,ligature",
        scope: Scope::SelectionOrFullText,
        apply: normalize_nfkc,
    },
    Transform {
        name: "Unicode Normalize NFKD",
        description: "Compatibility decompose (NFKD); fully decomposes characters, including compatibility mappings such as ligatures and full-width forms.",
        tags: "unicode,normalize,nfkd,decompose,compatibility,ligature",
        scope: Scope::SelectionOrFullText,
        apply: normalize_nfkd,
    },
];

fn normalize_nfc(input: &str) -> TransformResult {
    TransformResult::replace(input.nfc().collect::<String>())
}

fn normalize_nfd(input: &str) -> TransformResult {
    TransformResult::replace(input.nfd().collect::<String>())
}

fn normalize_nfkc(input: &str) -> TransformResult {
    TransformResult::replace(input.nfkc().collect::<String>())
}

fn normalize_nfkd(input: &str) -> TransformResult {
    TransformResult::replace(input.nfkd().collect::<String>())
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
    fn registers_all_four_forms() {
        let names: Vec<&str> = TRANSFORMS.iter().map(|transform| transform.name).collect();
        assert_eq!(
            names,
            vec![
                "Unicode Normalize NFC",
                "Unicode Normalize NFD",
                "Unicode Normalize NFKC",
                "Unicode Normalize NFKD",
            ]
        );
        assert!(TRANSFORMS
            .iter()
            .all(|transform| transform.scope == Scope::SelectionOrFullText));
    }

    #[test]
    fn canonical_forms_compose_and_decompose() {
        // "e" + combining acute composes to U+00E9 under NFC.
        assert_eq!(text(normalize_nfc("e\u{301}")), "\u{e9}");
        // U+00E9 decomposes to "e" + combining acute under NFD.
        assert_eq!(text(normalize_nfd("\u{e9}")), "e\u{301}");
    }

    #[test]
    fn canonical_forms_leave_compatibility_characters_alone() {
        // Ligature and circled digit have no canonical decomposition.
        assert_eq!(text(normalize_nfc("\u{fb03}")), "\u{fb03}");
        assert_eq!(text(normalize_nfc("\u{2460}")), "\u{2460}");
    }

    #[test]
    fn compatibility_forms_apply_compatibility_mappings() {
        // U+FB03 LATIN SMALL LIGATURE FFI -> "ffi".
        assert_eq!(text(normalize_nfkc("\u{fb03}")), "ffi");
        assert_eq!(text(normalize_nfkd("\u{fb03}")), "ffi");
        // U+2460 CIRCLED DIGIT ONE -> "1".
        assert_eq!(text(normalize_nfkc("\u{2460}")), "1");
        assert_eq!(text(normalize_nfkd("\u{2460}")), "1");
        // U+FF21 FULLWIDTH LATIN CAPITAL LETTER A -> "A".
        assert_eq!(text(normalize_nfkc("\u{ff21}")), "A");
        assert_eq!(text(normalize_nfkd("\u{ff21}")), "A");
    }

    #[test]
    fn astral_characters_pass_through() {
        assert_eq!(text(normalize_nfc("\u{1f600}")), "\u{1f600}");
        assert_eq!(text(normalize_nfkc("\u{1f600}")), "\u{1f600}");
    }

    #[test]
    fn normalization_is_idempotent() {
        let input = "Cafe\u{301} \u{fb03}\u{2460}";
        let once = text(normalize_nfkc(input));
        assert_eq!(text(normalize_nfkc(&once)), once);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(text(normalize_nfc("")), "");
        assert_eq!(text(normalize_nfkd("")), "");
    }
}
