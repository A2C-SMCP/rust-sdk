/*!
* 文件名: diagnostics.rs
* 作者: JQQ
* 创建日期: 2026/08/20
* 最后修改日期: 2026/08/20
* 版权: 2023 JQQ. All rights reserved
* 依赖: serde, chrono
* 描述: #162 —— 结构化 Runtime diagnostics 词汇与单条诊断 DTO（可清除、可订阅）。
*/

//! 结构化 Runtime diagnostics（#162）。
//!
//! **进程内诊断，非 wire 错误码**：诊断 MUST NOT 进入任何 Agent-facing 响应 / 协议载荷
//! （协议 computer-management 红线「Management diagnostics MUST NOT 被复制到 Agent-facing
//! responses…」）；仅供 SDK 本地管理面（[`Computer::status`](crate::computer::Computer::status) /
//! [`Computer::subscribe_events`](crate::computer::Computer::subscribe_events)）与下游宿主消费。
//! severity 语义（TFRC-76 北极星）：`Error` = 核心 Runtime 能力不可用；`Degraded` = Runtime
//! 仍可用但部分能力受限。
//!
//! [`DiagnosticCode`] 命名与协议 runtime-contract §6 错误类别（startup / partial_failure /
//! network / …）**概念对齐**，但**不绑数值**——数值属 `ComputerError::error_code`（wire 错误码），
//! 二者互不复用。
//!
//! ## 生命周期接线规则（`Computer` 侧统一遵守）
//!
//! - **键控并存**：诊断以 `(code, target)` 为键存于 [`RuntimeStatus`](crate::status::RuntimeStatus)
//!   的 `BTreeMap`——同键后写**替代**先写（supersede）、异键**并存**（单项问题不覆盖其他并存问题）；
//!   全等（除 `occurred_at`）重复记录被去重（不 bump revision，防双接线风暴）。
//! - **任一 MCP 生命周期操作成功 = 该 bundle 恢复/消亡 → 清其全部诊断**（`start`/`restart` 成功
//!   清全部；`stop` 的 `Ok(false)` 精确清 `McpStopFailed`——「没停到」不等于「start 失败已恢复」）；
//!   `unmount_server` 真摘到后同样清（否则移除的 server 诊断永久滞留）。
//! - **代际清除**：boot 开场清 `source == Boot` 的残留（重启语义）；`reconcile_governance`
//!   开场清 `source == Governance` 后按本轮结果重记——上轮失败的 marketplace 本轮恢复即自然清除。
//! - **MCP 源永不整源清除**（只有按 bundle target 的精确清除）。
//!
//! ## 脱敏 choke point
//!
//! [`RuntimeDiagnostic::new`] 是唯一推荐构造点：message 一律经 `redact_git_urls_in_text`
//! （`crate::settings::redaction`，fail-closed）脱敏后再落字段；SDK 内全部写入点经此构造。
//! 直接结构体字面量构造绕过脱敏，仅供测试注入。
//!
//! ⚠️ **已知边界**（依赖的 invariant，非 choke point 能兜底的）：构造期脱敏覆盖 `scheme://`
//! URL 形态凭据；MCP 生命周期失败路径的 message 取 `ComputerError` Display（对齐 #161
//! `WindowEnumerationFailure.message` 先例），其 secret-free 依赖**错误类型本身不携带渲染值**
//! 的既有纪律（`HttpAuthenticationError` / `OAuthProtocolError` 的保安全设计）。新增错误变体
//! 若 Display 可能嵌入非 URL 形态 secret（header token / env 值），须在错误类型侧拦截，
//! 不得依赖本模块兜底。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::errors::ComputerError;
use crate::mcp_clients::BundleId;
use crate::settings::redaction::redact_git_urls_in_text;

// ===========================================================================
// 词汇 / Vocabulary
// ===========================================================================

/// 稳定诊断码（小闭集，snake_case 序列化；`#[non_exhaustive]` 令新增变体为非破坏演进）/
/// stable diagnostic code.
///
/// 进程内诊断码，非 wire 错误码——与 `ComputerError::error_code()`（协议错误码）概念对齐
/// （runtime-contract §6 错误类别）但不绑数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticCode {
    /// boot 期 MCP manager 初始化失败（核心能力不可用 → Error，生命周期落 `error`）。
    BootManagerInitFailed,
    /// boot 期 raw 声明注册被拒（核心能力不可用 → Error）。
    BootDeclarationRejected,
    /// boot 期 toolspool blob store 初始化失败（非阻断：本会话 blob 禁用 → Degraded）。
    BlobStoreInitFailed,
    /// 单个 MCP server start 失败（部分能力受限 → Degraded）。
    McpStartFailed,
    /// 单个 MCP server stop 失败（能力仍在、到不了 desired 态 → Degraded）。
    McpStopFailed,
    /// 单个 MCP server restart 失败（Degraded）。
    McpRestartFailed,
    /// 治理重挂 bundled server 失败（register 路径，Degraded）。
    BundledRemountFailed,
    /// marketplace 源同步失败（不可达 / clone 树缺失且 clone 失败，Degraded）。
    MarketplaceSyncFailed,
    /// 账本派生缓存重物化失败（§63，Degraded）。
    LedgerRematerializeFailed,
}

/// 严重度（TFRC-76 北极星）/ severity.
///
/// `Error` = 核心 Runtime 能力不可用；`Degraded` = Runtime 仍可用但部分能力受限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticSeverity {
    /// 核心 Runtime 能力不可用 / core capability unavailable.
    Error,
    /// 可用但部分能力受限 / available with partial restrictions.
    Degraded,
}

/// 产出诊断的子系统 / owning subsystem（代际清除的清除粒度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticSource {
    /// boot 链路（boot 开场代际清除）。
    Boot,
    /// MCP 生命周期操作（永不整源清除，仅按 bundle target 精确清除）。
    Mcp,
    /// 治理恢复链路（reconcile 开场代际清除后按本轮结果重记）。
    Governance,
}

/// 失败的操作 / the failing operation（诊断码的动词面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticOperation {
    /// `boot_up` 内 manager.initialize。
    InitializeManager,
    /// `boot_up` 内 register_raw_server。
    RegisterDeclaration,
    /// `boot_up` 内 ToolspoolBlobStore::new。
    InitBlobStore,
    /// `start_mcp_client`。
    StartClient,
    /// `stop_mcp_client`。
    StopClient,
    /// `restart_mcp_client`。
    RestartClient,
    /// `reconcile_governance` 阶段二 register_server。
    RemountBundledServer,
    /// marketplace 源同步（阶段一 stage）。
    MarketplaceSync,
    /// 账本重物化（阶段一·五）。
    LedgerRematerialize,
}

/// 受影响对象 / affected target（非 `Option`：boot 失败影响的是「整个 runtime」而非「无」）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticTarget {
    /// 整个 runtime（boot 级失败）/ whole runtime.
    Runtime,
    /// 单个 MCP server（身份键 = `bundle_id`，协议 §身份正交性）/ one MCP server.
    Bundle(BundleId),
    /// 一个 marketplace 源（known_marketplaces 键）/ one marketplace source.
    Marketplace(String),
    /// 一个治理 plugin（pid `<plugin>@<marketplace>`）/ one governed plugin.
    Plugin(String),
}

// ===========================================================================
// 键 + 单条诊断 / Key & single diagnostic
// ===========================================================================

/// 诊断键 = `(code, target)`：同键覆盖（supersede）、异键并存 / the dedup & supersede key.
///
/// 作为 [`RuntimeStatus`](crate::status::RuntimeStatus) 内 `BTreeMap` 的键——键序即快照
/// `diagnostics` 的稳定输出序。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiagnosticKey {
    /// 诊断码 / diagnostic code.
    pub code: DiagnosticCode,
    /// 受影响对象 / affected target.
    pub target: DiagnosticTarget,
}

/// 单条 runtime 诊断 / one runtime diagnostic（进程内 DTO，不上 wire、不持久化）。
///
/// 字段面**恰好**为机器可读属性（#162 验收⑦）：不含 UI 文案、Robot、connection authority
/// 或客户端操作策略——呈现与推荐动作由消费方（如 tfrobot-client）自行组合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDiagnostic {
    /// 稳定诊断码 / stable code.
    pub code: DiagnosticCode,
    /// 严重度 / severity.
    pub severity: DiagnosticSeverity,
    /// 产出子系统 / owning subsystem.
    pub source: DiagnosticSource,
    /// 失败操作 / failing operation.
    pub operation: DiagnosticOperation,
    /// 受影响对象 / affected target.
    pub target: DiagnosticTarget,
    /// 人类可读、secret-safe message（经构造点 fail-closed 脱敏）/ secret-free message.
    pub message: String,
    /// 发生时间（RFC3339）/ occurrence time.
    pub occurred_at: DateTime<Utc>,
    /// 同操作立即重试可能成功（连接拒绝 / 超时等）/ immediate retry may succeed.
    pub retryable: bool,
    /// 预期自愈（网络抖动），无需用户动作 / expected to heal without action.
    pub transient: bool,
}

impl RuntimeDiagnostic {
    /// 唯一推荐构造点（脱敏 choke point）：message 一律经 git-URL fail-closed 脱敏；
    /// `occurred_at = now`；`retryable`/`transient` 缺省 `false`，经 [`Self::with_recovery`] 补充。
    #[must_use]
    pub fn new(
        code: DiagnosticCode,
        severity: DiagnosticSeverity,
        source: DiagnosticSource,
        operation: DiagnosticOperation,
        target: DiagnosticTarget,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity,
            source,
            operation,
            target,
            message: redact_git_urls_in_text(&message.into()),
            occurred_at: Utc::now(),
            retryable: false,
            transient: false,
        }
    }

    /// 链式补充恢复属性（机器可读建议，非契约）/ attach recovery flags.
    #[must_use]
    pub fn with_recovery(mut self, retryable: bool, transient: bool) -> Self {
        self.retryable = retryable;
        self.transient = transient;
        self
    }

    /// 是否「同一问题仍在」（除 `occurred_at` 外全等）——去重判据，不 bump revision。
    pub(crate) fn same_problem(&self, other: &Self) -> bool {
        self.code == other.code
            && self.severity == other.severity
            && self.source == other.source
            && self.operation == other.operation
            && self.target == other.target
            && self.message == other.message
            && self.retryable == other.retryable
            && self.transient == other.transient
    }
}

/// 从 [`ComputerError`] 推 `(retryable, transient)` 的**尽力**启发式（非契约）：
/// 连接 / 传输 / 超时类 → `(true, true)`（立即重试可能成功且预期自愈）；其余（配置 / 校验 /
/// 鉴权 / 状态机）→ `(false, false)`。错分只影响消费方的呈现建议，不影响诊断本体。
pub(crate) fn classify_recovery(err: &ComputerError) -> (bool, bool) {
    matches!(
        err,
        ComputerError::ConnectionError(_)
            | ComputerError::TransportError(_)
            | ComputerError::TimeoutError(_)
    )
    .then_some((true, true))
    .unwrap_or((false, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_severity_source_operation_serde_snake_case() {
        // 词汇 serde 形态稳定（snake_case）——消费方依赖的机器可读契约。
        assert_eq!(
            serde_json::to_string(&DiagnosticCode::BootManagerInitFailed).unwrap(),
            "\"boot_manager_init_failed\""
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticSeverity::Degraded).unwrap(),
            "\"degraded\""
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticSource::Governance).unwrap(),
            "\"governance\""
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticOperation::RemountBundledServer).unwrap(),
            "\"remount_bundled_server\""
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticTarget::Runtime).unwrap(),
            "\"runtime\""
        );
    }

    #[test]
    fn target_bundle_serde_is_externally_tagged() {
        let bid = BundleId::try_from("srv".to_string()).unwrap();
        let v = serde_json::to_value(DiagnosticTarget::Bundle(bid)).unwrap();
        assert_eq!(v, serde_json::json!({"bundle": "srv"}));
        let v = serde_json::to_value(DiagnosticTarget::Marketplace("acme".into())).unwrap();
        assert_eq!(v, serde_json::json!({"marketplace": "acme"}));
        let v = serde_json::to_value(DiagnosticTarget::Plugin("p@acme".into())).unwrap();
        assert_eq!(v, serde_json::json!({"plugin": "p@acme"}));
    }

    #[test]
    fn message_is_redacted_at_construction() {
        // 验收⑤：构造点强制脱敏（fail-closed）——凭据 URL 不落 message。
        let diag = RuntimeDiagnostic::new(
            DiagnosticCode::MarketplaceSyncFailed,
            DiagnosticSeverity::Degraded,
            DiagnosticSource::Governance,
            DiagnosticOperation::MarketplaceSync,
            DiagnosticTarget::Marketplace("acme".into()),
            "clone https://cnb:hunter2@example.com/mp.git failed",
        );
        assert!(!diag.message.contains("hunter2"), "凭据不得落 message");
        assert!(
            !diag.message.contains("cnb:"),
            "userinfo 整段不得落 message"
        );
        assert!(diag.message.contains("example.com"), "保留 host 供定位");
    }

    #[test]
    fn same_problem_ignores_occurred_at_only() {
        let a = RuntimeDiagnostic::new(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            DiagnosticSource::Mcp,
            DiagnosticOperation::StartClient,
            DiagnosticTarget::Runtime,
            "x",
        );
        let mut b = a.clone();
        b.occurred_at = Utc::now() + chrono::Duration::seconds(5);
        assert!(a.same_problem(&b), "仅时间不同 = 同一问题仍在");
        b.message = "different".into();
        assert!(!a.same_problem(&b), "message 变 = 问题被替代");
    }

    #[test]
    fn classify_recovery_marks_transient_transport_errors() {
        use crate::errors::ComputerError;
        assert_eq!(
            classify_recovery(&ComputerError::ConnectionError("refused".into())),
            (true, true)
        );
        assert_eq!(
            classify_recovery(&ComputerError::InvalidConfiguration("bad".into())),
            (false, false)
        );
    }
}
