use super::random::Rng;
use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Camel Case",
        description: "Convert words to camelCase; splits on case, acronym, and punctuation boundaries and strips accents.",
        tags: "camel,case,function,lodash",
        scope: Scope::SelectionOrFullText,
        apply: camel_case,
    },
    Transform {
        name: "Kebab Case",
        description: "Convert words to kebab-case joined by hyphens; splits on case, acronym, and punctuation boundaries and strips accents.",
        tags: "kebab,case,function,lodash",
        scope: Scope::SelectionOrFullText,
        apply: kebab_case,
    },
    Transform {
        name: "Snake Case",
        description: "Convert words to snake_case joined by underscores; splits on case, acronym, and punctuation boundaries and strips accents.",
        tags: "snake,case,function,lodash",
        scope: Scope::SelectionOrFullText,
        apply: snake_case,
    },
    Transform {
        name: "Start Case",
        description: "Capitalize the first letter of every word and join them with spaces; splits on case and punctuation boundaries.",
        tags: "start,case,function,lodash",
        scope: Scope::SelectionOrFullText,
        apply: start_case,
    },
    Transform {
        name: "Sponge Case",
        description: "Randomly capitalize characters after the first, producing the 'mock' style; the first character never changes.",
        tags: "bob,sarcasm,no,this,is,patrick",
        scope: Scope::SelectionOrFullText,
        apply: sponge_case,
    },
    Transform {
        name: "Downcase",
        description: "Convert all characters to lowercase using Unicode case mapping.",
        tags: "downcase,lowercase",
        scope: Scope::SelectionOrFullText,
        apply: downcase,
    },
    Transform {
        name: "Upcase",
        description: "Convert all characters to uppercase using Unicode case mapping.",
        tags: "upcase,uppercase,capital,capitalize,capitalization",
        scope: Scope::SelectionOrFullText,
        apply: upcase,
    },
    Transform {
        name: "Deburr",
        description: "Replace accented and non-Latin characters with basic ASCII equivalents.",
        tags: "burr,special,characters,function,lodash",
        scope: Scope::SelectionOrFullText,
        apply: deburr,
    },
    Transform {
        name: "Replace Smart Quotes",
        description: "Replace curly single and double quotes with straight ASCII quotes.",
        tags: "smart,quotes,quotations,quotation,smart-quotes,smart-quotations",
        scope: Scope::SelectionOrFullText,
        apply: replace_smart_quotes,
    },
];

fn camel_case(input: &str) -> TransformResult {
    let words = words(input);
    let mut out = String::new();
    for (index, word) in words.iter().enumerate() {
        if index == 0 {
            out.push_str(&word.to_lowercase());
        } else {
            out.push_str(&upper_first(&word.to_lowercase()));
        }
    }
    TransformResult::replace(out)
}

fn kebab_case(input: &str) -> TransformResult {
    TransformResult::replace(join_lower(input, "-"))
}

fn snake_case(input: &str) -> TransformResult {
    TransformResult::replace(join_lower(input, "_"))
}

fn start_case(input: &str) -> TransformResult {
    let joined = words(input)
        .iter()
        .map(|word| upper_first(word))
        .collect::<Vec<_>>()
        .join(" ");
    TransformResult::replace(joined)
}

fn sponge_case(input: &str) -> TransformResult {
    let mut rng = Rng::new();
    let mut out = String::with_capacity(input.len());
    // Boop's loop runs from the end while `i > 0`, so the first character is
    // never touched; randomize every later one independently.
    for (index, ch) in input.chars().enumerate() {
        if index == 0 {
            out.push(ch);
        } else if rng.below(2) == 0 {
            out.extend(ch.to_lowercase());
        } else {
            out.extend(ch.to_uppercase());
        }
    }
    TransformResult::replace(out)
}

fn downcase(input: &str) -> TransformResult {
    TransformResult::replace(input.to_lowercase())
}

fn upcase(input: &str) -> TransformResult {
    TransformResult::replace(input.to_uppercase())
}

fn deburr(input: &str) -> TransformResult {
    TransformResult::replace(deunicode::deunicode(input))
}

fn replace_smart_quotes(input: &str) -> TransformResult {
    let out: String = input
        .chars()
        .map(|ch| match ch {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            other => other,
        })
        .collect();
    TransformResult::replace(out)
}

fn join_lower(input: &str, separator: &str) -> String {
    words(input)
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>()
        .join(separator)
}

/// Uppercase the first character only, leaving the rest of the word as-is.
fn upper_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Split like lodash's case helpers: deburr and drop apostrophes first, then
/// split on non-alphanumeric runs and camelCase/acronym boundaries.
fn words(input: &str) -> Vec<String> {
    let prepared = deunicode::deunicode(input).replace(|c| c == '\'' || c == '\u{2019}', "");
    split_words(&prepared)
}

fn split_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();

    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }

        let boundary = match current.chars().last() {
            Some(prev) => {
                // "fooBar": split where a lower/digit is followed by an upper.
                ((prev.is_lowercase() || prev.is_numeric()) && ch.is_uppercase())
                    // "XMLHttp": split before the last capital of an all-caps
                    // run that starts a new lowercase word ("XML" | "Http").
                    || (prev.is_uppercase()
                        && ch.is_uppercase()
                        && chars.peek().is_some_and(|next| next.is_lowercase()))
            }
            None => false,
        };

        if boundary {
            out.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }

    if !current.is_empty() {
        out.push(current);
    }
    out
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
    fn splits_camel_case_boundaries() {
        assert_eq!(split_words("XMLHttpRequest"), ["XML", "Http", "Request"]);
        assert_eq!(split_words("fooBar baz-qux"), ["foo", "Bar", "baz", "qux"]);
        assert_eq!(split_words("hello_world"), ["hello", "world"]);
    }

    #[test]
    fn converts_between_styles() {
        assert_eq!(text(camel_case("XMLHttpRequest")), "xmlHttpRequest");
        assert_eq!(text(kebab_case("XMLHttpRequest")), "xml-http-request");
        assert_eq!(text(snake_case("fooBar baz")), "foo_bar_baz");
        assert_eq!(text(start_case("XMLHttpRequest")), "XML Http Request");
    }

    #[test]
    fn deburrs_and_drops_apostrophes_before_splitting() {
        assert_eq!(text(snake_case("déjà vu's")), "deja_vus");
        assert_eq!(text(deburr("café")), "cafe");
    }

    #[test]
    fn sponge_leaves_first_character_alone() {
        let out = text(sponge_case("hello world"));
        assert_eq!(out.chars().next(), Some('h'));
        assert_eq!(out.to_lowercase(), "hello world");
    }

    #[test]
    fn replaces_smart_quotes() {
        assert_eq!(
            text(replace_smart_quotes("\u{2018}a\u{2019} \u{201C}b\u{201D}")),
            "'a' \"b\""
        );
    }

    #[test]
    fn downcase_and_upcase() {
        assert_eq!(text(downcase("HeLLo")), "hello");
        assert_eq!(text(upcase("HeLLo")), "HELLO");
    }
}
