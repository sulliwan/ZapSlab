//! Number base conversions and summation, ported from Boop's scripts.
//!
//! The conversions are line-oriented and lean on JavaScript's `parseInt` and
//! `Number`. Those are looser than Rust's `FromStr`: they parse a leading run of
//! valid digits and ignore the rest (`"12abc"` becomes 12), accept a `0x` prefix
//! when the radix is 16, and yield `NaN` for empty input. The helpers below
//! reproduce that behavior so the output matches the original scripts.

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "ASCII To Hex",
        description: "Converts ASCII characters to hexadecimal codes.",
        tags: "ascii,hex,convert",
        // The script scans `state.fullText`, so it ignores any selection.
        scope: Scope::FullText,
        apply: ascii_to_hex,
    },
    Transform {
        name: "Hex To ASCII",
        description: "Converts hexadecimal values into ASCII characters",
        tags: "hex,ascii,convert",
        scope: Scope::FullText,
        apply: hex_to_ascii,
    },
    Transform {
        name: "Binary to Decimal",
        description: "Converts binary values to decimal.",
        tags: "decimal,binary,dec,bin",
        scope: Scope::SelectionOrFullText,
        apply: binary_to_decimal,
    },
    Transform {
        name: "Decimal to Binary",
        description: "Converts decimal values to binary.",
        tags: "decimal,binary,dec,bin",
        scope: Scope::SelectionOrFullText,
        apply: decimal_to_binary,
    },
    Transform {
        name: "Decimal to Hex",
        description: "Converts decimal values to hexadecimal.",
        tags: "decimal,hexadecimal,dec,hex",
        scope: Scope::SelectionOrFullText,
        apply: decimal_to_hex,
    },
    Transform {
        name: "Hex to Dec",
        description: "Converts hexadecimal to decimal.",
        tags: "decimal,hexadecimal,dec,hex",
        scope: Scope::SelectionOrFullText,
        apply: hex_to_decimal,
    },
    Transform {
        name: "Sum All",
        description: "Sums up a list of numbers.",
        tags: "sum,calculator,addition,add",
        scope: Scope::SelectionOrFullText,
        apply: sum_all,
    },
];

fn ascii_to_hex(text: &str) -> TransformResult {
    // `charCodeAt` walks UTF-16 code units, so astral characters contribute
    // their surrogate pair as four hex digits (e.g. an emoji -> `D83DDE00`).
    let mut out = String::new();
    for unit in text.encode_utf16() {
        out.push_str(&format!("{unit:02X}"));
    }
    TransformResult::replace(out)
}

fn hex_to_ascii(text: &str) -> TransformResult {
    let mut out = String::new();
    let mut high: Option<u32> = None;
    for c in text.chars() {
        if let Some(digit) = c.to_digit(16) {
            match high.take() {
                Some(high) => {
                    // `high` and `digit` are both < 16, so the byte is < 256.
                    if let Some(ch) = char::from_u32(high * 16 + digit) {
                        out.push(ch);
                    }
                }
                None => high = Some(digit),
            }
        } else if !matches!(c, ' ' | '\t' | '\n' | '\r') {
            return TransformResult::error("Text is not hex");
        }
    }
    // A dangling half-byte is silently dropped, matching the script.
    TransformResult::replace(out)
}

fn binary_to_decimal(text: &str) -> TransformResult {
    TransformResult::replace(convert_lines(text, |line| match parse_int(line, 2) {
        Some(value) => format_number(value),
        None => line.to_string(),
    }))
}

fn decimal_to_binary(text: &str) -> TransformResult {
    TransformResult::replace(convert_lines(text, |line| match parse_int(line, 10) {
        Some(value) => number_to_radix(value, 2, true),
        None => line.to_string(),
    }))
}

fn decimal_to_hex(text: &str) -> TransformResult {
    TransformResult::replace(convert_lines(text, |line| {
        // The script gates on `isNaN(text)` (a `Number` coercion) but then
        // converts with `parseInt`, so `""` and `"Infinity"` reach `parseInt`
        // and end up as `NAN` rather than being passed through.
        if js_number(line).is_none() {
            line.to_string()
        } else {
            match parse_int(line, 10) {
                Some(value) => number_to_radix(value, 16, true),
                None => "NAN".to_string(),
            }
        }
    }))
}

fn hex_to_decimal(text: &str) -> TransformResult {
    TransformResult::replace(convert_lines(text, |line| match parse_int(line, 16) {
        Some(value) => format_number(value),
        None => line.to_string(),
    }))
}

/// Trim each line, convert it, and rejoin. The script trims the whole result at
/// the end as well, which drops leading/trailing blank lines.
fn convert_lines(text: &str, convert: impl Fn(&str) -> String) -> String {
    let joined = text
        .split('\n')
        .map(|line| convert(line.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim().to_string()
}

fn sum_all(text: &str) -> TransformResult {
    if text.is_empty() {
        return TransformResult::error("");
    }

    let numbers = collect_numbers(text);
    if numbers.is_empty() {
        // Boop's `reduce` throws on an empty array; surface an error instead of
        // panicking, leaving the text untouched.
        return TransformResult::error("Sum All: no numbers found");
    }

    let sum: f64 = numbers.iter().sum();
    let mut sum_output = format_number(sum);
    if numbers.len() > 1 {
        sum_output.push_str("\t// ");
        let joined = numbers
            .iter()
            .map(|number| format_number(*number))
            .collect::<Vec<_>>()
            .join(" + ");
        sum_output.push_str(&joined);
    }

    let mut lines: Vec<String> = text
        .split(|c: char| matches!(c, '\n' | ',' | ';'))
        .map(annotate_segment)
        .collect();
    // The script concatenates the total as an array element that itself starts
    // with a newline, which is why there is a blank line before `= total`.
    lines.push(format!("\n= {sum_output}"));
    TransformResult::replace(lines.join("\n"))
}

/// Annotate one Sum All segment, mirroring the script's template string and its
/// JavaScript truthiness rules.
fn annotate_segment(segment: &str) -> String {
    let text = segment.trim();
    if text.starts_with('=') || text.is_empty() || is_plain_number(text) {
        return text.to_string();
    }

    let number = get_number(text);
    if number.truthy() {
        format!("{text}\t// {}", number.to_js_string())
    } else {
        // A falsy number is interpolated directly: `0` becomes `"0"`, an empty
        // parse becomes `""`, and `NaN` from a malformed fraction becomes
        // `"NaN"`.
        format!("{text}{}", number.to_js_string())
    }
}

fn collect_numbers(text: &str) -> Vec<f64> {
    strip_line_comments(text)
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '='))
        .filter_map(|token| match get_number(token) {
            ParsedNumber::Value(value) if value != 0.0 && !value.is_nan() => Some(value),
            _ => None,
        })
        .collect()
}

/// `s.replace(/\/\/.*/g, '')`: drop from `//` to the end of each line.
fn strip_line_comments(text: &str) -> String {
    text.split('\n')
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The script's `getNumber`, which returns either `''` (unparseable) or a
/// number that may itself be `NaN` for a malformed fraction.
enum ParsedNumber {
    Empty,
    Value(f64),
}

impl ParsedNumber {
    fn truthy(&self) -> bool {
        matches!(self, Self::Value(value) if *value != 0.0 && !value.is_nan())
    }

    fn to_js_string(&self) -> String {
        match self {
            Self::Empty => String::new(),
            Self::Value(value) => format_number(*value),
        }
    }
}

fn get_number(s: &str) -> ParsedNumber {
    if looks_like_fraction(s) {
        return ParsedNumber::Value(get_fraction(s));
    }
    match js_number(s) {
        Some(value) => ParsedNumber::Value(value),
        None => ParsedNumber::Empty,
    }
}

/// `/^[\d\.]+\/[\d\.]+$/`: digits and dots on each side of exactly one slash.
fn looks_like_fraction(s: &str) -> bool {
    let mut parts = s.split('/');
    let Some(numerator) = parts.next() else {
        return false;
    };
    let Some(denominator) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !numerator.is_empty()
        && !denominator.is_empty()
        && numerator.chars().all(|c| c.is_ascii_digit() || c == '.')
        && denominator.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn get_fraction(s: &str) -> f64 {
    let mut parts = s.split('/');
    let numerator = parts.next().unwrap_or("");
    let denominator = parts.next().unwrap_or("");
    let numerator = js_number(numerator).unwrap_or(f64::NAN);
    let denominator = js_number(denominator).unwrap_or(f64::NAN);
    numerator / denominator
}

/// `e.toString() === Number(e).toString()`.
fn is_plain_number(s: &str) -> bool {
    match js_number(s) {
        Some(value) => format_number(value) == s,
        None => false,
    }
}

/// JavaScript's `Number(string)` for the numeric forms these scripts encounter.
/// Returns `None` where JavaScript would produce `NaN`.
fn js_number(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return Some(0.0);
    }
    match s {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return parse_full_radix(rest, 16);
    }
    if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        return parse_full_radix(rest, 2);
    }
    if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        return parse_full_radix(rest, 8);
    }
    if is_js_decimal(s) {
        return s.parse::<f64>().ok();
    }
    None
}

/// Parse an entire radix-prefixed literal, requiring every character to be a
/// valid digit (unlike `parseInt`, which stops at the first invalid one).
fn parse_full_radix(s: &str, radix: u32) -> Option<f64> {
    if s.is_empty() || !s.chars().all(|c| c.to_digit(radix).is_some()) {
        return None;
    }
    if let Ok(value) = u128::from_str_radix(s, radix) {
        return Some(value as f64);
    }
    let mut value = 0.0;
    for c in s.chars() {
        // Validation above guarantees a digit.
        let digit = c.to_digit(radix).unwrap();
        value = value * f64::from(radix) + f64::from(digit);
    }
    Some(value)
}

/// JavaScript's decimal `StringNumericLiteral` grammar (without the
/// `Infinity`/`NaN` spellings, which `js_number` handles separately).
fn is_js_decimal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }

    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }

    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let mut exponent_digits = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            exponent_digits += 1;
        }
        if exponent_digits == 0 {
            return false;
        }
    }

    i == bytes.len()
}

/// JavaScript's `parseInt(s, radix)`: optional sign, an optional `0x` prefix for
/// radix 16, then the longest valid digit run. `None` means `NaN`.
fn parse_int(s: &str, radix: u32) -> Option<f64> {
    let s = s.trim_start();
    let (negative, rest) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest)
    } else {
        (false, s)
    };
    let rest = if radix == 16 {
        rest.strip_prefix("0x")
            .or_else(|| rest.strip_prefix("0X"))
            .unwrap_or(rest)
    } else {
        rest
    };

    // JavaScript computes the exact integer and rounds once when converting to
    // a double. Accumulating in `u128` reproduces that for the common 64-bit
    // range; past 128 bits we fall back to per-step `f64` arithmetic.
    let mut exact: u128 = 0;
    let mut exact_overflow = false;
    let mut approximate = 0.0;
    let mut found = false;
    for c in rest.chars() {
        let Some(digit) = c.to_digit(radix) else {
            break;
        };
        found = true;
        approximate = approximate * f64::from(radix) + f64::from(digit);
        if !exact_overflow {
            match exact
                .checked_mul(u128::from(radix))
                .and_then(|value| value.checked_add(u128::from(digit)))
            {
                Some(value) => exact = value,
                None => exact_overflow = true,
            }
        }
    }

    if !found {
        return None;
    }
    let value = if exact_overflow {
        approximate
    } else {
        exact as f64
    };
    Some(if negative { -value } else { value })
}

/// `Number.prototype.toString()` for the values these scripts produce.
///
/// Rust and JavaScript agree on plain decimal output for ordinary values; the
/// exponent-notation cutoff for very large/small magnitudes is not replicated.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value == f64::INFINITY {
        "Infinity".to_string()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else if value == 0.0 {
        // Covers `-0.0`, which JavaScript prints as `0`.
        "0".to_string()
    } else {
        value.to_string()
    }
}

/// `Number.prototype.toString(radix)` for the integral values returned by
/// `parseInt`. Only radices 2 and 16 are used here.
fn number_to_radix(value: f64, radix: u32, uppercase: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }

    let negative = value < 0.0;
    let mut magnitude = value.abs();
    let mut digits = Vec::new();
    while magnitude >= 1.0 {
        let remainder = (magnitude % f64::from(radix)) as u32;
        // `remainder < radix`, so `from_digit` always succeeds.
        let digit = char::from_digit(remainder, radix).unwrap();
        digits.push(if uppercase {
            digit.to_ascii_uppercase()
        } else {
            digit
        });
        magnitude = (magnitude / f64::from(radix)).floor();
    }
    if negative {
        digits.push('-');
    }
    digits.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected a replacement"),
        }
    }

    fn error_message(result: TransformResult) -> String {
        match result {
            TransformResult::Error(message) => message,
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn registers_boop_names_in_order() {
        let names: Vec<&str> = TRANSFORMS.iter().map(|transform| transform.name).collect();
        assert_eq!(
            names,
            vec![
                "ASCII To Hex",
                "Hex To ASCII",
                "Binary to Decimal",
                "Decimal to Binary",
                "Decimal to Hex",
                "Hex to Dec",
                "Sum All",
            ]
        );
    }

    #[test]
    fn ascii_to_hex_walks_utf16_code_units() {
        assert_eq!(replaced(ascii_to_hex("Hi")), "4869");
        assert_eq!(replaced(ascii_to_hex("é")), "E9");
        assert_eq!(replaced(ascii_to_hex("😀")), "D83DDE00");
    }

    #[test]
    fn hex_to_ascii_groups_digits_and_skips_whitespace() {
        assert_eq!(replaced(hex_to_ascii("48656C6C6F")), "Hello");
        assert_eq!(replaced(hex_to_ascii("48 65 6c 6c 6f")), "Hello");
        assert_eq!(replaced(hex_to_ascii("486")), "H");
        assert_eq!(error_message(hex_to_ascii("zz")), "Text is not hex");
    }

    #[test]
    fn binary_to_decimal_converts_each_line() {
        assert_eq!(replaced(binary_to_decimal("101\n10")), "5\n2");
        assert_eq!(replaced(binary_to_decimal("101abc")), "5");
        assert_eq!(replaced(binary_to_decimal("abc")), "abc");
        assert_eq!(replaced(binary_to_decimal("-101")), "-5");
    }

    #[test]
    fn decimal_to_binary_converts_each_line() {
        assert_eq!(replaced(decimal_to_binary("5\n10")), "101\n1010");
        assert_eq!(replaced(decimal_to_binary("abc")), "abc");
        assert_eq!(replaced(decimal_to_binary("-5")), "-101");
    }

    #[test]
    fn decimal_to_hex_converts_each_line() {
        assert_eq!(replaced(decimal_to_hex("255\n4095")), "FF\nFFF");
        assert_eq!(replaced(decimal_to_hex("abc")), "abc");
    }

    #[test]
    fn hex_to_decimal_converts_each_line() {
        assert_eq!(replaced(hex_to_decimal("ff\n10")), "255\n16");
        assert_eq!(replaced(hex_to_decimal("0xff")), "255");
        assert_eq!(replaced(hex_to_decimal("zz")), "zz");
    }

    #[test]
    fn sum_all_appends_total_and_running_comment() {
        assert_eq!(
            replaced(sum_all("1\n2\n3")),
            "1\n2\n3\n\n= 6\t// 1 + 2 + 3"
        );
    }

    #[test]
    fn sum_all_handles_fractions() {
        assert_eq!(
            replaced(sum_all("1/2\n1/4")),
            "1/2\t// 0.5\n1/4\t// 0.25\n\n= 0.75\t// 0.5 + 0.25"
        );
    }

    #[test]
    fn sum_all_ignores_zeros_and_existing_totals() {
        assert_eq!(
            replaced(sum_all("0\n5\n= 99")),
            "0\n5\n= 99\n\n= 104\t// 5 + 99"
        );
    }

    #[test]
    fn sum_all_annotates_non_canonical_numbers() {
        assert_eq!(
            replaced(sum_all("01\n2")),
            "01\t// 1\n2\n\n= 3\t// 1 + 2"
        );
    }

    #[test]
    fn sum_all_errors_without_input_or_numbers() {
        assert_eq!(error_message(sum_all("")), "");
        assert_eq!(error_message(sum_all("abc")), "Sum All: no numbers found");
    }
}
