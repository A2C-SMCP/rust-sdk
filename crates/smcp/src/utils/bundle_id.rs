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
//!
//! # [`BundleId`] newtype：构造即校验（parse, don't validate）—— rust-sdk#130
//!
//! `bundle_id`（身份，给代码）与 `server_name`（display 名，给人）此前**同为 `String` 别名**，对编译器
//! **完全同型** ⇒ 混用永不报错，`#117 → #118 → #121 → #127` 四轮只能靠人眼扫、扫漏处编译器一声不吭
//! （本仓 `mcp_clients::manager` 自陈「同为 `HashMap<String, _>`、类型上无从分辨」）。
//!
//! [`BundleId`] 令该混用**在类型层不可能**：其**存在即合法**（由 [`TryFrom`] 构造保证，判决完全委托
//! [`is_valid_bundle_id`]）⇒ #127 那类「注册边界放行、SKILL 层判废 → 合法 SKILL 对 Agent 隐身」的
//! **规则集分歧**在类型上无从发生。
//!
//! **非对称是有意的**：`ServerName` / `ExposedToolName` 保持 `String`——display 名混用无害（反正给人看），
//! 不值得付 newtype 的 `.0` / `.as_str()` 噪声。
//!
//! **刻意不实现 `Borrow<str>`**：否则 `HashMap<BundleId, _>::get(&str)` 仍编译，用 display 名查身份键表
//! 这一**核心混用点**会继续静默通过——那正是本 newtype 要消灭的东西。代价是查表需显式转换，值得。
//!
//! # ⚠️ 本轮关掉的是**查表面**，不是全部（勿高估保证）
//!
//! [`PartialEq<&str>`] 仍在（见其实现处的理由：pub API 暂收 `&str`，rust-sdk#130 的"紧边界"）⇒
//! **name-join 式比较**（`resolve_bundle_id(&cfg) == some_display_name`）**依然编译通过**。而 #126/#127 的
//! 真实 bug 恰是 name-join 比较（`bundled.contains(name)`、按 display 名关联归属），不是 map 查表。
//!
//! 即：本轮令「用 name **查身份键表**」编译红；「用 name **与身份比较**」仍需人眼。后者随 **#141**（库层
//! 公开 API 一律收 `BundleId`）移除 `PartialEq<&str>` 后收口。

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

/// `bundle_id` 校验错误（结构化分类）/ structured bundle_id validation error。
///
/// 仅用于**显式**配置值与外部输入；缺省生成结果恒合法（见 `smcp-computer` 的 `derive_bundle_id`）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleIdError {
    /// 显式 `bundle_id` 为空 / explicit bundle_id is empty。
    #[error("bundle_id must not be empty")]
    Empty,
    /// 含保留分隔符 `__`（BundleID 与工具名的分隔符，禁出现于 BundleID 内）/ contains reserved `__`。
    #[error("bundle_id '{0}' must not contain the reserved separator '__'")]
    ReservedSeparator(String),
    /// 含字符集 `[A-Za-z0-9_-]` 之外的字符（含 `.`）/ contains a char outside `[A-Za-z0-9_-]`。
    #[error(
        "bundle_id '{value}' contains illegal character '{ch}' (allowed charset: [A-Za-z0-9_-])"
    )]
    IllegalChar {
        /// 违规的完整值 / the offending value。
        value: String,
        /// 首个违规字符 / first offending character。
        ch: char,
    },
}

/// 校验 `bundle_id`：非空、无 `__`、字符集 `[A-Za-z0-9_-]`（含 `.` 判为 [`BundleIdError::IllegalChar`]）。
///
/// **判决完全委托** [`is_valid_bundle_id`]（单一权威），本函数**只在已知非法时**做结构化**分类**——
/// accept/reject 的边界因此**由构造保证**与 SKILL 侧（[`crate::skill_name`] 的 mcp `<server>` 段）一致，
/// 而非靠人同步维护两份规则集。
///
/// 曾经的写法是在此重写一遍「非空 + 字符集 + 无 `__`」：彼时与权威等价纯属巧合，协议一旦新增规则（如
/// 「MUST NOT 以 `-` 开头」）而只改权威，注册边界就会**放行**、SKILL 层却**判废** → 合法 `bundle_id` 的
/// SKILL 对 Agent 隐身，即 rust-sdk#127 / python-sdk#142 的失效模式上移一层复发。
///
/// # Errors
/// 值非法时返回结构化分类（[`BundleIdError`]）。
pub fn validate_bundle_id(value: &str) -> Result<(), BundleIdError> {
    if is_valid_bundle_id(value) {
        return Ok(());
    }
    // 已知非法 → 仅做结构化分类（分类顺序不影响 accept/reject 判决，判决已由上方权威给出）。
    if value.is_empty() {
        return Err(BundleIdError::Empty);
    }
    if let Some(ch) = value.chars().find(|c| !is_bundle_id_char(*c)) {
        return Err(BundleIdError::IllegalChar {
            value: value.to_string(),
            ch,
        });
    }
    Err(BundleIdError::ReservedSeparator(value.to_string()))
}

/// MCP Server 的**唯一身份**（构造即校验）/ MCP Server identity (valid by construction)。
///
/// **实例存在 ⇒ 值合法**（由 [`TryFrom`] 保证，判决完全委托 [`is_valid_bundle_id`]）。与 display 名
/// （`ServerName = String`）**不同型** ⇒ 把 name 传给要 `bundle_id` 的位置 = **编译红**。设计理由见模块文档。
///
/// # serde
///
/// 反序列化经 `try_from = "String"` ⇒ **畸形值在字段级即判废**（如 `mcp.json` 里的 `"bundleId": "a.b"`），
/// 由调用方的既有逐条降级通道照常处理（如 `validate_server` 的单-server drop + 错误，整份文件不 abort）。
/// 序列化经 `into = "String"` ⇒ **wire 形状仍是裸字符串**，与改型前逐字节一致。
///
/// # 用 name 查身份键表 = 编译红
///
/// ```compile_fail,E0277
/// use smcp::utils::bundle_id::BundleId;
/// # use std::collections::HashMap;
/// // display 名（给人看）——与 bundle_id（给代码）不同型。
/// type ServerName = String;
/// let routes: HashMap<BundleId, u8> = HashMap::new();
/// let name: ServerName = "filesystem".to_string();
/// // ❌ E0277：BundleId 不 Borrow<String>/Borrow<str>（刻意未实现）⇒ 用 display 名查身份键表编译不过。
/// let _ = routes.get(&name);
/// ```
///
/// 正确姿势是显式构造（构造点即校验点）：
///
/// ```
/// use smcp::utils::bundle_id::BundleId;
/// let id = BundleId::try_from("filesystem".to_string()).unwrap();
/// assert_eq!(id.as_str(), "filesystem");
/// // 畸形值构造不出来 —— 这正是「构造即校验」。
/// assert!(BundleId::try_from("my.server".to_string()).is_err());
/// assert!(BundleId::try_from("a__b".to_string()).is_err());
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct BundleId(String);

impl BundleId {
    /// 借出底层字符串 / borrow the inner string。
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 取回底层字符串 / consume into the inner string。
    #[inline]
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for BundleId {
    type Error = BundleIdError;

    /// 判决完全委托 [`validate_bundle_id`]（→ [`is_valid_bundle_id`] 单一权威）；**勿**在此重写规则集。
    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_bundle_id(&value)?;
        Ok(BundleId(value))
    }
}

impl TryFrom<&str> for BundleId {
    type Error = BundleIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        validate_bundle_id(value)?;
        Ok(BundleId(value.to_string()))
    }
}

impl std::str::FromStr for BundleId {
    type Err = BundleIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl From<BundleId> for String {
    fn from(id: BundleId) -> Self {
        id.0
    }
}

impl AsRef<str> for BundleId {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BundleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 与 `&str` 直接比较（免 `.as_str()` 噪声；**只**放开比较，不放开查表）/ compare against `&str`。
impl PartialEq<str> for BundleId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for BundleId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
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

    /// #130：`BundleId` 的 accept/reject 边界 **MUST 恒等于**权威 [`is_valid_bundle_id`]。
    ///
    /// 这条守护正是 newtype 的存在理由——一旦有人在 `TryFrom` 里重写规则集（#127 的失效模式），
    /// 构造边界即与 SKILL `<server>` 段判据漂移 ⇒ 合法 bundle_id 的 SKILL 对 Agent 隐身。
    #[test]
    fn bundle_id_construction_verdict_matches_authority_130() {
        for v in [
            "tfrobot-tools",
            "my_api",
            "MyServer",
            "bundle_a1b2c3d4e5f60718",
            "a",
            "_leading-ok",
            "trailing-ok_",
            "",
            "my.api",
            "a__b",
            "__lead",
            "trail__",
            "服务器",
            "a b",
            &"a".repeat(256),
        ] {
            assert_eq!(
                BundleId::try_from(v.to_string()).is_ok(),
                is_valid_bundle_id(v),
                "{v:?}：构造边界必须与权威判决恒等"
            );
        }
    }

    /// 非法值的结构化分类（分类只在已知非法后进行，不参与判决）。
    #[test]
    fn bundle_id_error_classification() {
        assert_eq!(BundleId::try_from(String::new()), Err(BundleIdError::Empty));
        assert_eq!(
            BundleId::try_from("a__b".to_string()),
            Err(BundleIdError::ReservedSeparator("a__b".to_string()))
        );
        assert_eq!(
            BundleId::try_from("my.api".to_string()),
            Err(BundleIdError::IllegalChar {
                value: "my.api".to_string(),
                ch: '.'
            })
        );
    }

    /// serde：wire 形状是**裸字符串**（改型前后逐字节一致）；畸形值在**字段级**判废（不 panic、可降级）。
    #[test]
    fn bundle_id_serde_is_transparent_and_rejects_malformed() {
        let id = BundleId::try_from("acme-editor".to_string()).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"acme-editor\"");
        assert_eq!(
            serde_json::from_str::<BundleId>("\"acme-editor\"").unwrap(),
            id
        );

        // 畸形 → Err（**非** panic）：调用方（如 mcp.json 的 validate_server）照常逐条降级。
        assert!(serde_json::from_str::<BundleId>("\"a.b\"").is_err());
        // 字段级：`Option<BundleId>` 缺省仍是 None，畸形才报错。
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default)]
            bundle_id: Option<BundleId>,
        }
        assert!(serde_json::from_str::<Holder>("{}")
            .unwrap()
            .bundle_id
            .is_none());
        assert!(serde_json::from_str::<Holder>(r#"{"bundle_id":"a.b"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<Holder>(r#"{"bundle_id":"ok_1"}"#)
                .unwrap()
                .bundle_id
                .unwrap(),
            "ok_1"
        );
    }
}
