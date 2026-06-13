/*!
* 文件名: debouncer.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio
* 描述: SKILL 事件去抖器（缓存失效 + 300ms debounce + 单次 emit）
*       SKILL event debouncer (cache invalidation + 300ms debounce + single emit).
*/

//! SKILL 事件去抖器 / SKILL event debouncer（v0.2.1，对标 Python `skills/debouncer.py`）。
//!
//! SDK 设计 / Design: python-sdk `docs/design-0.2.1-cli-marketplace-ux.md` §8.1 / §8.2
//! （借鉴 Claude Code `clearAllCaches` + `skillChangeDetector` 300ms debounce）。
//!
//! 多源 SKILL 变更（mcp `ResourceListChanged` / user 源文件 [`watcher`](super::watcher) /
//! 后续 CLI marketplace·plugin 操作）统一喂给 [`SkillEventDebouncer::mark_dirty`]，在 `window`
//! （默认 [`DEFAULT_DEBOUNCE_MS`]）窗口内合并为**一次** `invalidate → emit` 结算：
//!
//! 1. 窗口到期 → 先 `invalidate`（重扫文件源 / 清缓存），再 `emit`（推送 `server:update_skills`）；
//! 2. 窗口内多次 `mark_dirty` → 取消未到期结算、重排，**末次胜出**（避免抖动期间多次广播）；
//! 3. `emit` 失败 → ERROR 日志 + **单次重试**（设计 §12），仍失败则放弃本轮。
//!
//! 线程模型 / Threading: 去抖器假定在 Computer 的 Tokio 运行时内被驱动；来自其它线程（如 `notify`
//! 观察者线程）的触发**必须**先 marshal 回运行时线程（例如经一个 mpsc / `Handle::spawn`），再调
//! [`SkillEventDebouncer::mark_dirty`]。见 [`watcher`](super::watcher) 的接入说明。
//!
//! 与 Python 的差异 / Difference from Python: Python 用单 asyncio loop 的 `create_task` + `cancel`；
//! 本实现用 [`tokio::spawn`] + [`tokio::task::JoinHandle::abort`] 实现等价的「重排、末次胜出」，
//! 并以内部 [`std::sync::Mutex`] 守护可变状态（回调可跨线程触发，需 `Send + Sync`）。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

/// 默认去抖窗口（毫秒）/ Default debounce window (ms)。对齐 CC `skillChangeDetector` 的 300ms。
pub const DEFAULT_DEBOUNCE_MS: u64 = 300;

/// 结算回调的返回 / Settlement callback result：`Err` → 记 ERROR（emit 还会重试一次）。
pub type CallbackResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// 异步结算回调（`invalidate` / `emit`）/ Async settlement callback。
///
/// 每次结算调用一次，故为 `Fn`（非 `FnOnce`）；返回 `'static + Send` 的 future（跨 spawn 任务执行）。
pub type AsyncCallback =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = CallbackResult> + Send>> + Send + Sync>;

/// 去抖器内部可变状态 / Debouncer mutable state。
struct State {
    /// 当前挂起的结算任务（重排时 abort 旧任务）/ pending settlement task。
    task: Option<JoinHandle<()>>,
    /// [`SkillEventDebouncer::aclose`] 后置位；此后 `mark_dirty` 一律 no-op / closed flag。
    closed: bool,
}

/// SKILL 变更去抖器 / SKILL-change debouncer（设计 §8.2）。
///
/// 构造见 [`SkillEventDebouncer::new`] / [`SkillEventDebouncer::builder`]。结算末端的 `emit`
/// 通常 → `client.emit_update_skills`；`invalidate` 通常 → 重扫文件源 / 清缓存（`None` = 直接 emit）。
pub struct SkillEventDebouncer {
    on_emit: AsyncCallback,
    invalidate: Option<AsyncCallback>,
    window: Duration,
    state: Arc<Mutex<State>>,
}

impl SkillEventDebouncer {
    /// 用默认窗口（[`DEFAULT_DEBOUNCE_MS`]）、无 `invalidate` 构造 / Construct with defaults。
    pub fn new(on_emit: AsyncCallback) -> Self {
        Self::builder(on_emit).build()
    }

    /// 链式构造器 / Builder。
    pub fn builder(on_emit: AsyncCallback) -> SkillEventDebouncerBuilder {
        SkillEventDebouncerBuilder {
            on_emit,
            invalidate: None,
            window_ms: DEFAULT_DEBOUNCE_MS,
        }
    }

    /// 标脏并重排一次结算 / Mark dirty and (re)schedule a settlement。
    ///
    /// 窗口内多次调用 → 取消未到期任务、重排，**末次胜出**（多事件合并为一次 emit）。
    /// [`aclose`](Self::aclose) 后为 **no-op**——中和停机临终投递、滞留队列的跨线程 `mark_dirty`，
    /// 杜绝停机后复活挂起 emit。
    ///
    /// **权衡：纯尾部去抖、无 max-wait 上限**——若事件持续以 `< window` 间隔到来，结算被无限推迟
    /// （emit 饿死）。离散 SKILL.md 编辑不会触发，与 CC `skillChangeDetector` 同义。
    ///
    /// 须在 Tokio 运行时上下文内调用（内部 [`tokio::spawn`]）。
    pub fn mark_dirty(&self) {
        let mut guard = self.state.lock().expect("debouncer state poisoned");
        if guard.closed {
            return;
        }
        if let Some(task) = guard.task.take() {
            task.abort();
        }
        let window = self.window;
        let on_emit = self.on_emit.clone();
        let invalidate = self.invalidate.clone();
        guard.task = Some(tokio::spawn(async move {
            tokio::time::sleep(window).await;
            run_settlement(&on_emit, invalidate.as_ref()).await;
        }));
    }

    /// 立即结算挂起窗口 / Settle the pending window immediately。
    ///
    /// 取消去抖等待，直接 `invalidate → emit`；**无挂起则 no-op**（不会凭空 emit）。主要供单测
    /// **确定性结算**（免 `sleep` 猜时）；停机不走本方法——[`aclose`](Self::aclose) **丢弃**挂起 emit。
    pub async fn aflush(&self) {
        // 取出并 abort 挂起的等待任务（其 sleep 尚未结算）；锁不跨 await。
        let aborted = {
            let mut guard = self.state.lock().expect("debouncer state poisoned");
            match guard.task.take() {
                Some(task) => {
                    task.abort();
                    true
                }
                None => false,
            }
        };
        if aborted {
            run_settlement(&self.on_emit, self.invalidate.as_ref()).await;
        }
    }

    /// 关闭去抖器，丢弃未结算的挂起 emit / Close the debouncer, dropping any pending emit。
    ///
    /// 生命周期清理（停机）：先置 `closed`（此后 [`mark_dirty`](Self::mark_dirty) 一律 no-op），再
    /// abort 挂起任务（幂等）。**不**冲刷——停机时无需再广播。
    pub async fn aclose(&self) {
        let task = {
            let mut guard = self.state.lock().expect("debouncer state poisoned");
            guard.closed = true;
            guard.task.take()
        };
        if let Some(task) = task {
            task.abort();
            // 等待任务真正结束（abort 后 join 返回 Cancelled）；幂等。
            let _ = task.await;
        }
    }

    /// 是否有挂起未结算的窗口 / Whether a settlement is currently pending（供测试 / 内省）。
    pub fn has_pending(&self) -> bool {
        let guard = self.state.lock().expect("debouncer state poisoned");
        guard.task.as_ref().is_some_and(|t| !t.is_finished())
    }
}

/// 结算一次：先缓存失效（失败不阻断 emit），再 emit（失败 ERROR + 单次重试）。
async fn run_settlement(on_emit: &AsyncCallback, invalidate: Option<&AsyncCallback>) {
    if let Some(invalidate) = invalidate {
        if let Err(e) = invalidate().await {
            tracing::error!(error = %e, "SKILL 缓存失效失败（仍继续 emit）/ invalidate failed (emit anyway)");
        }
    }
    if let Err(e) = on_emit().await {
        tracing::error!(error = %e, "emit_update_skills 失败，重试一次 / emit failed, retrying once");
        if let Err(e2) = on_emit().await {
            tracing::error!(error = %e2, "emit_update_skills 重试仍失败，放弃本轮 / emit retry failed, giving up");
        }
    }
}

/// [`SkillEventDebouncer`] 构造器 / builder。
pub struct SkillEventDebouncerBuilder {
    on_emit: AsyncCallback,
    invalidate: Option<AsyncCallback>,
    window_ms: u64,
}

impl SkillEventDebouncerBuilder {
    /// 设置结算前的缓存失效回调 / Set the pre-emit invalidate callback。
    #[must_use]
    pub fn invalidate(mut self, invalidate: AsyncCallback) -> Self {
        self.invalidate = Some(invalidate);
        self
    }

    /// 设置去抖窗口毫秒数（`0` → 下个 tick 即结算）/ Set the debounce window (ms)。
    #[must_use]
    pub fn window_ms(mut self, window_ms: u64) -> Self {
        self.window_ms = window_ms;
        self
    }

    /// 完成构造 / Build。
    pub fn build(self) -> SkillEventDebouncer {
        SkillEventDebouncer {
            on_emit: self.on_emit,
            invalidate: self.invalidate,
            window: Duration::from_millis(self.window_ms),
            state: Arc::new(Mutex::new(State {
                task: None,
                closed: false,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 构造一个统计调用次数的 emit 回调 / An emit callback counting its calls。
    fn counting_callback(counter: Arc<AtomicUsize>) -> AsyncCallback {
        Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn aflush_no_pending_is_noop() {
        let emits = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::new(counting_callback(emits.clone()));
        // 无挂起 → aflush 不应凭空 emit。
        deb.aflush().await;
        assert_eq!(emits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn aflush_settles_once_after_coalescing() {
        let emits = Arc::new(AtomicUsize::new(0));
        let invalidations = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::builder(counting_callback(emits.clone()))
            .invalidate(counting_callback(invalidations.clone()))
            .build();

        // 窗口内多次标脏（无 await 间隔，前序任务被 abort）→ 末次胜出。
        deb.mark_dirty();
        deb.mark_dirty();
        deb.mark_dirty();
        deb.aflush().await;

        assert_eq!(emits.load(Ordering::SeqCst), 1, "多源合并应只 emit 一次");
        assert_eq!(
            invalidations.load(Ordering::SeqCst),
            1,
            "结算前 invalidate 一次"
        );
        assert!(!deb.has_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn paused_clock_emits_once_after_window() {
        let emits = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::new(counting_callback(emits.clone()));

        deb.mark_dirty();
        // 让 spawn 的结算任务跑到 `sleep` 注册点（虚拟 t=0 + 窗口），再推进虚拟时钟。
        tokio::task::yield_now().await;
        // 未到窗口：虚拟时钟前进不足，不应结算。
        tokio::time::advance(Duration::from_millis(DEFAULT_DEBOUNCE_MS - 50)).await;
        tokio::task::yield_now().await;
        assert_eq!(emits.load(Ordering::SeqCst), 0, "窗口未到不应 emit");

        // 越过窗口：结算任务被唤醒并执行一次 emit。
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(emits.load(Ordering::SeqCst), 1, "窗口到期应 emit 一次");
    }

    #[tokio::test(start_paused = true)]
    async fn paused_clock_latest_wins_resets_window() {
        let emits = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::new(counting_callback(emits.clone()));

        deb.mark_dirty();
        tokio::task::yield_now().await; // T1 注册 sleep@300
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        // 第二次标脏在窗口内 → 重排（前序 200ms 作废，T1 被 abort）。
        deb.mark_dirty();
        tokio::task::yield_now().await; // T2 注册 sleep@(200+300=500)
        tokio::time::advance(Duration::from_millis(200)).await; // t=400 < 500
        tokio::task::yield_now().await;
        assert_eq!(emits.load(Ordering::SeqCst), 0, "重排后窗口未满，不应 emit");

        tokio::time::advance(Duration::from_millis(150)).await; // t=550 > 500
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            emits.load(Ordering::SeqCst),
            1,
            "自末次标脏满窗口后 emit 一次"
        );
    }

    #[tokio::test]
    async fn emit_failure_retried_once() {
        let attempts = Arc::new(AtomicUsize::new(0));
        // 首次失败、其后成功 → 验证「失败重试一次」。
        let attempts_cb = attempts.clone();
        let on_emit: AsyncCallback = Arc::new(move || {
            let attempts = attempts_cb.clone();
            Box::pin(async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err("transient emit failure".into())
                } else {
                    Ok(())
                }
            })
        });
        let deb = SkillEventDebouncer::new(on_emit);
        deb.mark_dirty();
        deb.aflush().await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "首次失败 + 重试一次 = 2 次尝试"
        );
    }

    #[tokio::test]
    async fn mark_dirty_after_close_is_noop() {
        let emits = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::new(counting_callback(emits.clone()));
        deb.aclose().await;
        deb.mark_dirty();
        deb.aflush().await; // 关闭后无挂起 → 仍 no-op
        assert_eq!(
            emits.load(Ordering::SeqCst),
            0,
            "aclose 后 mark_dirty 应 no-op"
        );
        assert!(!deb.has_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn aclose_drops_pending_emit() {
        let emits = Arc::new(AtomicUsize::new(0));
        let deb = SkillEventDebouncer::new(counting_callback(emits.clone()));
        deb.mark_dirty();
        // 窗口未到即关闭 → 挂起 emit 被丢弃。
        deb.aclose().await;
        tokio::time::advance(Duration::from_millis(DEFAULT_DEBOUNCE_MS + 100)).await;
        tokio::task::yield_now().await;
        assert_eq!(emits.load(Ordering::SeqCst), 0, "aclose 应丢弃挂起 emit");
    }
}
