use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use html_escape::decode_html_entities;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use zapslab_document::encoding::{self as byte_encoding, Encoding};

use super::{Scope, Transform, TransformResult};

/// Registry name of the byte-interpreting decode transform. The view matches on
/// it to pass the hex pane's current encoding (see [`super::apply_with_encoding`]);
/// `base64_decode_is_registered` keeps the constant and the registry in sync.
pub(crate) const BASE64_DECODE_NAME: &str = "Base64 Decode";

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Base64 Encode",
        description: "Encode the text as standard Base64.",
        tags: "base64,atob,encode",
        scope: Scope::SelectionOrFullText,
        apply: base64_encode,
    },
    Transform {
        name: BASE64_DECODE_NAME,
        description: "Decode standard or URL-safe Base64 using the hex pane's encoding; whitespace and padding are ignored and invalid input is an error.",
        tags: "base64,btoa,decode",
        scope: Scope::SelectionOrFullText,
        apply: base64_decode,
    },
    Transform {
        name: "HTML Encode",
        description: "Escape HTML-sensitive characters and non-ASCII code points as hex numeric entities.",
        tags: "html,encode,web",
        scope: Scope::SelectionOrFullText,
        apply: html_encode,
    },
    Transform {
        name: "HTML Decode",
        description: "Decode named, numeric, and hex HTML character entities.",
        tags: "html,decode,web",
        scope: Scope::SelectionOrFullText,
        apply: html_decode,
    },
    Transform {
        name: "HTML Encode all characters",
        description: "Escape every UTF-16 code unit as a decimal numeric entity; astral characters become two references.",
        tags: "html,encode,web,email",
        scope: Scope::SelectionOrFullText,
        apply: html_encode_all,
    },
    Transform {
        name: "URL Encode",
        description: "Percent-encode the text as a URL component (like encodeURIComponent).",
        tags: "url,encode,convert",
        scope: Scope::SelectionOrFullText,
        apply: url_encode,
    },
    Transform {
        name: "URL Decode",
        description: "Decode percent-encoded text as UTF-8; malformed escapes are an error.",
        tags: "url,decode,convert",
        scope: Scope::SelectionOrFullText,
        apply: url_decode,
    },
    Transform {
        name: "URL Entity Encode",
        description: "Percent-encode every UTF-16 code unit as lowercase hex, including letters and digits.",
        tags: "url,encode,full",
        scope: Scope::SelectionOrFullText,
        apply: url_entities_encode,
    },
    Transform {
        name: "URL Entities Decode",
        description: "Decode %XX hex runs as UTF-16 code units; sequences that are not valid UTF-16 are an error.",
        tags: "url,decode,full",
        scope: Scope::SelectionOrFullText,
        apply: url_entities_decode,
    },
    Transform {
        name: "Defang",
        description: "Neutralize URLs and indicators of compromise (hXXp://, [.]) so they are not clickable; Refang reverses it.",
        tags: "defang,url,ioc",
        scope: Scope::SelectionOrFullText,
        apply: url_defang,
    },
    Transform {
        name: "Refang",
        description: "Restore defanged URLs and indicators (hXXp -> http, [.] -> .); the inverse of Defang.",
        tags: "refang,url,ioc",
        scope: Scope::SelectionOrFullText,
        apply: url_refang,
    },
    Transform {
        name: "Add Slashes",
        description: "Escape backslashes, single/double quotes, and NUL with a leading backslash (PHP addslashes style).",
        tags: "add,slashes,escape",
        scope: Scope::SelectionOrFullText,
        apply: add_slashes,
    },
    Transform {
        name: "Remove Slashes",
        description: "Remove one level of backslash escaping; the inverse of Add Slashes.",
        tags: "strip,slashes,remove,unescape",
        scope: Scope::SelectionOrFullText,
        apply: remove_slashes,
    },
    Transform {
        name: "Fish PATH Hex Converter",
        description: "Escape shell-significant characters as \\xNN for fish variable assignments; spaces become \\x:.",
        tags: "fish_user_paths, fish, hex, ascii, path, var",
        scope: Scope::SelectionOrFullText,
        apply: fish_hex_path_converter,
    },
];

/// The characters `encodeURIComponent` leaves untouched, expressed as
/// `NON_ALPHANUMERIC` minus the unreserved/reserved-but-safe punctuation.
const URL_COMPONENT_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn base64_encode(input: &str) -> TransformResult {
    TransformResult::replace(STANDARD.encode(input.as_bytes()))
}

fn base64_decode(input: &str) -> TransformResult {
    base64_decode_with_encoding(input, Encoding::Utf8)
}

/// Decodes Base64 and interprets the resulting bytes as text with `encoding`.
///
/// The document is Unicode text, so arbitrary bytes can only be represented
/// losslessly as the Latin-1 code points U+0000..=U+00FF; that is exactly what
/// `Encoding::Latin1` does. Interpreting the bytes with the hex pane's current
/// encoding keeps that pane the single "how are these bytes text?" control:
/// decode binary with Latin-1 selected and the pane shows the original bytes.
pub(crate) fn base64_decode_with_encoding(input: &str, encoding: Encoding) -> TransformResult {
    // Boop's decoder accepts URL-safe input and ignores whitespace, padding,
    // and any other non-alphabet characters before decoding.
    let mut filtered = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '-' => filtered.push('+'),
            '_' => filtered.push('/'),
            ch if ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' => filtered.push(ch),
            _ => {}
        }
    }

    let remainder = filtered.len() % 4;
    if remainder == 1 {
        return TransformResult::error("Base64 Decode: input is not valid Base64");
    }
    if remainder != 0 {
        filtered.push_str(&"=".repeat(4 - remainder));
    }

    match STANDARD.decode(filtered.as_bytes()) {
        Ok(bytes) => match byte_encoding::decode(&bytes, encoding) {
            Ok(text) => TransformResult::replace(text),
            Err(_) => TransformResult::error(format!(
                "Base64 Decode: decoded bytes are not valid {}; set the hex encoding to Latin-1 to decode arbitrary binary",
                encoding.label()
            )),
        },
        Err(error) => TransformResult::error(format!("Base64 Decode: {error}")),
    }
}

fn html_encode(input: &str) -> TransformResult {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        // Matches `he.encode` defaults: hex numeric references (not named
        // ones) for markup characters, astral code points, and the BMP
        // characters listed by he's `regexBmpWhitelist`. A few C1 controls
        // (U+0080, U+0082..=U+008C, U+008E, U+0091..=U+009C, U+009E..=U+009F),
        // NUL, tab-free whitespace like CR/LF, and printable ASCII pass
        // through unchanged.
        let escape = matches!(
            ch,
            '"' | '&' | '\''
                | '<'
                | '>'
                | '`'
                | '\u{01}'..='\u{09}'
                | '\u{0B}'
                | '\u{0C}'
                | '\u{0E}'..='\u{1F}'
                | '\u{7F}'
                | '\u{81}'
                | '\u{8D}'
                | '\u{8F}'
                | '\u{90}'
                | '\u{9D}'
        ) || ch >= '\u{A0}';

        if escape {
            output.push_str(&format!("&#x{:X};", ch as u32));
        } else {
            output.push(ch);
        }
    }
    TransformResult::replace(output)
}

fn html_decode(input: &str) -> TransformResult {
    TransformResult::replace(decode_html_entities(input).into_owned())
}

fn html_encode_all(input: &str) -> TransformResult {
    let mut output = String::with_capacity(input.len() * 6);
    // Boop iterates UTF-16 code units, so astral characters become a pair of
    // decimal references rather than a single code-point reference.
    for unit in input.encode_utf16() {
        output.push_str(&format!("&#{unit};"));
    }
    TransformResult::replace(output)
}

fn url_encode(input: &str) -> TransformResult {
    TransformResult::replace(utf8_percent_encode(input, URL_COMPONENT_SET).to_string())
}

fn url_decode(input: &str) -> TransformResult {
    if !has_valid_percent_escapes(input) {
        return TransformResult::error("URL Decode: URI malformed");
    }

    match percent_decode_str(input).decode_utf8() {
        Ok(text) => TransformResult::replace(text.into_owned()),
        Err(_) => TransformResult::error("URL Decode: percent-encoded bytes are not valid UTF-8"),
    }
}

/// `decodeURIComponent` rejects a `%` that is not followed by two hex digits;
/// `percent-encoding` would instead leave it verbatim, so check up front.
fn has_valid_percent_escapes(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn url_entities_encode(input: &str) -> TransformResult {
    let mut output = String::new();
    // The script walks `charCodeAt`, i.e. UTF-16 code units, and formats each
    // value as lowercase hex without zero-padding.
    for unit in input.encode_utf16() {
        output.push_str(&format!("%{unit:x}"));
    }
    TransformResult::replace(output)
}

fn url_entities_decode(input: &str) -> TransformResult {
    let mut units: Vec<u16> = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '%' {
            let mut buffer = [0u16; 2];
            units.extend_from_slice(ch.encode_utf16(&mut buffer));
            continue;
        }

        // Consume the whole hex run: the encoder emits up to four digits (the
        // surrogate halves of astral characters are four digits each), and the
        // original `parseInt` is greedy as well. Wrapping in u32 keeps the low
        // 16 bits, matching `String.fromCharCode`'s truncation.
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(digit) = chars.peek().and_then(|next| next.to_digit(16)) {
            value = value.wrapping_mul(16).wrapping_add(digit);
            digits += 1;
            chars.next();
        }

        if digits == 0 {
            // The original parses an empty group as NaN and emits a NUL; keep
            // the literal `%` instead so stray input is not corrupted.
            units.push(u16::from(b'%'));
        } else {
            units.push(value as u16);
        }
    }

    match String::from_utf16(&units) {
        Ok(text) => TransformResult::replace(text),
        Err(_) => {
            TransformResult::error("URL Entities Decode: input decodes to invalid UTF-16")
        }
    }
}

fn url_defang(input: &str) -> TransformResult {
    let dotted = input.replace('.', "[.]");
    let http = replace_ascii_case_insensitive(&dotted, "http", "hXXp");
    TransformResult::replace(http.replace("://", "[://]"))
}

fn url_refang(input: &str) -> TransformResult {
    let dotted = input.replace("[.]", ".");
    let http = replace_ascii_case_insensitive(&dotted, "hXXp", "http");
    TransformResult::replace(http.replace("[://]", "://"))
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut output = String::with_capacity(haystack.len());
    let mut copied = 0;
    let mut index = 0;

    while index + needle_bytes.len() <= haystack_bytes.len() {
        if haystack_bytes[index..index + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes) {
            output.push_str(&haystack[copied..index]);
            output.push_str(replacement);
            index += needle_bytes.len();
            copied = index;
        } else {
            index += 1;
        }
    }

    output.push_str(&haystack[copied..]);
    output
}

fn add_slashes(input: &str) -> TransformResult {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '"' | '\'' => {
                output.push('\\');
                output.push(ch);
            }
            '\0' => output.push_str("\\0"),
            _ => output.push(ch),
        }
    }
    TransformResult::replace(output)
}

fn remove_slashes(input: &str) -> TransformResult {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }

        match chars.next() {
            Some('\\') => output.push('\\'),
            Some('0') => output.push('\0'),
            // JS `.` cannot match line terminators, so a backslash before one
            // is dropped on its own.
            Some(line @ ('\n' | '\r' | '\u{2028}' | '\u{2029}')) => output.push(line),
            Some(other) => output.push(other),
            None => {}
        }
    }

    TransformResult::replace(output)
}

fn fish_hex_path_converter(input: &str) -> TransformResult {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        // Note space maps to ":" rather than a hex pair; the script prepends
        // `\x` to whatever the dict holds.
        let escaped = match ch {
            ' ' => Some("\\x:"),
            '%' => Some("\\x25"),
            '&' => Some("\\x26"),
            '+' => Some("\\x2b"),
            '-' => Some("\\x2d"),
            '.' => Some("\\x2e"),
            '*' => Some("\\x2a"),
            ':' => Some("\\x3a"),
            '@' => Some("\\x40"),
            ';' => Some("\\x3b"),
            _ => None,
        };

        match escaped {
            Some(value) => output.push_str(value),
            None => output.push(ch),
        }
    }
    TransformResult::replace(output)
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

    fn is_error(result: TransformResult) -> bool {
        matches!(result, TransformResult::Error(_))
    }

    #[test]
    fn registry_contains_every_transform() {
        assert_eq!(TRANSFORMS.len(), 14);
        assert!(TRANSFORMS.iter().all(|t| t.scope == Scope::SelectionOrFullText));

        let names: Vec<&str> = TRANSFORMS.iter().map(|t| t.name).collect();
        for expected in [
            "Base64 Encode",
            "Base64 Decode",
            "HTML Encode",
            "HTML Decode",
            "HTML Encode all characters",
            "URL Encode",
            "URL Decode",
            "URL Entity Encode",
            "URL Entities Decode",
            "Defang",
            "Refang",
            "Add Slashes",
            "Remove Slashes",
            "Fish PATH Hex Converter",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn base64_round_trips_and_matches_known_vectors() {
        assert_eq!(replaced(base64_encode("hello")), "aGVsbG8=");

        let sample = "ZapSlab — café 😀";
        let encoded = replaced(base64_encode(sample));
        assert_eq!(replaced(base64_decode(&encoded)), sample);
    }

    #[test]
    fn base64_decode_accepts_unpadded_and_noisy_input() {
        assert_eq!(replaced(base64_decode("aGVsbG8=")), "hello");
        assert_eq!(replaced(base64_decode("aGVsbG8")), "hello");
        assert_eq!(replaced(base64_decode("aGVs\n bG8=")), "hello");
    }

    #[test]
    fn base64_decode_reports_invalid_input() {
        assert!(is_error(base64_decode("a")));
        // "/w==" is the byte 0xFF, which is not valid UTF-8.
        assert!(is_error(base64_decode("/w==")));
        assert_eq!(replaced(base64_decode("")), "");
    }

    #[test]
    fn base64_decode_uses_the_given_encoding() {
        // 0xFF is not valid UTF-8, but it is the Latin-1 code point U+00FF.
        assert!(is_error(base64_decode_with_encoding("/w==", Encoding::Utf8)));
        assert_eq!(
            replaced(base64_decode_with_encoding("/w==", Encoding::Latin1)),
            "\u{00FF}"
        );

        // Arbitrary binary round-trips byte-for-byte through Latin-1.
        let bytes = [0x00, 0x7F, 0x80, 0xFF];
        let encoded = STANDARD.encode(bytes);
        let decoded = replaced(base64_decode_with_encoding(&encoded, Encoding::Latin1));
        assert_eq!(
            byte_encoding::encode(&decoded, Encoding::Latin1).expect("encode"),
            bytes.to_vec()
        );
    }

    #[test]
    fn base64_decode_is_registered() {
        let transform = TRANSFORMS
            .iter()
            .find(|transform| transform.name == BASE64_DECODE_NAME)
            .expect("Base64 Decode must be registered");
        assert_eq!(transform.name, "Base64 Decode");
    }

    #[test]
    fn html_encode_matches_he_defaults() {
        assert_eq!(
            replaced(html_encode("&<>\"'`")),
            "&#x26;&#x3C;&#x3E;&#x22;&#x27;&#x60;"
        );
        assert_eq!(replaced(html_encode("café")), "caf&#xE9;");
        assert_eq!(replaced(html_encode("😀")), "&#x1F600;");
        assert_eq!(replaced(html_encode("line\nbreak\rcr")), "line\nbreak\rcr");
        // he leaves U+0080 alone but escapes U+007F and most C1 controls.
        assert_eq!(replaced(html_encode("\u{7F}\u{80}")), "&#x7F;\u{80}");
    }

    #[test]
    fn html_decode_decodes_named_numeric_and_hex_entities() {
        assert_eq!(
            replaced(html_decode("&amp;&lt;&gt;&quot;&#x27;&#65;")),
            "&<>\"'A"
        );

        let sample = "A & B <tag> \"q\" 's' café 😀\n";
        assert_eq!(
            replaced(html_decode(&replaced(html_encode(sample)))),
            sample
        );
    }

    #[test]
    fn html_encode_all_encodes_every_utf16_unit() {
        assert_eq!(replaced(html_encode_all("Hi")), "&#72;&#105;");
        assert_eq!(replaced(html_encode_all("é")), "&#233;");
        assert_eq!(replaced(html_encode_all("\n")), "&#10;");
        assert_eq!(replaced(html_encode_all("😀")), "&#55357;&#56832;");
    }

    #[test]
    fn url_encode_follows_encode_uri_component() {
        assert_eq!(replaced(url_encode("AZaz09-_.!~*'()")), "AZaz09-_.!~*'()");
        assert_eq!(
            replaced(url_encode("a b/c?d=e&f")),
            "a%20b%2Fc%3Fd%3De%26f"
        );
        assert_eq!(replaced(url_encode("é")), "%C3%A9");

        let sample = "some value/with?stuff=1&more é";
        assert_eq!(
            replaced(url_decode(&replaced(url_encode(sample)))),
            sample
        );
    }

    #[test]
    fn url_decode_rejects_malformed_input() {
        assert_eq!(replaced(url_decode("a%20b")), "a b");
        assert_eq!(replaced(url_decode("100%25")), "100%");
        assert!(is_error(url_decode("%zz")));
        assert!(is_error(url_decode("%")));
        assert!(is_error(url_decode("%C3")));
    }

    #[test]
    fn url_entities_encode_uses_utf16_code_units() {
        assert_eq!(replaced(url_entities_encode("a b")), "%61%20%62");
        assert_eq!(replaced(url_entities_encode("\n")), "%a");
        assert_eq!(replaced(url_entities_encode("😀")), "%d83d%de00");
    }

    #[test]
    fn url_entities_decode_round_trips() {
        assert_eq!(replaced(url_entities_decode("%61%20%62")), "a b");
        assert_eq!(replaced(url_entities_decode("%a")), "\n");
        assert_eq!(replaced(url_entities_decode("100%25")), "100%");
        // Three- and four-digit groups fall out of the unpadded encoder.
        assert_eq!(replaced(url_entities_encode("Ω")), "%3a9");
        assert_eq!(replaced(url_entities_decode("%3a9")), "Ω");
        assert_eq!(replaced(url_entities_decode("%d83d%de00")), "😀");

        let sample = "ZapSlab test 123! é 😀\n";
        assert_eq!(
            replaced(url_entities_decode(&replaced(url_entities_encode(sample)))),
            sample
        );
        // Stray percent signs are preserved rather than turned into NULs.
        assert_eq!(replaced(url_entities_decode("50%")), "50%");
    }

    #[test]
    fn defang_and_refang_round_trip() {
        let url = "http://example.com/a.b?x=1";
        let defanged = replaced(url_defang(url));
        assert_eq!(defanged, "hXXp[://]example[.]com/a[.]b?x=1");
        assert_eq!(replaced(url_refang(&defanged)), url);
    }

    #[test]
    fn defang_and_refang_are_case_insensitive_for_http() {
        assert_eq!(
            replaced(url_defang("HTTP://EXAMPLE.COM")),
            "hXXp[://]EXAMPLE[.]COM"
        );
        assert_eq!(
            replaced(url_refang("HXXP[://]Example[.]com")),
            "http://Example.com"
        );
    }

    #[test]
    fn add_slashes_escapes_only_slashes_quotes_and_nul() {
        assert_eq!(replaced(add_slashes(r#"a'b"c\d"#)), r#"a\'b\"c\\d"#);
        assert_eq!(replaced(add_slashes("\0")), r"\0");
        assert_eq!(replaced(add_slashes("plain text")), "plain text");
    }

    #[test]
    fn remove_slashes_inverts_add_slashes() {
        assert_eq!(replaced(remove_slashes(r"a\'b")), "a'b");
        assert_eq!(replaced(remove_slashes(r"a\0b")), "a\0b");
        assert_eq!(replaced(remove_slashes("trailing\\")), "trailing");

        let sample = "a'b\"c\\d\0e";
        let escaped = replaced(add_slashes(sample));
        assert_eq!(replaced(remove_slashes(&escaped)), sample);
    }

    #[test]
    fn fish_hex_path_converter_uses_the_script_dictionary() {
        assert_eq!(replaced(fish_hex_path_converter("a b")), r"a\x:b");
        assert_eq!(
            replaced(fish_hex_path_converter("/usr/local.bin")),
            r"/usr/local\x2ebin"
        );
        assert_eq!(
            replaced(fish_hex_path_converter("%&+-.*:@;")),
            r"\x25\x26\x2b\x2d\x2e\x2a\x3a\x40\x3b"
        );
    }
}
