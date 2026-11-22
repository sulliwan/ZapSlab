//! Color conversions ported from Boop's `hex2rgb.js`.

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[Transform {
    name: "Hex to RGB",
    description: "Convert color in hexadecimal to RGB.",
    tags: "hex,color,rgb,convert",
    scope: Scope::SelectionOrFullText,
    apply: hex_to_rgb,
}];

fn hex_to_rgb(text: &str) -> TransformResult {
    // `cutHex`: drop a leading `#` and keep at most six characters. Boop then
    // slices that buffer into three two-character components; short input
    // yields short (or empty) slices, which `parseInt` turns into `NaN`.
    let cut: Vec<char> = if text.starts_with('#') {
        text.chars().skip(1).take(6).collect()
    } else {
        text.chars().collect()
    };

    let red = component(&cut, 0, 2);
    let green = component(&cut, 2, 4);
    let blue = component(&cut, 4, 6);
    TransformResult::replace(format!("{red},{green},{blue}"))
}

fn component(cut: &[char], start: usize, end: usize) -> String {
    let slice: String = cut.iter().copied().skip(start).take(end - start).collect();
    match parse_int16(&slice) {
        Some(value) => value.to_string(),
        None => "NaN".to_string(),
    }
}

/// JavaScript's `parseInt(s, 16)` for the short slices above.
fn parse_int16(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (negative, rest) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest)
    } else {
        (false, s)
    };
    let rest = rest
        .strip_prefix("0x")
        .or_else(|| rest.strip_prefix("0X"))
        .unwrap_or(rest);

    let mut value: i64 = 0;
    let mut found = false;
    for c in rest.chars() {
        let Some(digit) = c.to_digit(16) else {
            break;
        };
        value = value * 16 + i64::from(digit);
        found = true;
    }

    if !found {
        return None;
    }
    Some(if negative { -value } else { value })
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

    #[test]
    fn registers_boop_metadata() {
        assert_eq!(TRANSFORMS.len(), 1);
        assert_eq!(TRANSFORMS[0].name, "Hex to RGB");
        assert_eq!(TRANSFORMS[0].description, "Convert color in hexadecimal to RGB.");
        assert_eq!(TRANSFORMS[0].tags, "hex,color,rgb,convert");
    }

    #[test]
    fn converts_six_digit_hex() {
        assert_eq!(replaced(hex_to_rgb("#ff0000")), "255,0,0");
        assert_eq!(replaced(hex_to_rgb("00ff00")), "0,255,0");
        assert_eq!(replaced(hex_to_rgb("#0000ff")), "0,0,255");
    }

    #[test]
    fn handles_short_and_malformed_input_like_boop() {
        assert_eq!(replaced(hex_to_rgb("#f00")), "240,0,NaN");
        assert_eq!(replaced(hex_to_rgb("#zz")), "NaN,NaN,NaN");
        assert_eq!(replaced(hex_to_rgb("#")), "NaN,NaN,NaN");
    }
}
