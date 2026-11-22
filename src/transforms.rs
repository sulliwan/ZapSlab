//! Text transformations available from the transform palette.
//!
//! Keep this registry small and pure: each entry maps selected text to new
//! text. Adding a transform is one `Transform` entry.

use base64::Engine as _;

pub struct Transform {
    pub name: &'static str,
    pub apply: fn(&str) -> String,
}

pub const TRANSFORMS: &[Transform] = &[Transform {
    name: "Base64 encode",
    apply: base64_encode,
}];

fn base64_encode(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let encoded = base64_encode("hello, boop");
        assert_eq!(encoded, "aGVsbG8sIGJvb3A=");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            b"hello, boop"
        );
    }
}
