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
/// 先 `trim().to_ascii_lowercase()`，命中 `TRUTHY`（`1` / `true` / `yes` / `on`）为真；
/// 空白或其它值（如 `0` / `false` / `off`）→ `false`。
///
/// Trims + lowercases, then checks membership in `TRUTHY`; blank or anything else → `false`.
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
/// 取 `std::env::var(key)` 后 `trim().to_ascii_lowercase()`：命中 `TRUTHY` 为真；
/// **未设置或空白** → `default`；其它值（如 `0` / `false` / `off`）→ `false`。
/// 语义与 Python `env_truthy(key, *, default)` 完全一致。
///
/// Reads `std::env::var(key)`; unset or blank → `default`; truthy set → `true`; otherwise `false`.
pub fn env_truthy_or(key: &str, default: bool) -> bool {
    env_truthy_in(key, default, |k| std::env::var(k).ok())
}

/// 注入式真值判定 / Truthiness with an injectable getter（便于确定性测试与自定义环境映射）。
///
/// 镜像 Python `env_truthy(key, *, default, env=...)` 的 `env` 注入参：`getter(key)` 返回 `None`
/// 视为未设置 → `default`；返回空白 → `default`；否则按 [`is_truthy`] 判定（如 `0`/`off` → `false`）。
/// `env_truthy_or` 即以 `std::env::var` 作为 `getter` 的特化。
///
/// Mirrors the Python `env=` injection so the present-but-falsy / present-but-truthy / unset paths
/// are testable without mutating the process environment.
pub fn env_truthy_in(key: &str, default: bool, getter: impl Fn(&str) -> Option<String>) -> bool {
    match getter(key) {
        Some(raw) => {
            let norm = raw.trim();
            if norm.is_empty() {
                default
            } else {
                is_truthy(norm)
            }
        }
        None => default,
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

    #[test]
    fn test_env_truthy_in_injected_states() {
        // 注入式：三态确定性覆盖（无需写进程环境）。
        // 用 fn item（Copy、零捕获）按值传入，避免把 `&闭包` 传给泛型 `impl Fn` 触发
        // clippy::needless_borrows_for_generic_args。
        fn env(k: &str) -> Option<String> {
            match k {
                "T" => Some("on".to_string()),      // 已设置真值
                "F" => Some("0".to_string()),       // 已设置假值
                "OFF" => Some(" OFF ".to_string()), // 已设置假值（含空白/大小写）
                "BLANK" => Some("   ".to_string()), // 空白
                _ => None,                          // 未设置
            }
        }
        assert!(env_truthy_in("T", false, env)); // 真值 → true（default 不生效）
        assert!(!env_truthy_in("F", true, env)); // 假值 → false（default 不生效）← 关键：已设置但为假
        assert!(!env_truthy_in("OFF", true, env)); // off → false
        assert!(env_truthy_in("BLANK", true, env)); // 空白 → default
        assert!(!env_truthy_in("BLANK", false, env));
        assert!(!env_truthy_in("MISSING", false, env)); // 未设置 → default
        assert!(env_truthy_in("MISSING", true, env));
    }
}
