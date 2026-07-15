//! BundleID 字符集与合法性判据（**单一权威**）/ BundleID charset & validity predicate (single source)。
//!
//! 协议依据 / Protocol：`data-structures.md` §BundleID（`#bundleid`）——`bundle_id` 是 MCP Server 的
//! **唯一身份**，字符集 `[A-Za-z0-9_-]`、**MUST NOT** 含 `.`、**MUST NOT** 含连续 `__`（`__` 是
//! `bundle_id` 与工具名之间的保留分隔符）。**协议未对 `bundle_id` 设长度上限**。
//! 对标 Python 参考实现 / mirrors the Python reference：`a2c_smcp/utils/bundle_id.py`
//! （`is_valid_bundle_id`）——两 SDK 判据须逐字节一致。
//!
//! **为何住在 `smcp`（协议 crate）而非 `smcp-computer`**：`bundle_id` 的合法性判据有**两个**消费者——
//! ① `smcp-computer` 的注册边界（`mcp_clients::bundle_id::validate_bundle_id`，显式值校验）；
//! ② 本 crate 的 SKILL 命名（[`crate::skill_name`]：mcp `<server>` 段**就是** `bundle_id`，skill.md §1.3）。
//! 依赖方向是 `smcp-computer → smcp` 单向，故判据必须下沉到此处才能被两者共享。
//!
//! **此处之外不得再写一份等价谓词**：两处一旦漂移（如协议调整 BundleID 字符集），mcp `<server>` 段就会
//! 拒绝合法 `bundle_id` → 该 Server 的 SKILL 对 Agent **隐身**——正是 rust-sdk#127 / python-sdk#142 要
//! 消灭的失效模式**原地复活**。

/// 判定字符是否属 BundleID 字符集 `[A-Za-z0-9_-]`（**显式 ASCII 类**，非 Unicode-aware `\w`）。
///
/// **MUST** 用显式 ASCII 类：各语言 Unicode `\w` 集合不一致，会击穿跨 SDK 逐字节一致性。
#[inline]
#[must_use]
pub fn is_bundle_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `bundle_id` 是否合法：**非空** + **无连续 `__`** + 字符集 `[A-Za-z0-9_-]`（含 `.` 即非法）。
///
/// **无长度上限**——协议 §BundleID 未设，`data-structures.md` §1.4 的 `<server>` 段亦**未**列 1–64
/// （对比 user / marketplace 段明列「1–64」）。擅自加上限会拒绝合法 `bundle_id`（显式长值 / 长 display
/// 名的缺省生成结果），令其 SKILL 隐身，并与 python-sdk 出线分歧。
#[must_use]
pub fn is_valid_bundle_id(bundle_id: &str) -> bool {
    !bundle_id.is_empty() && !bundle_id.contains("__") && bundle_id.chars().all(is_bundle_id_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议 §BundleID 判据（与 python-sdk `is_valid_bundle_id` 逐条对齐）。
    #[test]
    fn valid_bundle_id_matches_protocol_charset_rules() {
        // 合法：字母 / 数字 / 单下划线 / 连字符 / 大小写保留 / hash-fallback 形态。
        for ok in [
            "tfrobot-tools",
            "my_api",
            "acme-editor",
            "MyServer",
            "bundle_a1b2c3d4e5f60718",
            "a",
            "_leading-ok",  // 首尾 `_-` 仅在缺省生成时被裁；**显式**值不禁
            "trailing-ok_", // 同上
        ] {
            assert!(is_valid_bundle_id(ok), "{ok:?} 应合法");
        }

        // 非法：空 / 含 `.` / 含连续 `__` / 非 ASCII / 空格。
        for bad in ["", "my.api", "a__b", "__lead", "trail__", "服务器", "a b"] {
            assert!(!is_valid_bundle_id(bad), "{bad:?} 应非法");
        }
    }

    /// **无长度上限**（协议 §BundleID 未设）——守护「勿擅自加 64 上限」这一跨 SDK 分歧点。
    #[test]
    fn valid_bundle_id_has_no_length_cap() {
        assert!(is_valid_bundle_id(&"a".repeat(65)));
        assert!(is_valid_bundle_id(&"a".repeat(256)));
    }
}
