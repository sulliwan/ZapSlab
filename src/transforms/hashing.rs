//! Cryptographic checksums of the input text, ported from Boop's hash scripts.
//!
//! Boop hashes the JavaScript string's UTF-8 bytes and renders lowercase hex;
//! these wrappers do the same via the RustCrypto/`sha1_smol` crates.

use super::{Scope, Transform, TransformResult};
use md5::{Digest, Md5};
use sha1_smol::Sha1;
use sha2::{Sha256, Sha512};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "MD5 Checksum",
        description: "Compute the MD5 digest of the UTF-8 bytes as lowercase hex.",
        tags: "md5,checksum,hash,digest",
        scope: Scope::SelectionOrFullText,
        apply: md5_hex,
    },
    Transform {
        name: "SHA1 Hash",
        description: "Compute the SHA-1 digest of the UTF-8 bytes as lowercase hex.",
        tags: "sha1,checksum,hash,digest",
        scope: Scope::SelectionOrFullText,
        apply: sha1_hex,
    },
    Transform {
        name: "SHA256 Hash",
        description: "Compute the SHA-256 digest of the UTF-8 bytes as lowercase hex.",
        tags: "sha256,checksum,hash,digest",
        scope: Scope::SelectionOrFullText,
        apply: sha256_hex,
    },
    Transform {
        name: "SHA512 Hash",
        description: "Compute the SHA-512 digest of the UTF-8 bytes as lowercase hex.",
        tags: "sha512,checksum,hash,digest",
        scope: Scope::SelectionOrFullText,
        apply: sha512_hex,
    },
];

fn md5_hex(text: &str) -> TransformResult {
    let digest = Md5::digest(text.as_bytes());
    TransformResult::replace(format!("{digest:x}"))
}

fn sha1_hex(text: &str) -> TransformResult {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    TransformResult::replace(hasher.digest().to_string())
}

fn sha256_hex(text: &str) -> TransformResult {
    let digest = Sha256::digest(text.as_bytes());
    TransformResult::replace(format!("{digest:x}"))
}

fn sha512_hex(text: &str) -> TransformResult {
    let digest = Sha512::digest(text.as_bytes());
    TransformResult::replace(format!("{digest:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashed(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected a replacement"),
        }
    }

    #[test]
    fn registers_boop_names_in_order() {
        let names: Vec<&str> = TRANSFORMS.iter().map(|transform| transform.name).collect();
        assert_eq!(
            names,
            vec![
                "MD5 Checksum",
                "SHA1 Hash",
                "SHA256 Hash",
                "SHA512 Hash",
            ]
        );
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(hashed(md5_hex("hello")), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(hashed(md5_hex("")), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn sha1_matches_known_vector() {
        assert_eq!(
            hashed(sha1_hex("hello")),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            hashed(sha256_hex("hello")),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha512_matches_known_vector() {
        assert_eq!(
            hashed(sha512_hex("hello")),
            "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
        );
    }

    #[test]
    fn hashes_utf8_bytes() {
        // U+00E9 is two bytes in UTF-8; the digest must be over `c3 a9`.
        assert_eq!(hashed(md5_hex("é")), "66ddcd97cfdeabb2f6fb8a999b4bc76f");
    }
}
