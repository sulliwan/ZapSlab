//! Punycode (RFC 3492) encode/decode for internationalized domain labels.
//!
//! The codec is implemented here instead of pulling in an IDNA crate: the
//! palette only needs the raw RFC 3492 algorithm, whereas `idna` also applies
//! UTS #46 mapping (which silently rewrites input before encoding) and would be
//! a new dependency.
//!
//! Punycode has no notion of labels or lines — it encodes an arbitrary code
//! point sequence — so the transforms supply the IDN shape: every dot-separated
//! label of every line is converted independently, with separators preserved.

use super::{Scope, Transform, TransformResult};

/// ACE (ASCII Compatible Encoding) prefix that marks an encoded DNS label, so a
/// decoder can tell `xn--mnchen-3ya` from an ordinary ASCII label.
const ACE_PREFIX: &str = "xn--";

// RFC 3492 section 5 parameters for Punycode.
const BASE: u64 = 36;
const TMIN: u64 = 1;
const TMAX: u64 = 26;
const SKEW: u64 = 38;
const DAMP: u64 = 700;
const INITIAL_BIAS: u64 = 72;
const INITIAL_CODE_POINT: u64 = 0x80;
const DELIMITER: char = '-';

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Punycode Encode",
        description: "Encode each dot-separated label of every line as Punycode with the xn-- prefix; ASCII labels are left unchanged.",
        tags: "punycode,idn,domain,ace,encode",
        scope: Scope::SelectionOrFullText,
        apply: punycode_encode,
    },
    Transform {
        name: "Punycode Decode",
        description: "Decode xn-- Punycode labels back to Unicode; labels without the prefix are left unchanged and invalid payloads are an error.",
        tags: "punycode,idn,domain,ace,decode",
        scope: Scope::SelectionOrFullText,
        apply: punycode_decode,
    },
];

fn punycode_encode(input: &str) -> TransformResult {
    match map_labels(input, encode_ace_label) {
        Ok(text) => TransformResult::replace(text),
        Err(message) => TransformResult::error(message),
    }
}

fn punycode_decode(input: &str) -> TransformResult {
    match map_labels(input, decode_ace_label) {
        Ok(text) => TransformResult::replace(text),
        Err(message) => TransformResult::error(message),
    }
}

/// Applies `convert` to every dot-separated label of every line, keeping the
/// line and label separators exactly as they were.
fn map_labels(input: &str, convert: fn(&str) -> Result<String, String>) -> Result<String, String> {
    let mut output = String::with_capacity(input.len());
    for (line_index, line) in input.split('\n').enumerate() {
        if line_index > 0 {
            output.push('\n');
        }
        for (label_index, label) in line.split('.').enumerate() {
            if label_index > 0 {
                output.push('.');
            }
            output.push_str(&convert(label)?);
        }
    }
    Ok(output)
}

fn encode_ace_label(label: &str) -> Result<String, String> {
    // An all-ASCII label is already its own Punycode form; prefixing it would
    // rewrite domains that were never internationalized (and `a-b` would
    // become the odd-looking `a-b-`).
    if label.is_ascii() {
        return Ok(label.to_string());
    }

    match encode_label(label) {
        Some(encoded) => Ok(format!("{ACE_PREFIX}{encoded}")),
        None => Err(format!(
            "Punycode Encode: label {label:?} could not be encoded"
        )),
    }
}

fn decode_ace_label(label: &str) -> Result<String, String> {
    let Some(payload) = strip_ace_prefix(label) else {
        return Ok(label.to_string());
    };
    // `xn--` with nothing after it is not a decodable label, not an empty one.
    if payload.is_empty() {
        return Err(format!(
            "Punycode Decode: label {label:?} has no Punycode payload"
        ));
    }

    match decode_label(payload) {
        Some(decoded) => Ok(decoded),
        None => Err(format!("Punycode Decode: {label:?} is not valid Punycode")),
    }
}

/// Returns the payload after an `xn--` prefix, matched case-insensitively (DNS
/// labels are case-insensitive and uppercase ACE prefixes appear in the wild).
fn strip_ace_prefix(label: &str) -> Option<&str> {
    let prefix = label.get(..ACE_PREFIX.len())?;
    prefix
        .eq_ignore_ascii_case(ACE_PREFIX)
        .then(|| &label[ACE_PREFIX.len()..])
}

/// RFC 3492 section 6.3 bias adaptation.
fn adapt(delta: u64, num_points: u64, first_time: bool) -> u64 {
    let mut delta = delta / if first_time { DAMP } else { 2 };
    // Saturating: `delta` is bounded by the caller's checked arithmetic, so this
    // only guards against a hypothetical u64 overflow on absurd input.
    delta = delta.saturating_add(delta / num_points.max(1));

    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + (((BASE - TMIN + 1) * delta) / (delta + SKEW))
}

/// Encodes a digit 0..=35 as its Punycode character: `a`..`z`, then `0`..`9`.
fn encode_digit(digit: u64) -> char {
    if digit < 26 {
        char::from(b'a' + digit as u8)
    } else {
        char::from(b'0' + (digit - 26) as u8)
    }
}

/// Digit value of a Punycode character; uppercase letters are accepted because
/// RFC 3492 decoders must treat them like lowercase ones.
fn digit_value(ch: char) -> Option<u64> {
    match ch {
        'a'..='z' => Some(ch as u64 - 'a' as u64),
        'A'..='Z' => Some(ch as u64 - 'A' as u64),
        '0'..='9' => Some(ch as u64 - '0' as u64 + 26),
        _ => None,
    }
}

/// Raw Punycode encoding of a single label (no ACE prefix).
///
/// Returns `None` only on arithmetic overflow, which cannot happen for a label
/// taken from a document: the totals are bounded by
/// `(max code point) * (label length)`, far below `u64::MAX` for any input the
/// 64 MB document cap allows.
fn encode_label(label: &str) -> Option<String> {
    let points: Vec<u64> = label.chars().map(|ch| ch as u64).collect();
    let basic = points
        .iter()
        .filter(|&&point| point < INITIAL_CODE_POINT)
        .count();
    let mut output = String::with_capacity(label.len());

    for &point in points.iter().filter(|&&point| point < INITIAL_CODE_POINT) {
        output.push(char::from_u32(point as u32)?);
    }
    // The delimiter separates the copied basic code points from the encoded
    // remainder, and is only present when there are basic code points.
    if basic > 0 {
        output.push(DELIMITER);
    }

    let mut handled = basic as u64;
    let mut n = INITIAL_CODE_POINT;
    let mut delta = 0u64;
    let mut bias = INITIAL_BIAS;

    while (handled as usize) < points.len() {
        // The smallest unhandled code point at or above `n`.
        let m = points.iter().copied().filter(|&point| point >= n).min()?;
        delta += (m - n) * (handled + 1);
        n = m;

        for &point in &points {
            if point < n {
                delta += 1;
            } else if point == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = threshold(k, bias);
                    if q < t {
                        break;
                    }
                    output.push(encode_digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                output.push(encode_digit(q));
                bias = adapt(delta, handled + 1, handled == basic as u64);
                delta = 0;
                handled += 1;
            }
        }

        delta += 1;
        n += 1;
    }

    Some(output)
}

/// RFC 3492 threshold function shared by the encoder and decoder.
fn threshold(k: u64, bias: u64) -> u64 {
    if k <= bias {
        TMIN
    } else if k >= bias + TMAX {
        TMAX
    } else {
        k - bias
    }
}

/// Raw Punycode decoding of one label's payload (the part after `xn--`).
///
/// Returns `None` for input that is not valid Punycode: non-ASCII basic code
/// points, characters outside the digit alphabet, overflow, or a decoded code
/// point that is not a Unicode scalar value.
fn decode_label(payload: &str) -> Option<String> {
    // The payload may start with the copied basic code points, terminated by the
    // last delimiter; everything after it is the encoded remainder.
    let (basic, encoded) = match payload.rfind(DELIMITER) {
        Some(index) => (&payload[..index], &payload[index + DELIMITER.len_utf8()..]),
        None => ("", payload),
    };

    let mut output: Vec<u32> = Vec::with_capacity(payload.len());
    for ch in basic.chars() {
        if !ch.is_ascii() {
            return None;
        }
        output.push(ch as u32);
    }

    let mut n = INITIAL_CODE_POINT;
    let mut i = 0u64;
    let mut bias = INITIAL_BIAS;

    let mut encoded = encoded.chars().peekable();
    while encoded.peek().is_some() {
        let old_i = i;
        let mut weight = 1u64;
        let mut k = BASE;
        loop {
            let digit = digit_value(encoded.next()?)?;
            i = i.checked_add(digit.checked_mul(weight)?)?;
            let t = threshold(k, bias);
            if digit < t {
                break;
            }
            weight = weight.checked_mul(BASE - t)?;
            k += BASE;
        }

        let length = output.len() as u64 + 1;
        bias = adapt(i - old_i, length, old_i == 0);
        n = n.checked_add(i / length)?;
        i %= length;

        // Rejects surrogates (and anything above U+10FFFF) as invalid Punycode.
        char::from_u32(n as u32)?;
        output.insert(i as usize, n as u32);
        i += 1;
    }

    output.iter().map(|&point| char::from_u32(point)).collect()
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

    fn error_message(result: TransformResult) -> String {
        match result {
            TransformResult::Error(message) => message,
            _ => panic!("expected TransformResult::Error"),
        }
    }

    #[test]
    fn registry_contains_both_transforms() {
        let names: Vec<&str> = TRANSFORMS.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["Punycode Encode", "Punycode Decode"]);
        assert!(
            TRANSFORMS
                .iter()
                .all(|transform| transform.scope == Scope::SelectionOrFullText)
        );
    }

    #[test]
    fn encoding_matches_rfc_3492_samples() {
        assert_eq!(replaced(punycode_encode("bücher")), "xn--bcher-kva");
        assert_eq!(replaced(punycode_encode("münchen")), "xn--mnchen-3ya");
        assert_eq!(replaced(punycode_encode("mañana")), "xn--maana-pta");
        assert_eq!(replaced(punycode_encode("例子")), "xn--fsqu00a");
        assert_eq!(replaced(punycode_encode("日本語")), "xn--wgv71a119e");
        assert_eq!(
            replaced(punycode_encode("他们为什么不说中文")),
            "xn--ihqwcrb4cv8a8dqg056pqjye"
        );
        let arabic = "\u{0644}\u{064A}\u{0647}\u{0645}\u{0627}\u{0628}\u{062A}\u{0643}\
                      \u{0644}\u{0645}\u{0648}\u{0634}\u{0639}\u{0631}\u{0628}\u{064A}\u{061F}";
        assert_eq!(
            replaced(punycode_encode(arabic)),
            "xn--egbpdaj6bu4bxfgehfvwxn"
        );
    }

    #[test]
    fn decoding_restores_the_unicode_labels() {
        assert_eq!(replaced(punycode_decode("xn--bcher-kva")), "bücher");
        assert_eq!(replaced(punycode_decode("xn--mnchen-3ya")), "münchen");
        assert_eq!(replaced(punycode_decode("xn--wgv71a119e")), "日本語");
        assert_eq!(
            replaced(punycode_decode("xn--ihqwcrb4cv8a8dqg056pqjye")),
            "他们为什么不说中文"
        );
        assert_eq!(replaced(punycode_decode("xn--e28h")), "😀");
    }

    #[test]
    fn domains_round_trip_label_by_label() {
        assert_eq!(replaced(punycode_encode("münchen.de")), "xn--mnchen-3ya.de");
        assert_eq!(
            replaced(punycode_encode("пример.рф")),
            "xn--e1afmkfd.xn--p1ai"
        );
        assert_eq!(
            replaced(punycode_decode("xn--e1afmkfd.xn--p1ai")),
            "пример.рф"
        );
        assert_eq!(
            replaced(punycode_decode(&replaced(punycode_encode(
                "www.münchen.example"
            )))),
            "www.münchen.example"
        );
    }

    #[test]
    fn ascii_labels_and_text_pass_through_unchanged() {
        assert_eq!(replaced(punycode_encode("example.com")), "example.com");
        // `a-b` stays `a-b` instead of the raw codec's `a-b-`.
        assert_eq!(replaced(punycode_encode("a-b")), "a-b");
        assert_eq!(replaced(punycode_decode("example.com")), "example.com");
        assert_eq!(replaced(punycode_decode("hello world")), "hello world");
        assert_eq!(replaced(punycode_decode("")), "");
    }

    #[test]
    fn separators_and_lines_are_preserved() {
        assert_eq!(
            replaced(punycode_encode("münchen.de\nexample.com\n")),
            "xn--mnchen-3ya.de\nexample.com\n"
        );
        assert_eq!(
            replaced(punycode_encode("münchen.example.")),
            "xn--mnchen-3ya.example."
        );
    }

    #[test]
    fn ace_prefix_is_matched_case_insensitively() {
        assert_eq!(replaced(punycode_decode("XN--mnchen-3ya")), "münchen");
        // The basic part keeps its case; only the encoded run is normalized.
        assert_eq!(replaced(punycode_decode("xn--MNCHEN-3YA")), "MüNCHEN");
    }

    #[test]
    fn invalid_payloads_are_errors() {
        assert!(error_message(punycode_decode("xn--")).contains("no Punycode payload"));
        assert!(error_message(punycode_decode("xn--!!!")).contains("not valid Punycode"));
        // `ü` cannot appear in the copied basic part.
        assert!(error_message(punycode_decode("xn--ü")).contains("not valid Punycode"));
        assert!(error_message(punycode_decode("xn--mnchen-3ya.xn--")).contains("xn--"));
    }

    #[test]
    fn decoder_accepts_uppercase_digits() {
        // Uppercase digits decode like lowercase; the copied basic code points
        // keep the case they were written with.
        assert_eq!(replaced(punycode_decode("xn--BCHER-KVA")), "BüCHER");
    }
}
