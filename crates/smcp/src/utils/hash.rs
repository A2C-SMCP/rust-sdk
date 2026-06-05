//! sha256 + 十六进制编码工具 / sha256 + hex helpers。
//!
//! 统一治理层散落的「digest→hex」副本（blob drain `utils::blob`、skill `resource`/`staging`、
//! blob `toolspool`）到单一真相源，消除重复的 hex-loop / sha256+hex 逻辑。
//!
//! Single source of truth for the digest→hex helpers previously duplicated across the governance layer.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// 字节切片 → 小写十六进制 / Lowercase hex of a byte slice。
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// 计算字节的 sha256 十六进制（小写）/ Lowercase sha256 hex of bytes。
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hex_lowercase_padded() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn sha256_hex_known_vectors() {
        // NIST 已知向量 / known vectors。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
