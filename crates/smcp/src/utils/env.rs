//! 环境变量解析工具 / Environment-variable parsing helpers。
//!
//! 统一布尔环境变量的**真值集**，避免各处内联 `... .to_lowercase()` 解析因集合不一致而漂移
//! （如某处认 `"on"` 而另一处不认）。
//! Unifies the canonical truth set for boolean env vars so inline parses don't drift across modules.
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/utils/env.py`。

/// 规范真值集 / Canonical truthy set（解析前统一 `trim().to_ascii_lowercase()`，故此处只列小写裸值）。
///
/// Canonical truthy set (inputs are `trim().to_ascii_lowercase()`-normalized first).
const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

/// 判断**值**是否为真 / Whether a value is truthy（值级谓词）。
///
/// 先 `trim().to_ascii_lowercase()`，命中 [`TRUTHY`]（`1` / `true` / `yes` / `on`）为真；
/// 空白或其它值（如 `0` / `false` / `off`）→ `false`。
///
/// Trims + lowercases, then checks membership in [`TRUTHY`]; blank or anything else → `false`.
pub fn is_truthy(value: &str) -> bool {
    let norm = value.trim().to_ascii_lowercase();
    TRUTHY.contains(&norm.as_str())
}

/// 判断布尔型环境变量是否为真（未设置 → `false`）/ Whether a boolean env var is truthy (unset → `false`)。
///
/// 等价于 [`env_truthy_or`]`(key, false)`。
pub fn env_truthy(key: &str) -> bool {
    env_truthy_or(key, false)
}

/// 判断布尔型环境变量是否为真，可指定回退 / Whether a boolean env var is truthy, with a fallback。
///
/// 取 `std::env::var(key)` 后 `trim().to_ascii_lowercase()`：命中 [`TRUTHY`] 为真；
/// **未设置或空白** → `default`；其它值（如 `0` / `false` / `off`）→ `false`。
/// 语义与 Python `env_truthy(key, *, default)` 完全一致。
///
/// Reads `std::env::var(key)`; unset or blank → `default`; truthy set → `true`; otherwise `false`.
pub fn env_truthy_or(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(raw) => {
            let norm = raw.trim();
            if norm.is_empty() {
                default
            } else {
                is_truthy(norm)
            }
        }
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_truthy_table() {
        // 真值（含大小写 / 前后空白 / 混合）/ truthy (case-insensitive, surrounding whitespace tolerated)
        for v in [
            "1", "true", "TRUE", "True", "yes", "YES", "on", "ON", "  on  ", "\tTrue\n",
        ] {
            assert!(is_truthy(v), "{v:?} 应判定为真");
        }
        // 假值 / falsy
        for v in [
            "0", "false", "FALSE", "off", "no", "n", "", "   ", "2", "enable", "y", "t",
        ] {
            assert!(!is_truthy(v), "{v:?} 应判定为假");
        }
    }

    #[test]
    fn test_env_truthy_unset_uses_default() {
        // 用一个几乎不可能被设置的变量名，验证未设置时回退 default
        let key = "A2C_SMCP_ENV_TRUTHY_DEFINITELY_UNSET_VAR_XYZ";
        assert!(!env_truthy(key)); // 默认 false
        assert!(!env_truthy_or(key, false));
        assert!(env_truthy_or(key, true)); // 未设置 → default=true
    }
}
