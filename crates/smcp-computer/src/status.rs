/*!
* 文件名: status.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde, tokio::sync::broadcast
* 描述: #107 S7（#114）—— runtime handle 的 status / observability：结构化状态快照 + 单调 revision + 事件订阅。
*       对齐协议 runtime-contract §3（生命周期状态）/ §4.7（shutdown 后不再发 stale events）。
*       Runtime status surface: lifecycle state + monotonic revisions + event subscription。
*/

//! Runtime status / observability（#107 S7 / #114）。
//!
//! **边界**（设计 §7「演进 `Computer`，不造平行类型」）：本模块只提供**观测底座**——
//! [`RuntimeStatus`] 持有生命周期状态、两个**分离**的单调 revision（config ⊥ capability，设计 §12 R2）、
//! 公开诊断（last error / degraded reason）与广播事件通道。[`Computer`](crate::computer::Computer) 持一个
//! `Arc<RuntimeStatus>`，在其生命周期方法（boot / connect / join / disconnect / shutdown）与能力变更点
//! （start/stop MCP）上驱动状态迁移与 revision bump；config revision 的 mutate-bump 由 S6（#113）接线。
//!
//! **revision 分离**（§12 R2）：config revision 记「声明式配置内容变化」（S6 mutate 落盘时 bump）；
//! capability revision 记「Agent-facing 能力投影变化」（MCP 起停 / 工具集变化时 bump）。二者独立单调，
//! 因「config 改不一定改 capability」（如 disable 一个本未激活的 server）。
//!
//! **shutdown 语义**（contract §4.7）：[`RuntimeStatus::enter_shutdown`] 后，事件通道**闸断**——除进入
//! shutdown 时发的那一条终态 [`ComputerEvent::LifecycleChanged`]`(Shutdown)` 外，不再发出任何 stale 事件，
//! 后续 revision bump 亦降为 no-op（既有计数不回退，保单调）。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::RwLock as StdRwLock;

use crate::diagnostics::{DiagnosticKey, DiagnosticSeverity, RuntimeDiagnostic};
use crate::mcp_clients::BundleId;
use crate::oauth::OAuthStatus;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// 事件广播通道容量（滞后订阅者会收到 `Lagged`，快照可经 [`Computer::status`](crate::computer::Computer::status)
/// 重新拉取，故有界容量安全）/ event channel capacity。
const EVENT_CHANNEL_CAPACITY: usize = 64;

// ===========================================================================
// 生命周期状态 / Lifecycle state
// ===========================================================================

/// runtime 生命周期状态（协议 runtime-contract §3 的 Rust 映射）/ lifecycle state。
///
/// 契约 §3 允许「文档化的等价映射」而非精确字符串；本枚举即该映射，serde 以 snake_case 序列化，与协议表用词一致。
/// `#[repr(u8)]` + 显式判别值供 [`RuntimeStatus`] 用 [`AtomicU8`] 无锁存取。当前 `Computer` 实际迁移到其中的子集
/// （Created / Starting / Started / Connected / JoinedOffice / Degraded / Shutdown 等），其余变体为前向兼容保留
/// （SDK 后续可细化迁移，快照消费方无需改动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum LifecycleState {
    /// runtime 对象已存在，尚未初始化本地资源 / object exists, no local resources yet。
    Created = 0,
    /// 正在加载 config / 解析本地状态 / 启动 MCP 资源 / loading config & local state。
    Starting = 1,
    /// 本地 runtime 已初始化（可能未连接 Server）/ local runtime initialized。
    Started = 2,
    /// 正在建立 Socket.IO 连接并握手 / establishing connection。
    Connecting = 3,
    /// Socket.IO 已连接（Office join 可能未完成）/ connected。
    Connected = 4,
    /// 已加入 Office，可接收路由来的 `client:*` / joined office。
    JoinedOffice = 5,
    /// 正在应用新 config / reconcile desired state / applying new config。
    Syncing = 6,
    /// 部分可用，带公开诊断（见 `degraded_reason`）/ partially available。
    Degraded = 7,
    /// 正在离开 / 关闭 Socket.IO 连接 / disconnecting。
    Disconnecting = 8,
    /// 正在停止本地 MCP/service 活动 / stopping local activity。
    Stopping = 9,
    /// 已停止 service 活动 / stopped。
    Stopped = 10,
    /// 已释放资源，不应再发 stale events（§4.7）/ resources released。
    Shutdown = 11,
    /// 无外部动作 / 新 config 则无法推进 / cannot make progress。
    Error = 12,
}

impl LifecycleState {
    /// 从 [`AtomicU8`] 存储值还原（未知值 → [`LifecycleState::Error`]，防越界 UB）/ from stored u8。
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Created,
            1 => Self::Starting,
            2 => Self::Started,
            3 => Self::Connecting,
            4 => Self::Connected,
            5 => Self::JoinedOffice,
            6 => Self::Syncing,
            7 => Self::Degraded,
            8 => Self::Disconnecting,
            9 => Self::Stopping,
            10 => Self::Stopped,
            11 => Self::Shutdown,
            _ => Self::Error,
        }
    }
}

impl std::fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::JoinedOffice => "joined_office",
            Self::Syncing => "syncing",
            Self::Degraded => "degraded",
            Self::Disconnecting => "disconnecting",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Shutdown => "shutdown",
            Self::Error => "error",
        };
        f.write_str(s)
    }
}

// ===========================================================================
// 快照 + 事件 / Snapshot & events
// ===========================================================================

/// runtime 状态快照（cheap、非阻塞诊断）/ a point-in-time runtime status snapshot。
///
/// 由 [`Computer::status`](crate::computer::Computer::status) 组装：状态 / revision / 诊断取自 [`RuntimeStatus`]，
/// 汇总计数为当次对内存态的只读投影（MCP 声明集 / 活跃集 / 工具集 / SKILL 集）——**不做 ledger / 磁盘 IO**，
/// 故 plugin / marketplace 明细留给 ledger 支撑的专用 inventory API（`list_mcp_servers_with_metadata`），
/// 本快照只承载热路径可廉价取得的能力汇总。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerStatusSnapshot {
    /// 生命周期状态 / lifecycle state。
    pub lifecycle: LifecycleState,
    /// config revision（声明式配置内容摘要单调计数；S6 mutate 落盘时 bump）/ config revision。
    pub config_revision: u64,
    /// capability revision（Agent-facing 能力投影单调计数；MCP 起停 / boot 时 bump）/ capability revision。
    pub capability_revision: u64,
    /// 已声明 MCP server 数（desired 集）/ declared MCP server count。
    pub mcp_servers: usize,
    /// 已启动（active）MCP server 数 / active MCP server count。
    pub active_mcp_servers: usize,
    /// 已注册工具数（**已缓存** `tool_mapping` 长度，由 start/stop/refresh 维护；非实时 `tools/list` RPC）/
    /// registered/loaded tool count from the cached mapping (not a live `tools/list` RPC)。中途变不健康但未
    /// 经 stop/refresh 的 server 其工具仍计入——反映 desired 已加载集，非「此刻可解析」集。
    pub tools: usize,
    /// 当前活跃 SKILL 数（排除孤儿）/ active SKILL count。
    pub skills: usize,
    /// 最近一次公开错误（**投影**，不含 secret）：`severity == Error` 条目中 `occurred_at` 最大者的
    /// message（#162 从裸存储升级为派生，兼容保留）/ last public error (derived projection, secret-free)。
    pub last_error: Option<String>,
    /// degraded 诊断原因（**投影**）：`severity == Degraded` 条目中 `occurred_at` 最大者的 message
    /// （`lifecycle == Degraded` 时通常非空，非硬不变量）/ degraded reason (derived projection)。
    pub degraded_reason: Option<String>,
    /// 诊断集单调 revision（与 config/capability 分离——「健康度」不进内容 revision，#128 先例）/
    /// monotonic diagnostics revision.
    #[serde(default)]
    pub diagnostics_revision: u64,
    /// 当前活跃诊断（BTreeMap 键序 = 确定性稳定序）/ active diagnostics (stable key order)。
    ///
    /// 空集不序列化（#128 兼容姿势：干净快照字节不变）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

/// runtime 观测事件（[`Computer::subscribe_events`](crate::computer::Computer::subscribe_events) 广播）/ runtime event。
///
/// 事件为**轻量增量**——订阅方收到后可按需调 [`Computer::status`](crate::computer::Computer::status) 取全量快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputerEvent {
    /// 生命周期状态迁移 / lifecycle transition。
    LifecycleChanged {
        /// 新状态 / the new state。
        state: LifecycleState,
    },
    /// config revision 增长 / config revision bumped。
    ConfigRevisionBumped {
        /// 新 revision / the new revision。
        revision: u64,
    },
    /// capability revision 增长 / capability revision bumped。
    CapabilityRevisionBumped {
        /// 新 revision / the new revision。
        revision: u64,
    },
    /// OAuth authorization status changed for one MCP server.
    OAuthStatusChanged {
        /// Stable MCP server identity.
        bundle_id: BundleId,
        /// Complete, non-secret status after the transition.
        status: OAuthStatus,
    },
    /// 诊断集变化（新增 / 替代 / 清除，#162）/ diagnostics set changed.
    ///
    /// **轻量增量**：仅携单调 revision；订阅方（含 `Lagged` 后）经
    /// [`Computer::status`](crate::computer::Computer::status) 拉全量快照重建——事件丢失安全
    /// （同 [`ComputerEvent::OAuthStatusChanged`] 的重同步姿势）。
    DiagnosticsChanged {
        /// 新 revision / the new revision.
        revision: u64,
    },
}

// ===========================================================================
// RuntimeStatus 持有者 / holder
// ===========================================================================

/// 公开诊断集（键控：同键替代、异键并存，#162）/ the keyed diagnostics set。
type DiagnosticsMap = BTreeMap<DiagnosticKey, RuntimeDiagnostic>;

/// runtime 观测状态持有者（`Computer` 持 `Arc<RuntimeStatus>`，跨 clone 共享同一视图）/ runtime status holder。
///
/// 锁纪律：状态 / revision / shutdown 闸门为原子无锁存取（cheap，`status()` 不阻塞）；诊断集走
/// [`std::sync::RwLock`]（临界区仅 clone / retain，**不跨 await**）。事件用 [`tokio::sync::broadcast`]。
pub struct RuntimeStatus {
    /// 生命周期状态（[`LifecycleState`] as u8）/ lifecycle state。
    state: AtomicU8,
    /// config revision 单调计数 / monotonic config revision。
    config_revision: AtomicU64,
    /// capability revision 单调计数 / monotonic capability revision。
    capability_revision: AtomicU64,
    /// diagnostics revision 单调计数（第三轴，健康度 ⊥ 内容，#162/#128）/ monotonic diagnostics revision。
    diagnostics_revision: AtomicU64,
    /// shutdown 闸门：`true` 后事件闸断、bump 降 no-op（§4.7）/ shutdown gate。
    shutdown: AtomicBool,
    /// 公开诊断集（键控）/ the keyed diagnostics set。
    diagnostics: StdRwLock<DiagnosticsMap>,
    /// Last published OAuth status per bundle, used to suppress duplicate events.
    oauth_statuses: StdRwLock<HashMap<BundleId, OAuthStatus>>,
    /// 事件广播发送端（订阅方经 `subscribe` 取 Receiver）/ event broadcast sender。
    events: broadcast::Sender<ComputerEvent>,
}

impl RuntimeStatus {
    /// 建初始状态（`Created`、revision=0、未 shutdown）/ construct at `Created`。
    pub fn new() -> Self {
        let (events, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            state: AtomicU8::new(LifecycleState::Created as u8),
            config_revision: AtomicU64::new(0),
            capability_revision: AtomicU64::new(0),
            diagnostics_revision: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            diagnostics: StdRwLock::new(BTreeMap::new()),
            oauth_statuses: StdRwLock::new(HashMap::new()),
            events,
        }
    }

    /// 订阅观测事件流（shutdown 后返回的 Receiver 除已在途终态事件外不会再收到新事件）/ subscribe to events。
    pub fn subscribe(&self) -> broadcast::Receiver<ComputerEvent> {
        self.events.subscribe()
    }

    /// 发一条事件（shutdown 后闸断）/ emit an event (gated after shutdown)。
    fn emit(&self, ev: ComputerEvent) {
        if !self.shutdown.load(Ordering::Acquire) {
            let _ = self.events.send(ev);
        }
    }

    /// 迁移生命周期状态并广播（**永不离开** `Shutdown` 终态）/ transition & broadcast (never leaves Shutdown)。
    ///
    /// 用 CAS 而非「load 闸门 + store」：后者在并发下有 TOCTOU——`transition` 读闸门=false 后被抢占、
    /// `enter_shutdown` 落 `Shutdown`、`transition` 恢复即用 `store` 把终态**回写**为非终态。CAS 循环保证一旦
    /// 状态为 `Shutdown` 就拒绝迁移；且与 `enter_shutdown` 的 `store(Shutdown)` 交错时，被抢占的 CAS 会因期望值
    /// 失配而重试→读到 `Shutdown`→返回。事件仍经 `emit` 的闸门抑制（`Shutdown` 恒为最后事件）。
    pub fn transition(&self, next: LifecycleState) {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            // 已进入 shutdown 终态 → 拒绝迁移（不改状态、不发事件）。
            if cur == LifecycleState::Shutdown as u8 {
                return;
            }
            match self.state.compare_exchange_weak(
                cur,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        self.emit(ComputerEvent::LifecycleChanged { state: next });
    }

    /// 进入 shutdown 终态：幂等；发出唯一的终态 `LifecycleChanged(Shutdown)` 后闸断所有后续事件（§4.7）。
    /// Enter the terminal shutdown state (idempotent); emits the final event then gates all future emissions。
    pub fn enter_shutdown(&self) {
        // 原子 claim：仅首次调用生效；swap 后闸门即对所有 `emit` 生效。
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state
            .store(LifecycleState::Shutdown as u8, Ordering::Release);
        // 终态事件**直接** send（绕过已生效的 `emit` 闸门），确保在途订阅者观测到 Shutdown 后即静默。
        let _ = self.events.send(ComputerEvent::LifecycleChanged {
            state: LifecycleState::Shutdown,
        });
    }

    /// bump config revision（单调 +1，广播）；shutdown 后 no-op 返回当前值（不回退，保单调）/ bump config revision。
    pub fn bump_config(&self) -> u64 {
        if self.shutdown.load(Ordering::Acquire) {
            return self.config_revision.load(Ordering::Acquire);
        }
        let new = self.config_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.emit(ComputerEvent::ConfigRevisionBumped { revision: new });
        new
    }

    /// bump capability revision（语义同 [`bump_config`](Self::bump_config)）/ bump capability revision。
    pub fn bump_capability(&self) -> u64 {
        if self.shutdown.load(Ordering::Acquire) {
            return self.capability_revision.load(Ordering::Acquire);
        }
        let new = self.capability_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.emit(ComputerEvent::CapabilityRevisionBumped { revision: new });
        new
    }

    /// Publish a changed OAuth status through the shared Computer event stream.
    ///
    /// Equal consecutive states for the same bundle are suppressed. After shutdown all emissions
    /// are gated, so the terminal lifecycle event remains the final observable event.
    pub(crate) fn update_oauth_status(&self, bundle_id: BundleId, status: OAuthStatus) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let changed = {
            let mut statuses = self
                .oauth_statuses
                .write()
                .expect("OAuth status cache poisoned");
            if statuses.get(&bundle_id) == Some(&status) {
                false
            } else {
                statuses.insert(bundle_id.clone(), status.clone());
                true
            }
        };
        if changed {
            self.emit(ComputerEvent::OAuthStatusChanged { bundle_id, status });
        }
    }

    pub(crate) fn latest_oauth_status(&self, bundle_id: &BundleId) -> Option<OAuthStatus> {
        self.oauth_statuses
            .read()
            .expect("OAuth status cache poisoned")
            .get(bundle_id)
            .cloned()
    }

    /// 诊断集变更的统一底座：`f` 在写锁内变更 map 并**如实返回是否真变**；真变才 bump
    /// [`diagnostics_revision`](Self::diagnostics_revision) 并广播 [`ComputerEvent::DiagnosticsChanged`]。
    /// shutdown 先行闸断（§4.7：描述的 runtime 已不存在 → map 冻结、不 bump、不发事件）。
    fn apply_diagnostics_change<F>(&self, f: F)
    where
        F: FnOnce(&mut DiagnosticsMap) -> bool,
    {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let changed = {
            let mut map = self.diagnostics.write().expect("diagnostics poisoned");
            f(&mut map)
        };
        if changed {
            let new = self.diagnostics_revision.fetch_add(1, Ordering::AcqRel) + 1;
            self.emit(ComputerEvent::DiagnosticsChanged { revision: new });
        }
    }

    /// 记录一条诊断（#162）：同键（`code`+`target`）后写**替代**先写、异键**并存**；全等
    /// （除 `occurred_at`）重复记录去重（不 bump——防双接线风暴，对齐 `update_oauth_status` 先例）。
    /// shutdown 后整段 no-op（map 冻结）。
    pub fn record_diagnostic(&self, diag: RuntimeDiagnostic) {
        let key = DiagnosticKey {
            code: diag.code,
            target: diag.target.clone(),
        };
        self.apply_diagnostics_change(|map| match map.get(&key) {
            Some(existing) if existing.same_problem(&diag) => false,
            _ => {
                map.insert(key, diag);
                true
            }
        });
    }

    /// 按谓词清除诊断（恢复路径专用，#162）：仅实际移除 ≥1 条才 bump + 广播（空清除不计数，
    /// §12 R2「真变化才计数」同款）。典型用法：boot 开场清 `source == Boot`、reconcile 开场清
    /// `source == Governance`、MCP 恢复 / 移除按 `target` 精确清。
    pub fn clear_diagnostics_where(&self, pred: impl Fn(&RuntimeDiagnostic) -> bool) {
        self.apply_diagnostics_change(|map| {
            let before = map.len();
            map.retain(|_, d| !pred(d));
            map.len() != before
        });
    }

    /// 是否存在 `severity == Degraded` 条目（boot 收尾 / reconcile 窄域恢复的 lifecycle 判定）。
    pub fn has_degraded(&self) -> bool {
        self.diagnostics
            .read()
            .expect("diagnostics poisoned")
            .values()
            .any(|d| d.severity == DiagnosticSeverity::Degraded)
    }

    /// 当前诊断集（BTreeMap 键序 = 稳定序）/ current diagnostics (stable key order).
    pub fn diagnostics(&self) -> Vec<RuntimeDiagnostic> {
        self.diagnostics
            .read()
            .expect("diagnostics poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// 当前生命周期状态 / current lifecycle state。
    pub fn state(&self) -> LifecycleState {
        LifecycleState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// 当前 config revision / current config revision。
    pub fn config_revision(&self) -> u64 {
        self.config_revision.load(Ordering::Acquire)
    }

    /// 当前 capability revision / current capability revision。
    pub fn capability_revision(&self) -> u64 {
        self.capability_revision.load(Ordering::Acquire)
    }

    /// 当前 diagnostics revision / current diagnostics revision。
    pub fn diagnostics_revision(&self) -> u64 {
        self.diagnostics_revision.load(Ordering::Acquire)
    }

    /// 是否已 shutdown / whether shutdown has been entered。
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// 用给定汇总计数组装完整快照（状态 / revision / 诊断取自本 holder）/ assemble a full snapshot。
    ///
    /// `last_error` / `degraded_reason` 为**派生投影**（#162）：同一把读锁内按 severity 过滤、取
    /// `occurred_at` 最大条目的 message（平手按键序，确定性）——空集 → 双 `None`。
    pub fn snapshot(
        &self,
        mcp_servers: usize,
        active_mcp_servers: usize,
        tools: usize,
        skills: usize,
    ) -> ComputerStatusSnapshot {
        let (last_error, degraded_reason, diagnostics) = {
            let map = self.diagnostics.read().expect("diagnostics poisoned");
            let latest = |severity: DiagnosticSeverity| {
                map.iter()
                    .filter(|(_, d)| d.severity == severity)
                    .max_by(|(k1, d1), (k2, d2)| (d1.occurred_at, *k1).cmp(&(d2.occurred_at, *k2)))
                    .map(|(_, d)| d.message.clone())
            };
            (
                latest(DiagnosticSeverity::Error),
                latest(DiagnosticSeverity::Degraded),
                map.values().cloned().collect(),
            )
        };
        ComputerStatusSnapshot {
            lifecycle: self.state(),
            config_revision: self.config_revision(),
            capability_revision: self.capability_revision(),
            mcp_servers,
            active_mcp_servers,
            tools,
            skills,
            last_error,
            degraded_reason,
            diagnostics_revision: self.diagnostics_revision(),
            diagnostics,
        }
    }
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_state_roundtrips_u8_and_serde() {
        // 全变体 as u8 → from_u8 往返一致；序列化用 snake_case（对齐协议 §3 用词）。
        let all = [
            LifecycleState::Created,
            LifecycleState::Starting,
            LifecycleState::Started,
            LifecycleState::Connecting,
            LifecycleState::Connected,
            LifecycleState::JoinedOffice,
            LifecycleState::Syncing,
            LifecycleState::Degraded,
            LifecycleState::Disconnecting,
            LifecycleState::Stopping,
            LifecycleState::Stopped,
            LifecycleState::Shutdown,
            LifecycleState::Error,
        ];
        for s in all {
            assert_eq!(LifecycleState::from_u8(s as u8), s, "u8 roundtrip {s}");
        }
        // 越界判别值 → Error（防 AtomicU8 存取越界 UB）。
        assert_eq!(LifecycleState::from_u8(200), LifecycleState::Error);
        // serde snake_case + Display 一致。
        assert_eq!(
            serde_json::to_string(&LifecycleState::JoinedOffice).unwrap(),
            "\"joined_office\""
        );
        assert_eq!(LifecycleState::JoinedOffice.to_string(), "joined_office");
    }

    #[test]
    fn revisions_are_independent_and_monotonic() {
        let s = RuntimeStatus::new();
        assert_eq!(s.config_revision(), 0);
        assert_eq!(s.capability_revision(), 0);
        // config bump 不影响 capability（§12 R2 分离）。
        assert_eq!(s.bump_config(), 1);
        assert_eq!(s.bump_config(), 2);
        assert_eq!(s.config_revision(), 2);
        assert_eq!(s.capability_revision(), 0);
        // capability 独立单调。
        assert_eq!(s.bump_capability(), 1);
        assert_eq!(s.capability_revision(), 1);
        assert_eq!(s.config_revision(), 2);
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let s = RuntimeStatus::new();
        let mut rx = s.subscribe();
        s.transition(LifecycleState::Started);
        s.bump_capability();
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::LifecycleChanged {
                state: LifecycleState::Started
            }
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::CapabilityRevisionBumped { revision: 1 }
        );
    }

    #[tokio::test]
    async fn no_events_after_shutdown_except_final() {
        let s = RuntimeStatus::new();
        let mut rx = s.subscribe();
        s.transition(LifecycleState::Started);
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::LifecycleChanged {
                state: LifecycleState::Started
            }
        );
        // 进入 shutdown：发唯一终态事件后闸断。
        s.enter_shutdown();
        assert!(s.is_shutdown());
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::LifecycleChanged {
                state: LifecycleState::Shutdown
            }
        );
        // 幂等：二次 enter_shutdown 不再发事件。
        s.enter_shutdown();
        // shutdown 后 bump / transition 全部 no-op（不发事件、不改状态）。
        assert_eq!(s.bump_config(), 0);
        assert_eq!(s.bump_capability(), 0);
        s.transition(LifecycleState::Connected);
        assert_eq!(s.state(), LifecycleState::Shutdown);
        // 通道再无事件（try_recv 为 Empty，非新事件）。
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn oauth_events_are_deduplicated_lag_resyncable_and_shutdown_gated() {
        let s = RuntimeStatus::new();
        let bundle_id = BundleId::try_from("oauth-events").unwrap();
        let mut rx = s.subscribe();

        s.update_oauth_status(bundle_id.clone(), OAuthStatus::Unauthorized);
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::OAuthStatusChanged {
                bundle_id: bundle_id.clone(),
                status: OAuthStatus::Unauthorized,
            }
        );
        s.update_oauth_status(bundle_id.clone(), OAuthStatus::Unauthorized);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        for index in 0..=EVENT_CHANNEL_CAPACITY {
            let status = if index % 2 == 0 {
                OAuthStatus::AuthorizationPending
            } else {
                OAuthStatus::Unauthorized
            };
            s.update_oauth_status(bundle_id.clone(), status);
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        assert_eq!(
            s.latest_oauth_status(&bundle_id),
            Some(OAuthStatus::AuthorizationPending)
        );

        let mut shutdown_rx = s.subscribe();
        s.enter_shutdown();
        assert_eq!(
            shutdown_rx.recv().await.unwrap(),
            ComputerEvent::LifecycleChanged {
                state: LifecycleState::Shutdown,
            }
        );
        s.update_oauth_status(
            bundle_id,
            OAuthStatus::Error {
                message: "must be gated".into(),
            },
        );
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    // ── #162：结构化 diagnostics（键控集 + 第三 revision 轴 + 事件 + 投影）─────────

    use crate::diagnostics::{
        DiagnosticCode, DiagnosticOperation, DiagnosticSeverity, DiagnosticSource,
        DiagnosticTarget, RuntimeDiagnostic,
    };
    use chrono::{TimeZone, Utc};

    /// 测试用构造器：显式 occurred_at（绕过 `new` 的 `Utc::now()`，保推导测试确定性）。
    fn diag_at(
        code: DiagnosticCode,
        severity: DiagnosticSeverity,
        target: DiagnosticTarget,
        message: &str,
        occurred_at: chrono::DateTime<Utc>,
    ) -> RuntimeDiagnostic {
        RuntimeDiagnostic {
            code,
            severity,
            source: DiagnosticSource::Mcp,
            operation: DiagnosticOperation::StartClient,
            target,
            message: message.to_string(),
            occurred_at,
            retryable: false,
            transient: false,
        }
    }

    fn bundle(id: &str) -> DiagnosticTarget {
        DiagnosticTarget::Bundle(BundleId::try_from(id).unwrap())
    }

    #[test]
    fn diagnostics_revision_independent_and_monotonic() {
        // 验收③：diagnostics revision 与 config/capability 分离（#128「健康度不进内容 revision」）且单调。
        let s = RuntimeStatus::new();
        assert_eq!(s.diagnostics_revision(), 0);
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "x",
            Utc::now(),
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStopFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "y",
            Utc::now(),
        ));
        assert_eq!(s.diagnostics_revision(), 2, "每次真变化 +1");
        assert_eq!(s.config_revision(), 0, "不动 config revision");
        assert_eq!(s.capability_revision(), 0, "不动 capability revision");
    }

    #[tokio::test]
    async fn record_diagnostic_inserts_bumps_and_emits() {
        let s = RuntimeStatus::new();
        let mut rx = s.subscribe();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("srv"),
            "spawn failed",
            Utc::now(),
        ));
        assert_eq!(
            rx.recv().await.unwrap(),
            ComputerEvent::DiagnosticsChanged { revision: 1 }
        );
        let snap = s.snapshot(0, 0, 0, 0);
        assert_eq!(snap.diagnostics.len(), 1);
        assert_eq!(snap.diagnostics[0].code, DiagnosticCode::McpStartFailed);
        assert_eq!(snap.diagnostics_revision, 1);
    }

    #[test]
    fn record_supersedes_same_key_and_coexists_across_keys() {
        // 验收④：同键（code+target）覆盖（supersede），异键并存——单项问题不覆盖其他并存问题。
        let s = RuntimeStatus::new();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "first failure",
            Utc::now(),
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "second failure",
            Utc::now(),
        ));
        // 异键并存：另一 bundle + marketplace 级。
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("b"),
            "other server",
            Utc::now(),
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::MarketplaceSyncFailed,
            DiagnosticSeverity::Degraded,
            DiagnosticTarget::Marketplace("acme".into()),
            "unreachable",
            Utc::now(),
        ));
        let diags = s.diagnostics();
        assert_eq!(diags.len(), 3, "1 同键替代 + 2 异键并存");
        assert_eq!(
            diags
                .iter()
                .find(|d| d.target == bundle("a"))
                .unwrap()
                .message,
            "second failure",
            "同键后写替代先写"
        );
    }

    #[test]
    fn record_equal_diagnostic_deduped() {
        // 全等（除 occurred_at）重复记录 → 不 bump、不发事件（防双接线风暴，对齐 oauth 去重先例）。
        let s = RuntimeStatus::new();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "same problem",
            Utc::now(),
        ));
        let rev = s.diagnostics_revision();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "same problem",
            Utc::now() + chrono::Duration::seconds(5),
        ));
        assert_eq!(s.diagnostics_revision(), rev, "问题未变 → 不 bump");
        assert_eq!(s.diagnostics().len(), 1);
    }

    #[test]
    fn clear_where_removes_only_matching_and_noop_clear_does_not_bump() {
        let s = RuntimeStatus::new();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "x",
            Utc::now(),
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::MarketplaceSyncFailed,
            DiagnosticSeverity::Degraded,
            DiagnosticTarget::Marketplace("mp".into()),
            "y",
            Utc::now(),
        ));
        let rev = s.diagnostics_revision();
        // 谓词精确清除：只清 Boot 源（无命中 → no-op 不 bump）。
        s.clear_diagnostics_where(|d| d.source == DiagnosticSource::Boot);
        assert_eq!(s.diagnostics_revision(), rev, "空清除不 bump");
        // 按 target 清除。
        s.clear_diagnostics_where(|d| d.target == bundle("a"));
        assert_eq!(s.diagnostics_revision(), rev + 1);
        assert_eq!(s.diagnostics().len(), 1, "marketplace 级条目不受影响");
    }

    #[test]
    fn last_error_and_degraded_reason_derived_from_map() {
        // 投影：按 severity 过滤取 occurred_at 最大条目；空 map → 双 None。
        let s = RuntimeStatus::new();
        s.transition(LifecycleState::Degraded);
        let t0 = Utc.with_ymd_and_hms(2026, 8, 20, 10, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 8, 20, 11, 0, 0).unwrap();
        s.record_diagnostic(diag_at(
            DiagnosticCode::BootManagerInitFailed,
            DiagnosticSeverity::Error,
            DiagnosticTarget::Runtime,
            "boot failed earlier",
            t0,
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "marketplace X unreachable",
            t1,
        ));
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Error,
            bundle("b"),
            "boot recovery partial",
            t1,
        ));
        let snap = s.snapshot(3, 1, 7, 2);
        assert_eq!(snap.lifecycle, LifecycleState::Degraded);
        assert_eq!(snap.mcp_servers, 3);
        assert_eq!(snap.active_mcp_servers, 1);
        assert_eq!(snap.tools, 7);
        assert_eq!(snap.skills, 2);
        assert_eq!(
            snap.last_error.as_deref(),
            Some("boot recovery partial"),
            "Error 级取最近（t1 > t0）"
        );
        assert_eq!(
            snap.degraded_reason.as_deref(),
            Some("marketplace X unreachable")
        );
        // 清空 → 双 None（旧 set/clear 流入快照语义的等价覆盖）。
        s.clear_diagnostics_where(|_| true);
        let snap2 = s.snapshot(0, 0, 0, 0);
        assert!(snap2.degraded_reason.is_none());
        assert!(snap2.last_error.is_none());
        assert!(snap2.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn diagnostics_frozen_after_shutdown() {
        // §4.7：shutdown 后 record/clear 全 no-op——map 冻结、revision 不增、无事件；快照仍可读（终态审计）。
        let s = RuntimeStatus::new();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "x",
            Utc::now(),
        ));
        let rev = s.diagnostics_revision();
        s.enter_shutdown();
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("b"),
            "after shutdown",
            Utc::now(),
        ));
        s.clear_diagnostics_where(|_| true);
        assert_eq!(s.diagnostics_revision(), rev, "shutdown 后不 bump");
        assert_eq!(s.diagnostics().len(), 1, "map 冻结");
        let snap = s.snapshot(0, 0, 0, 0);
        assert_eq!(snap.diagnostics.len(), 1, "快照仍可读（终态审计）");
    }

    #[tokio::test]
    async fn diagnostics_event_lag_resyncs_from_snapshot() {
        // 验收⑥：事件丢失（Lagged）→ 经 snapshot() 重建（诊断集 + revision）。
        let s = RuntimeStatus::new();
        let mut rx = s.subscribe();
        for i in 0..=EVENT_CHANNEL_CAPACITY + 1 {
            s.record_diagnostic(diag_at(
                DiagnosticCode::McpStartFailed,
                DiagnosticSeverity::Degraded,
                bundle("a"),
                &format!("failure round {i}"),
                Utc::now(),
            ));
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        let snap = s.snapshot(0, 0, 0, 0);
        assert_eq!(snap.diagnostics.len(), 1, "同键替代 → 仍单条最新");
        assert_eq!(
            snap.diagnostics[0].message,
            format!("failure round {}", EVENT_CHANNEL_CAPACITY + 1)
        );
        assert_eq!(
            snap.diagnostics_revision as usize,
            EVENT_CHANNEL_CAPACITY + 2
        );
    }

    #[test]
    fn empty_diagnostics_omitted_from_serde() {
        // #128 兼容姿势：空诊断集不序列化 `diagnostics` 键（干净快照字节不变）。
        let s = RuntimeStatus::new();
        let snap = s.snapshot(0, 0, 0, 0);
        let v = serde_json::to_value(&snap).unwrap();
        assert!(v.get("diagnostics").is_none());
        assert_eq!(v["diagnostics_revision"], serde_json::json!(0));
        // 有诊断 → 键出现。
        s.record_diagnostic(diag_at(
            DiagnosticCode::McpStartFailed,
            DiagnosticSeverity::Degraded,
            bundle("a"),
            "x",
            Utc::now(),
        ));
        let v2 = serde_json::to_value(s.snapshot(0, 0, 0, 0)).unwrap();
        assert!(v2.get("diagnostics").is_some());
    }
}
