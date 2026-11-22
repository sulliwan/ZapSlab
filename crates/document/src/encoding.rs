//! Text ⇄ bytes conversion for the hex view.
//!
//! The document's source of truth is the Unicode `String`; the hex pane shows
//! that text encoded with the selected [`Encoding`]. Converting encodings
//! therefore happens at the byte boundary:
//!
//! - "reinterpret bytes" = `decode(encode(text, old), new)`, which leaves the
//!   bytes unchanged and replaces the text;
//! - "re-encode text" = keep the text and change which encoding the hex pane
//!   uses to display its bytes.

use std::fmt;

/// Byte encodings understood by the hex view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
    Latin1,
    Ascii,
}

/// All encodings, in presentation order.
pub const ALL: &[Encoding] = &[
    Encoding::Utf8,
    Encoding::Utf16Le,
    Encoding::Utf16Be,
    Encoding::Utf32Le,
    Encoding::Utf32Be,
    Encoding::Latin1,
    Encoding::Ascii,
];

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Utf32Le => "UTF-32 LE",
            Encoding::Utf32Be => "UTF-32 BE",
            Encoding::Latin1 => "Latin-1",
            Encoding::Ascii => "ASCII",
        }
    }

    fn is_little_endian(self) -> bool {
        matches!(self, Encoding::Utf16Le | Encoding::Utf32Le)
    }
}

/// A code point that the target encoding cannot represent losslessly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeError {
    pub character: char,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "U+{:04X} is not representable", self.character as u32)
    }
}

/// Why a byte sequence could not be decoded as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    InvalidUtf8,
    OddLength,
    UnpairedSurrogate,
    InvalidScalar,
    NonAscii,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            DecodeError::InvalidUtf8 => "invalid UTF-8 sequence",
            DecodeError::OddLength => "byte count is not a multiple of the unit size",
            DecodeError::UnpairedSurrogate => "unpaired UTF-16 surrogate",
            DecodeError::InvalidScalar => "invalid Unicode scalar value",
            DecodeError::NonAscii => "byte is not valid ASCII",
        };
        f.write_str(message)
    }
}

/// Encodes `text` with `encoding`, failing when a code point cannot be
/// represented (Latin-1 and ASCII are not closed over all of Unicode).
pub fn encode(text: &str, encoding: Encoding) -> Result<Vec<u8>, EncodeError> {
    match encoding {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let mut out = Vec::with_capacity(text.len() * 2);
            let mut units = [0u16; 2];
            for ch in text.chars() {
                for unit in ch.encode_utf16(&mut units).iter() {
                    let bytes = if encoding.is_little_endian() {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    };
                    out.extend_from_slice(&bytes);
                }
            }
            Ok(out)
        }
        Encoding::Utf32Le | Encoding::Utf32Be => {
            let mut out = Vec::with_capacity(text.len() * 4);
            for ch in text.chars() {
                let scalar = ch as u32;
                let bytes = if encoding.is_little_endian() {
                    scalar.to_le_bytes()
                } else {
                    scalar.to_be_bytes()
                };
                out.extend_from_slice(&bytes);
            }
            Ok(out)
        }
        Encoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            for ch in text.chars() {
                let scalar = ch as u32;
                if scalar > 0xFF {
                    return Err(EncodeError { character: ch });
                }
                out.push(scalar as u8);
            }
            Ok(out)
        }
        Encoding::Ascii => {
            let mut out = Vec::with_capacity(text.len());
            for ch in text.chars() {
                if !ch.is_ascii() {
                    return Err(EncodeError { character: ch });
                }
                out.push(ch as u8);
            }
            Ok(out)
        }
    }
}

/// Encoded length of `text` in bytes, without allocating. Fails on a code point
/// that the encoding cannot represent, exactly like [`encode`].
pub fn encoded_len(text: &str, encoding: Encoding) -> Result<usize, EncodeError> {
    match encoding {
        Encoding::Utf8 => Ok(text.len()),
        Encoding::Utf16Le | Encoding::Utf16Be => Ok(text.encode_utf16().count() * 2),
        Encoding::Utf32Le | Encoding::Utf32Be => Ok(text.chars().count() * 4),
        Encoding::Latin1 => {
            for ch in text.chars() {
                if ch as u32 > 0xFF {
                    return Err(EncodeError { character: ch });
                }
            }
            Ok(text.chars().count())
        }
        Encoding::Ascii => {
            for ch in text.chars() {
                if !ch.is_ascii() {
                    return Err(EncodeError { character: ch });
                }
            }
            Ok(text.len())
        }
    }
}

/// Decodes `bytes` with `encoding`, failing on byte sequences that are not
/// valid text in that encoding.
pub fn decode(bytes: &[u8], encoding: Encoding) -> Result<String, DecodeError> {
    match encoding {
        Encoding::Utf8 => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| DecodeError::InvalidUtf8),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            if bytes.len() % 2 != 0 {
                return Err(DecodeError::OddLength);
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| {
                    let raw = [pair[0], pair[1]];
                    if encoding.is_little_endian() {
                        u16::from_le_bytes(raw)
                    } else {
                        u16::from_be_bytes(raw)
                    }
                })
                .collect();
            String::from_utf16(&units).map_err(|_| DecodeError::UnpairedSurrogate)
        }
        Encoding::Utf32Le | Encoding::Utf32Be => {
            if bytes.len() % 4 != 0 {
                return Err(DecodeError::OddLength);
            }
            let mut out = String::with_capacity(bytes.len());
            for chunk in bytes.chunks_exact(4) {
                let raw = [chunk[0], chunk[1], chunk[2], chunk[3]];
                let scalar = if encoding.is_little_endian() {
                    u32::from_le_bytes(raw)
                } else {
                    u32::from_be_bytes(raw)
                };
                match char::from_u32(scalar) {
                    Some(ch) => out.push(ch),
                    None => return Err(DecodeError::InvalidScalar),
                }
            }
            Ok(out)
        }
        Encoding::Latin1 => Ok(bytes.iter().map(|&byte| byte as char).collect()),
        Encoding::Ascii => {
            let mut out = String::with_capacity(bytes.len());
            for &byte in bytes {
                if byte > 0x7F {
                    return Err(DecodeError::NonAscii);
                }
                out.push(byte as char);
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str, encoding: Encoding) {
        let bytes = encode(text, encoding).expect("encode");
        let decoded = decode(&bytes, encoding).expect("decode");
        assert_eq!(decoded, text, "{}", encoding.label());
    }

    #[test]
    fn unicode_encodings_round_trip() {
        for text in ["hello", "héllo", "a😀b", "\u{1F468}\u{200D}\u{1F469}"] {
            for encoding in [
                Encoding::Utf8,
                Encoding::Utf16Le,
                Encoding::Utf16Be,
                Encoding::Utf32Le,
                Encoding::Utf32Be,
            ] {
                round_trip(text, encoding);
            }
        }
    }

    #[test]
    fn latin1_round_trips_and_rejects_unrepresentable() {
        round_trip("héllo", Encoding::Latin1);
        let error = encode("a😀", Encoding::Latin1).unwrap_err();
        assert_eq!(error.character, '😀');
    }

    #[test]
    fn ascii_round_trips_and_rejects_non_ascii() {
        round_trip("hello", Encoding::Ascii);
        assert!(encode("héllo", Encoding::Ascii).is_err());
    }

    #[test]
    fn invalid_byte_sequences_are_rejected() {
        assert_eq!(
            decode(&[0xFF], Encoding::Utf8),
            Err(DecodeError::InvalidUtf8)
        );
        assert_eq!(
            decode(&[0x00], Encoding::Utf16Le),
            Err(DecodeError::OddLength)
        );
        assert_eq!(
            decode(&[0x00, 0xD8], Encoding::Utf16Le),
            Err(DecodeError::UnpairedSurrogate)
        );
        assert_eq!(
            decode(&[0x00, 0x11, 0x00, 0x00], Encoding::Utf32Be),
            Err(DecodeError::InvalidScalar)
        );
        assert_eq!(decode(&[0x80], Encoding::Ascii), Err(DecodeError::NonAscii));
    }

    #[test]
    fn latin1_maps_bytes_to_code_points() {
        assert_eq!(decode(&[0xE9], Encoding::Latin1).unwrap(), "é");
        assert_eq!(encode("é", Encoding::Latin1).unwrap(), vec![0xE9]);
    }

    #[test]
    fn utf16_endianness_matters() {
        assert_eq!(encode("A", Encoding::Utf16Le).unwrap(), vec![0x41, 0x00]);
        assert_eq!(encode("A", Encoding::Utf16Be).unwrap(), vec![0x00, 0x41]);
    }

    #[test]
    fn encoded_len_matches_encode() {
        for text in ["", "hello", "héllo", "a😀b", "\u{1F468}\u{200D}\u{1F469}"] {
            for encoding in ALL {
                assert_eq!(
                    encoded_len(text, *encoding).map(|len| len),
                    encode(text, *encoding).map(|bytes| bytes.len()),
                    "{} for {text:?}",
                    encoding.label()
                );
            }
        }
        // Unrepresentable code points report the same error as `encode`.
        assert!(encoded_len("😀", Encoding::Latin1).is_err());
        assert!(encoded_len("é", Encoding::Ascii).is_err());
    }
}
