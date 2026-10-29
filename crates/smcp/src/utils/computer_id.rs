/*!
* 文件名: computer_id.rs
* 描述: Computer 稳定身份 `computer_id` 的规范形态谓词 / Canonical Computer identity predicate
*/

//! protocol#64（v0.5.0）`computer_id` 的**规范形态**判定——Server 入房校验
//! （`smcp-server-core`）与 Computer 本地文件解析（`smcp-computer`）的**单一权威**。
//!
//! 规则（`data-structures.md` §EnterOfficeReq、protocol#74）：
//! **整串**匹配，恰好 36 个字符，连字符位于 8/13/18/23，其余为小写十六进制；
//! 拒绝 nil / max。首尾空白或结尾换行因长度/字符集不符而非法（协议要求 `400`）。
//! 接收方 MUST 视为不透明字符串（按字节相等比较，不解析版本位）。

/// 规范 UUID 中的 nil / max：协议禁止 Computer 发出。
pub const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";
/// 规范 UUID 中的 max：协议禁止 Computer 发出。
pub const MAX_UUID: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

/// 是否为合法的 `computer_id` 形态（整串、规范小写 UUID、非 nil/max）。
///
/// 空串、大写、无连字符、非 UUID 文本、首尾空白、结尾换行都返回 `false`。
pub fn is_canonical_computer_id(value: &str) -> bool {
    if value == NIL_UUID || value == MAX_UUID {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// protocol#64 / protocol#74 的形态矩阵（Server 与 Computer 两侧共同依赖本谓词）。
    #[test]
    fn canonical_computer_id_matrix() {
        assert!(is_canonical_computer_id(
            "0f8fad5b-d9cb-469f-a165-70867728950e"
        ));
        assert!(is_canonical_computer_id(
            "ffffffff-ffff-4fff-8fff-fffffffffffe"
        ));
        for invalid in [
            "",
            "0F8FAD5B-D9CB-469F-A165-70867728950E",     // 大写
            "0f8fad5bd9cb469fa16570867728950e",         // 无连字符
            "0f8fad5b-d9cb-469f-a165-70867728950",      // 短一位
            "0f8fad5b-d9cb-469f-a165-70867728950e0",    // 长一位
            "not-a-uuid",                               // 非 UUID 文本
            "zz9e6679-7425-40de-944b-e07fc1f90ae7",     // 非十六进制
            NIL_UUID,                                   // nil
            MAX_UUID,                                   // max
            " 0f8fad5b-d9cb-469f-a165-70867728950e",    // 前导空白
            "0f8fad5b-d9cb-469f-a165-70867728950e ",    // 结尾空白
            "0f8fad5b-d9cb-469f-a165-70867728950e\n",   // 结尾换行
            "0f8fad5b-d9cb-469f-a165-70867728950e\r\n", // CRLF
        ] {
            assert!(!is_canonical_computer_id(invalid), "{invalid:?} 不应通过");
        }
    }
}
