//! 协议版本语义与兼容性判定 / Protocol version semantics & compatibility。
//!
//! 严格实现 a2c-smcp-protocol `versioning.md` §兼容性判定规则 的参考算法：
//!   - v0.x（MAJOR=0，不稳定阶段）：MAJOR 与 MINOR **必须严格匹配**，PATCH 自由
//!   - v1.0+（稳定阶段）：MAJOR 严格匹配，Server MINOR **≥** Client MINOR，PATCH 自由
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/version.py`。
//!
//! 供 Server 端 HTTP 握手中间件复用（client 侧只声明版本、解析 4008，不做兼容判定——
//! 判定权属 Server）。Reused by the Server-side HTTP handshake middleware.

use std::fmt;
use std::str::FromStr;

/// 版本号解析错误 / Version parse error。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid version: {0}")]
pub struct ProtocolVersionParseError(pub String);

/// 语义化版本三元组 / Semantic version triple (`MAJOR.MINOR.PATCH`)。
///
/// 顺序按 (major, minor, patch) 字典序（`derive(Ord)` 字段声明序），对齐 Python `order=True`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProtocolVersion {
    /// 主版本号 / major.
    pub major: u64,
    /// 次版本号 / minor.
    pub minor: u64,
    /// 修订号 / patch.
    pub patch: u64,
}

impl ProtocolVersion {
    /// 构造版本三元组 / Construct a version triple.
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// 解析 `MAJOR.MINOR.PATCH`；段数不为 3 或非整数均返回错误。
    /// Parse `MAJOR.MINOR.PATCH`; not exactly 3 integer parts → error。
    ///
    /// **严格 3 段**：对齐协议 `versioning.md` 参考实现与 Python `version.py`——`"0.2"`（2 段）
    /// 视为非法。SDK 暴露的 [`crate::PROTOCOL_VERSION`] 恒为 3 段，握手 query 始终传 3 段。
    ///
    /// # Errors
    /// 段数 ≠ 3、含空段或非数字段（含负号 / 溢出 `u64`）时返回 [`ProtocolVersionParseError`]。
    pub fn parse(s: &str) -> Result<Self, ProtocolVersionParseError> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(ProtocolVersionParseError(s.to_string()));
        }
        let parse_one = |p: &str| {
            p.parse::<u64>()
                .map_err(|_| ProtocolVersionParseError(s.to_string()))
        };
        Ok(Self {
            major: parse_one(parts[0])?,
            minor: parse_one(parts[1])?,
            patch: parse_one(parts[2])?,
        })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ProtocolVersion {
    type Err = ProtocolVersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// 判定 Client 是否能连接到 Server / Whether the Client may connect to the Server。
///
/// `versioning.md` §判定函数参考实现：
/// - MAJOR 不同 → 不兼容
/// - v0.x（client.major == 0）→ MINOR 严格相等
/// - v1.0+ → 向后兼容（`client.minor <= server.minor`，较新 Server 接纳较旧 Client）
///
/// PATCH 在任何阶段都不影响兼容性。
pub fn is_compatible(client: &ProtocolVersion, server: &ProtocolVersion) -> bool {
    if client.major != server.major {
        return false;
    }
    if client.major == 0 {
        // v0.x 严格匹配 MINOR / strict MINOR match in v0.x
        return client.minor == server.minor;
    }
    // v1.0+ 向后兼容（Server MINOR >= Client MINOR）/ backward compatible
    client.minor <= server.minor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid() {
        assert_eq!(
            ProtocolVersion::parse("0.2.0").unwrap(),
            ProtocolVersion::new(0, 2, 0)
        );
        assert_eq!(
            ProtocolVersion::parse("1.5.2").unwrap(),
            ProtocolVersion::new(1, 5, 2)
        );
        assert_eq!(
            ProtocolVersion::parse("10.20.30").unwrap(),
            ProtocolVersion::new(10, 20, 30)
        );
        assert_eq!(
            ProtocolVersion::parse("0.0.0").unwrap(),
            ProtocolVersion::new(0, 0, 0)
        );
        // FromStr 等价路径
        assert_eq!(
            "0.2.1".parse::<ProtocolVersion>().unwrap(),
            ProtocolVersion::new(0, 2, 1)
        );
    }

    #[test]
    fn test_parse_invalid_strict_three_segments() {
        // 严格 3 段：'0.2'（2 段）非法（对齐协议 + Python，#45 用户裁定）
        for s in [
            "", "0.2", "0", "0.2.0.1", "abc", "0.x.0", "0.2.", ".2.0", "0..0", "-1.0.0", "0.2.0 ",
            " 0.2.0", "v0.2.0",
        ] {
            assert!(ProtocolVersion::parse(s).is_err(), "{s:?} 应解析失败");
        }
    }

    #[test]
    fn test_display_roundtrip() {
        for s in ["0.2.0", "1.5.2", "10.20.30", "0.0.0"] {
            let v = ProtocolVersion::parse(s).unwrap();
            assert_eq!(v.to_string(), s);
            assert_eq!(ProtocolVersion::parse(&v.to_string()).unwrap(), v);
        }
    }

    #[test]
    fn test_is_compatible_v0_strict_minor() {
        let v020 = ProtocolVersion::new(0, 2, 0);
        // 同 0.2.x 互兼容（PATCH 自由）
        assert!(is_compatible(&v020, &ProtocolVersion::new(0, 2, 1)));
        assert!(is_compatible(&ProtocolVersion::new(0, 2, 99), &v020));
        // 不同 MINOR（v0.x）不兼容（两向）
        assert!(!is_compatible(&ProtocolVersion::new(0, 1, 0), &v020));
        assert!(!is_compatible(&ProtocolVersion::new(0, 3, 0), &v020));
        assert!(!is_compatible(&v020, &ProtocolVersion::new(0, 3, 0)));
    }

    #[test]
    fn test_is_compatible_v1_backward() {
        // v1.0+ 向后兼容：较旧 client 连较新 server → 兼容
        assert!(is_compatible(
            &ProtocolVersion::new(1, 0, 0),
            &ProtocolVersion::new(1, 2, 0)
        ));
        assert!(is_compatible(
            &ProtocolVersion::new(1, 2, 5),
            &ProtocolVersion::new(1, 2, 0)
        ));
        // 较新 client 连较旧 server → 不兼容
        assert!(!is_compatible(
            &ProtocolVersion::new(1, 2, 0),
            &ProtocolVersion::new(1, 0, 0)
        ));
    }

    #[test]
    fn test_is_compatible_major_mismatch() {
        assert!(!is_compatible(
            &ProtocolVersion::new(1, 0, 0),
            &ProtocolVersion::new(2, 0, 0)
        ));
        assert!(!is_compatible(
            &ProtocolVersion::new(0, 2, 0),
            &ProtocolVersion::new(1, 2, 0)
        ));
    }

    #[test]
    fn test_ordering() {
        assert!(ProtocolVersion::new(0, 2, 0) < ProtocolVersion::new(0, 2, 1));
        assert!(ProtocolVersion::new(0, 2, 1) < ProtocolVersion::new(0, 3, 0));
        assert!(ProtocolVersion::new(0, 3, 0) < ProtocolVersion::new(1, 0, 0));
    }

    #[test]
    fn test_protocol_version_constant_parses() {
        // 暴露的 PROTOCOL_VERSION 常量必须是合法 3 段版本
        let v = ProtocolVersion::parse(crate::PROTOCOL_VERSION).unwrap();
        assert_eq!(v, ProtocolVersion::new(0, 2, 0));
    }
}
