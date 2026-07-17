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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::RwLock as StdRwLock;

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
    /// 最近一次公开错误（不含 secret）/ last public error (secret-free)。
    pub last_error: Option<String>,
    /// degraded 诊断原因（`lifecycle == Degraded` 时通常非空）/ degraded reason。
    pub degraded_reason: Option<String>,
}

/// runtime 观测事件（[`Computer::subscribe_events`](crate::computer::Computer::subscribe_events) 广播）/ runtime event。
///
/// 事件为**轻量增量**——订阅方收到后可按需调 [`Computer::status`](crate::computer::Computer::status) 取全量快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

// ===========================================================================
// RuntimeStatus 持有者 / holder
// ===========================================================================

/// 公开诊断（last error / degraded reason）/ public diagnostics。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Diagnostics {
    last_error: Option<String>,
    degraded_reason: Option<String>,
}

/// runtime 观测状态持有者（`Computer` 持 `Arc<RuntimeStatus>`，跨 clone 共享同一视图）/ runtime status holder。
///
/// 锁纪律：状态 / revision / shutdown 闸门为原子无锁存取（cheap，`status()` 不阻塞）；诊断字符串走
/// [`std::sync::RwLock`]（临界区仅 clone 短串、**不跨 await**）。事件用 [`tokio::sync::broadcast`]。
pub struct RuntimeStatus {
    /// 生命周期状态（[`LifecycleState`] as u8）/ lifecycle state。
    state: AtomicU8,
    /// config revision 单调计数 / monotonic config revision。
    config_revision: AtomicU64,
    /// capability revision 单调计数 / monotonic capability revision。
    capability_revision: AtomicU64,
    /// shutdown 闸门：`true` 后事件闸断、bump 降 no-op（§4.7）/ shutdown gate。
    shutdown: AtomicBool,
    /// 公开诊断 / diagnostics。
    diagnostics: StdRwLock<Diagnostics>,
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
            shutdown: AtomicBool::new(false),
            diagnostics: StdRwLock::new(Diagnostics::default()),
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

    /// 记 / 清最近公开错误（`None` 清除）/ set-or-clear the last public error。
    pub fn set_last_error(&self, err: Option<String>) {
        self.diagnostics.write().expect("diagnostics poisoned").last_error = err;
    }

    /// 记 / 清 degraded 原因（`None` 清除）/ set-or-clear the degraded reason。
    pub fn set_degraded_reason(&self, reason: Option<String>) {
        self.diagnostics
            .write()
            .expect("diagnostics poisoned")
            .degraded_reason = reason;
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

    /// 是否已 shutdown / whether shutdown has been entered。
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// 用给定汇总计数组装完整快照（状态 / revision / 诊断取自本 holder）/ assemble a full snapshot。
    pub fn snapshot(
        &self,
        mcp_servers: usize,
        active_mcp_servers: usize,
        tools: usize,
        skills: usize,
    ) -> ComputerStatusSnapshot {
        let diag = self.diagnostics.read().expect("diagnostics poisoned").clone();
        ComputerStatusSnapshot {
            lifecycle: self.state(),
            config_revision: self.config_revision(),
            capability_revision: self.capability_revision(),
            mcp_servers,
            active_mcp_servers,
            tools,
            skills,
            last_error: diag.last_error,
            degraded_reason: diag.degraded_reason,
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

    #[test]
    fn diagnostics_set_and_clear_flow_into_snapshot() {
        let s = RuntimeStatus::new();
        s.transition(LifecycleState::Degraded);
        s.set_degraded_reason(Some("marketplace X unreachable".into()));
        s.set_last_error(Some("boot recovery partial".into()));
        let snap = s.snapshot(3, 1, 7, 2);
        assert_eq!(snap.lifecycle, LifecycleState::Degraded);
        assert_eq!(snap.mcp_servers, 3);
        assert_eq!(snap.active_mcp_servers, 1);
        assert_eq!(snap.tools, 7);
        assert_eq!(snap.skills, 2);
        assert_eq!(snap.degraded_reason.as_deref(), Some("marketplace X unreachable"));
        assert_eq!(snap.last_error.as_deref(), Some("boot recovery partial"));
        // 清除 → 快照 None。
        s.set_degraded_reason(None);
        s.set_last_error(None);
        let snap2 = s.snapshot(0, 0, 0, 0);
        assert!(snap2.degraded_reason.is_none());
        assert!(snap2.last_error.is_none());
    }
}
