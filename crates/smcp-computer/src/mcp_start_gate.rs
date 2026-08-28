/*!
* 文件名: mcp_start_gate.rs
* 作者: Claude Code
* 创建日期: 2026/08/28
* 描述: Computer 级 MCP 启动并发门控 / Computer-level MCP startup concurrency gate.
*
* #214 立项：单个启动、批量启动、Plugin 治理恢复必须共享**同一个** Computer 级并发控制器，
* 避免各调用方分别限流后突破总上限（tfrobot-client#65「不在客户端维护第二套 Semaphore」）。
*
* 语义要点（与协议 runtime-contract §4.7 / §5.13 对齐）：
* - 「完整启动事务」持许可：permit 覆盖 render（input 重解析）→ manager start 全链，Drop 即释放。
* - 许可之外，[`crate::computer::Computer`] 还有一把**全局 Input 解析串行锁**（不在本模块）——
*   每事务内 render 只允许一个交互请求在飞。
* - 未配置上限（`max = None`）= 门不限流，但**仍计数**（shutdown 等待 / 观测）。
*   批量启动器在未配置时按逐项串行驱动（保持既有行为），见 `Computer::start_mcp_clients_batch`。
* - `close()`：shutdown 终态拒新许可（排队者全部即刻失败——tokio Semaphore 内部无丢失唤醒竞态）；
*   在途事务由调用方既有生命周期排他（`mcp_lifecycle_gate` 写锁）等待收敛。
*   本模块不创建 detached task。
*/

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::errors::ComputerError;

/// 许可限额状态：`max = None` 代表不限流（许可数用 [`Semaphore::MAX_PERMITS`] 表达，
/// 实际不构成约束）；`Some(n)` 即有限并发。
#[derive(Debug)]
struct GateLimiter {
    max: Option<usize>,
    semaphore: Arc<Semaphore>,
}

/// Computer 级 MCP 启动并发门控。
///
/// 一个 Computer 共享一把门；`Computer::start_mcp_client` 与 `restart_mcp_client` 在事务
/// 入口 `acquire()`，保证任意时刻**总在途启动数 ≤ `max`**（跨调用方：用户 MCP / Plugin MCP /
/// 内建 MCP / 重叠批次）。
#[derive(Debug)]
pub(crate) struct McpStartGate {
    /// 许可限额（`Some` = 有限并发；`None` = 不限流）。构造后经
    /// [`with_mcp_start_concurrency`](crate::computer::Computer::with_mcp_start_concurrency) 安装上限，
    /// 安装必须发生在任何启动之前（构造期策略）。
    limiter: RwLock<GateLimiter>,
    /// 观测/等待用的在途计数（与 Semaphore 许可同步增减）。
    in_flight: AtomicUsize,
    /// 任一 acquire 尝试发生后置真——此后安装上限视为编程错误（fail-closed）。
    started: AtomicBool,
}

/// 持有的启动许可；Drop 时释放并唤醒一个等待者。
pub(crate) struct StartPermit {
    /// 先 drop 许可（Semaphore 容量立即回补），再 drop 门引用。
    /// 仅依赖其 Drop 副作用回补容量，无需读取。
    _permit: Option<OwnedSemaphorePermit>,
    gate: Arc<McpStartGate>,
}

impl std::fmt::Debug for StartPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartPermit")
    }
}

impl Drop for StartPermit {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl McpStartGate {
    /// 构造门；`max := None` 用 Semaphore 极值表达「不限流」。
    pub(crate) fn new(max: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            limiter: RwLock::new(GateLimiter {
                max,
                semaphore: Arc::new(Semaphore::new(max.unwrap_or(Semaphore::MAX_PERMITS))),
            }),
            in_flight: AtomicUsize::new(0),
            started: AtomicBool::new(false),
        })
    }

    /// 安装/替换并发上限（构造期策略，一次生效；`0` 按 `1` 处理——`0` 意味着「永不启动」，无意义）。
    ///
    /// 🔴 **fail-closed**：任何 acquire 尝试（含失败者，如 shutdown 后的拒绝）之后调用即 panic——
    /// 运行期替换会让已在旧 Semaphore 上排队的等待者脱离新上限、乃至悬挂（`Computer: Clone` 共享
    /// 同一 gate，`clone().with_mcp_start_concurrency(..)` 是可达用法）。配置必须在启动前完成。
    pub(crate) fn set_max(&self, max: usize) {
        assert!(
            !self.started.load(Ordering::Acquire),
            "MCP start concurrency must be configured at construction time, before any start transaction"
        );
        let mut limiter = self.limiter.write().expect("gate limiter lock");
        limiter.max = Some(max.max(1));
        limiter.semaphore = Arc::new(Semaphore::new(max.max(1)));
    }

    /// 当前并发上限（`None` = 未配置/不限流）。
    pub(crate) fn max(&self) -> Option<usize> {
        self.limiter.read().expect("gate limiter lock").max
    }

    /// 当前在途事务数（仅测试观测；生产路径经 `mcp_lifecycle_gate` 写锁天然收敛，无需读取）。
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// 是否已终态关闭（仅测试断言）。
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.limiter
            .read()
            .expect("gate limiter lock")
            .semaphore
            .is_closed()
    }

    /// 取得许可：`closed` → 立即报错（shutdown 后不再接纳）；达上限 → 排队等待许可释放
    /// （Semaphore 无丢失唤醒竞态，关闭即失败返回）。
    pub(crate) async fn acquire(self: &Arc<Self>) -> Result<StartPermit, ComputerError> {
        self.started.store(true, Ordering::Release);
        let semaphore = self
            .limiter
            .read()
            .expect("gate limiter lock")
            .semaphore
            .clone();
        let permit = semaphore.acquire_owned().await.map_err(|_| {
            ComputerError::InvalidState(
                "MCP startup gate is closed (Computer is shutting down)".to_string(),
            )
        })?;
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Ok(StartPermit {
            _permit: Some(permit),
            gate: Arc::clone(self),
        })
    }

    /// 终态关闭：不再接纳新许可，全部排队者即刻失败。
    pub(crate) fn close(&self) {
        self.limiter
            .read()
            .expect("gate limiter lock")
            .semaphore
            .close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_default_uncapped_and_close_rejects() {
        let gate = McpStartGate::new(None);
        assert_eq!(gate.max(), None);
        let p = gate.acquire().await.unwrap();
        assert_eq!(gate.in_flight(), 1);
        drop(p);
        assert_eq!(gate.in_flight(), 0);
        gate.close();
        assert!(matches!(
            gate.acquire().await,
            Err(ComputerError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn gate_caps_and_releases_in_order() {
        let gate = McpStartGate::new(Some(2));
        let p1 = gate.acquire().await.unwrap();
        let p2 = gate.acquire().await.unwrap();
        assert_eq!(gate.in_flight(), 2);
        // 第三个排队；释放一个后放行。
        let g = gate.clone();
        let waiter = tokio::spawn(async move {
            let _p = g.acquire().await.unwrap();
            g.in_flight()
        });
        // 给排队者机会进入等待（确定性：先 sleep 再释放）。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gate.in_flight(), 2, "waiter must stay queued at cap");
        drop(p1);
        assert_eq!(waiter.await.unwrap(), 2, "waiter admitted after release");
        // waiter 任务结束即 drop 其许可（结构化：许可不泄漏出任务）。
        assert_eq!(gate.in_flight(), 1);
        drop(p2);
        assert_eq!(gate.in_flight(), 0);
    }

    #[tokio::test]
    async fn set_max_clamps_zero_to_one() {
        let gate = McpStartGate::new(Some(5));
        gate.set_max(0);
        assert_eq!(gate.max(), Some(1));
        // 替换后新上限生效：第 2 个 acquire 排队。
        let p1 = gate.acquire().await.unwrap();
        let p2 = pin_waiter(&gate);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gate.in_flight(), 1, "cap=1 serializes");
        drop(p1);
        p2.await.unwrap();
    }

    #[tokio::test]
    async fn set_max_after_first_acquire_panics() {
        let gate = McpStartGate::new(None);
        let p = gate.acquire().await.unwrap();
        drop(p);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.set_max(3);
        }));
        assert!(
            result.is_err(),
            "acquire 之后的 set_max 必须 fail-closed panic"
        );
        // acquire 失败的尝试同样锁定构造期（shutdown 后冷启动歧义路径同判为编程错误）。
        gate.close();
        let _ = gate.acquire().await.unwrap_err();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.set_max(3);
        }));
        assert!(result.is_err());
    }

    fn pin_waiter(gate: &Arc<McpStartGate>) -> tokio::task::JoinHandle<()> {
        let g = gate.clone();
        tokio::spawn(async move {
            let _p = g.acquire().await.unwrap();
        })
    }
}
