/*!
* 文件名: watcher.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: notify, smcp::utils::path
* 描述: SKILL user 源文件 watcher（监控 DropIn 发现根，SKILL.md 变更 → 去抖 emit）
*       SKILL user-source file watcher (watch DropIn roots, SKILL.md change → debounced emit).
*/

//! SKILL 文件 watcher / SKILL file watcher（v0.2.1，对标 Python `skills/watcher.py`）。
//!
//! SDK 设计 / Design: python-sdk `docs/design-0.2.1-cli-marketplace-ux.md` §8.3。
//!
//! 监控范围 / Scope（§8.3）：
//! - 监控根（**递归**）= `$A2C_SKILL_HOME/user/` + **全部已登记** `<workdir>/.tfrobot/skills/`（能力发现层、
//!   全局并集、不随 active workdir 切换）；过滤器为 `**/SKILL.md`。**绝不监** marketplace clone 树
//!   （`<home>/marketplace/<mp>/...`）——clone 树是物化产物，变更只经 CLI 操作发生（操作自调去抖标脏），
//!   监控它会引发 `git pull` 雪崩并破坏「意图层 / 物化层」单向同步边界。
//! - **监控范围 ≠ 发现单元**：watcher 监根递归子树并过滤 `SKILL.md`；深度过滤由
//!   [`stage_user_skills`](super::staging::stage_user_skills) 在重扫时负责，watcher 只管「有 SKILL.md
//!   变更 → 标脏」。
//!
//! 触发规则 / Trigger rule（见 [`should_fire`]）：
//! - `SKILL.md` 文件 created/modified/deleted/renamed → 触发（经 [`SkillFileWatcher::mark_internal_write`]
//!   打标的自写除外）；
//! - **目录** removed（含不可判定的 `Remove(Any)`）/ renamed → 触发（`rm -rf <skill>/` / `mv` 在部分平台
//!   仅报目录事件、不逐文件，避免漏删）；
//! - 其余（目录 created、非 SKILL.md 附属文件 create/modify）→ 忽略。
//!   偶发过触发由下游 300ms 去抖 + 重扫集合对比吸收（宁可多扫一次，不可漏掉删除）。
//!
//! 线程模型 / Threading：`notify` 观察者回调在**独立线程**触发。本 watcher 的 `on_change` 回调由调用方注入，
//! **必须**自行做线程安全 marshal（通常把
//! [`SkillEventDebouncer::mark_dirty`](super::debouncer::SkillEventDebouncer::mark_dirty) 经一个 channel /
//! `Handle::spawn` marshal 回 Computer 的 Tokio 运行时线程），见 Computer 集成（#68）。
//!
//! 实现 / Impl：默认 [`notify::RecommendedWatcher`]（inotify/FSEvents/ReadDirectoryChangesW）；
//! `use_polling = true` 切 [`notify::PollWatcher`]，给不支持原生事件的 FS（某些网络挂载 / 容器
//! overlayfs）兜底。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, PollWatcher, RecursiveMode, Watcher};
use smcp::utils::path::normalize_lexical;

use crate::skills::sandbox::DEFAULT_SKILL_FILE;

/// 内部写打标默认存活窗口 / Default TTL for internal-write marks。对齐 CC ~2s。
pub const DEFAULT_INTERNAL_WRITE_TTL: Duration = Duration::from_millis(2000);

/// `PollWatcher` 下的 TTL 下限 / TTL floor under PollWatcher。
///
/// 轮询模式自写事件最迟于下个轮询周期才上报，若 TTL < 轮询周期，自写事件可能在打标过期后才到达 →
/// 逃过抑制 → 触发「写回 → watcher → 重载」自触发。故 polling 时把 TTL 抬到 ≥ 轮询周期 + 余量。
pub const POLLING_INTERNAL_WRITE_TTL_FLOOR: Duration = Duration::from_millis(5000);

/// `PollWatcher` 默认轮询周期 / Default poll interval（notify 无原生事件时的兜底扫描间隔）。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 线程安全的 `on_change` marshaller 类型 / Thread-safe on-change marshaller。
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// watcher 启动失败 / Watcher start failure（`notify` 观察者创建失败）。
#[derive(Debug, thiserror::Error)]
pub enum SkillWatchError {
    /// `notify` 底层错误 / underlying `notify` error。
    #[error("notify watcher error: {0}")]
    Notify(#[from] notify::Error),
}

/// 事件路径 basename 是否为 `SKILL.md` / Whether a path's basename is `SKILL.md`。
fn is_skill_md(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(DEFAULT_SKILL_FILE)
}

/// 纯事件过滤决策 / Pure event-filter decision（见模块「触发规则」）。
///
/// 返回 `true` 表示该事件应触发 `on_change`。`is_internal` 判定某路径是否处于未过期的内部写打标窗口内
/// （自写抑制）。本函数无副作用、可独立单测（喂合成 [`Event`]）。
pub fn should_fire(event: &Event, is_internal: impl Fn(&str) -> bool) -> bool {
    // 规则 1：任一路径 basename 为 SKILL.md 且非内部写 → 触发（覆盖 create/modify/delete/rename，含 dest）。
    let skill_md_changed = event
        .paths
        .iter()
        .any(|p| is_skill_md(p) && !is_internal(&p.to_string_lossy()));
    if skill_md_changed {
        return true;
    }
    // 规则 2：目录级 remove / rename（保守覆盖 `rm -rf <skill>/`、`mv <skill>`；`Remove(Any)` 无法判定
    //         目录性，宁可多扫一次也不漏删）。目录 create / 非 SKILL.md 文件的 create·modify → 不触发。
    matches!(
        event.kind,
        EventKind::Remove(RemoveKind::Folder)
            | EventKind::Remove(RemoveKind::Any)
            | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// 归一路径为比较键 / Normalize a path to a comparison key。
///
/// 优先 `canonicalize`（与 `notify` 上报路径对齐、解析符号链接）；对**不存在**的路径（如尚未落盘的自写
/// 目标）回退 [`normalize_lexical`]。mark 与事件两侧同走本函数，保证键一致。
fn normalize_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| normalize_lexical(path))
        .to_string_lossy()
        .into_owned()
}

/// 判定路径是否处于未过期的内部写打标窗口内（顺带清理过期项）/ Internal-write check at a given instant。
///
/// `now` 显式注入以便确定性单测（生产传 [`Instant::now`]）。
fn is_internal_write_at(map: &Mutex<HashMap<String, Instant>>, path: &str, now: Instant) -> bool {
    let key = normalize_key(Path::new(path));
    let mut guard = map.lock().expect("internal-write map poisoned");
    guard.retain(|_, exp| *exp > now);
    guard.get(&key).is_some_and(|exp| *exp > now)
}

/// user 源 DropIn 文件 watcher（`notify` 集成）/ User-source DropIn file watcher。
///
/// 生命周期由 Computer 管理：[`watch`](Self::watch) 启动、[`stop`](Self::stop) 停止（drop 观察者）。
pub struct SkillFileWatcher {
    on_change: OnChange,
    use_polling: bool,
    internal_ttl: Duration,
    poll_interval: Duration,
    internal_writes: Arc<Mutex<HashMap<String, Instant>>>,
    watcher: Option<Box<dyn Watcher + Send>>,
    watched: Vec<PathBuf>,
}

impl SkillFileWatcher {
    /// 用默认参数构造（原生 Observer，TTL 2s）/ Construct with defaults。
    ///
    /// `on_change`：检测到相关变更时调用的**线程安全** marshaller（在 `notify` 观察者线程内被调用）。
    pub fn new(on_change: OnChange) -> Self {
        Self::builder(on_change).build()
    }

    /// 链式构造器 / Builder。
    pub fn builder(on_change: OnChange) -> SkillFileWatcherBuilder {
        SkillFileWatcherBuilder {
            on_change,
            use_polling: false,
            internal_write_ttl: DEFAULT_INTERNAL_WRITE_TTL,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// 登记一次 SDK 自写路径 / Mark a path the SDK itself just wrote。
    ///
    /// 其 TTL 窗口内对该路径的 SKILL.md 事件将被忽略，避免「写回 → watcher → 重载 → 写回」自触发循环
    /// （对标 CC `settings.ts` `markInternalWrite`）。线程安全。
    pub fn mark_internal_write(&self, path: impl AsRef<Path>) {
        let key = normalize_key(path.as_ref());
        let expiry = Instant::now() + self.internal_ttl;
        self.internal_writes
            .lock()
            .expect("internal-write map poisoned")
            .insert(key, expiry);
    }

    /// 对全部**存在**的发现根注册递归监控并启动观察者 / Schedule recursive watches on existing roots。
    ///
    /// 缺失根跳过 + DEBUG（容错：user/ 或某 workdir 尚未创建很正常）；重复根去重；**无可监控根 → 不启动**
    /// （避免空观察者线程）。单根 schedule 失败记 WARN 但不中断其余。重复调用前请先 [`stop`](Self::stop)。
    pub fn watch<I, P>(&mut self, roots: I) -> Result<(), SkillWatchError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut existing: Vec<PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for root in roots {
            let rp = std::fs::canonicalize(root.as_ref())
                .unwrap_or_else(|_| normalize_lexical(root.as_ref()));
            let key = rp.to_string_lossy().into_owned();
            if !seen.insert(key) {
                continue;
            }
            if !rp.is_dir() {
                tracing::debug!(root = %rp.display(), "SKILL watcher 跳过不存在的发现根 / skip missing root");
                continue;
            }
            existing.push(rp);
        }

        if existing.is_empty() {
            tracing::debug!(
                "SKILL watcher 无可监控发现根，观察者不启动 / no watchable roots, not started"
            );
            return Ok(());
        }

        // 事件回调：在 notify 观察者线程内运行，过滤后 marshal 给注入的 on_change。
        let on_change = self.on_change.clone();
        let internal_writes = self.internal_writes.clone();
        let handler = move |res: notify::Result<Event>| match res {
            Ok(event) => {
                let is_internal =
                    |p: &str| is_internal_write_at(&internal_writes, p, Instant::now());
                if should_fire(&event, is_internal) {
                    on_change();
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "SKILL watcher 事件错误 / watch error");
            }
        };

        let mut watcher: Box<dyn Watcher + Send> = if self.use_polling {
            Box::new(PollWatcher::new(
                handler,
                Config::default().with_poll_interval(self.poll_interval),
            )?)
        } else {
            Box::new(notify::recommended_watcher(handler)?)
        };

        for rp in &existing {
            if let Err(e) = watcher.watch(rp, RecursiveMode::Recursive) {
                tracing::warn!(root = %rp.display(), error = %e, "SKILL watcher 注册监控失败（跳过该根）/ failed to watch root");
            } else {
                tracing::debug!(root = %rp.display(), "SKILL watcher 注册递归监控 / watching (recursive)");
            }
        }
        tracing::info!(
            roots = existing.len(),
            "SKILL 文件 watcher 已启动 / started"
        );
        self.watcher = Some(watcher);
        self.watched = existing;
        Ok(())
    }

    /// 停止观察者（drop 底层 watcher 即停止并清理线程）/ Stop the observer（幂等；未启动 → no-op）。
    pub fn stop(&mut self) {
        self.watcher = None;
        self.watched.clear();
    }

    /// 观察者是否在运行 / Whether the observer is running。
    pub fn is_running(&self) -> bool {
        self.watcher.is_some()
    }

    /// 当前实际监控的发现根（已存在并 schedule 的）/ Currently scheduled (existing) roots。
    pub fn watched_roots(&self) -> &[PathBuf] {
        &self.watched
    }
}

/// [`SkillFileWatcher`] 构造器 / builder。
pub struct SkillFileWatcherBuilder {
    on_change: OnChange,
    use_polling: bool,
    internal_write_ttl: Duration,
    poll_interval: Duration,
}

impl SkillFileWatcherBuilder {
    /// 切换轮询模式（不支持原生事件的 FS 兜底）/ Use the polling observer。
    #[must_use]
    pub fn use_polling(mut self, use_polling: bool) -> Self {
        self.use_polling = use_polling;
        self
    }

    /// 设置内部写打标存活窗口 / Set the internal-write mark TTL。
    #[must_use]
    pub fn internal_write_ttl(mut self, ttl: Duration) -> Self {
        self.internal_write_ttl = ttl;
        self
    }

    /// 设置 `PollWatcher` 轮询周期（仅 `use_polling` 时生效）/ Set the poll interval。
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// 完成构造 / Build。polling 模式把 TTL 抬到 ≥ 轮询周期下限（见 [`POLLING_INTERNAL_WRITE_TTL_FLOOR`]）。
    pub fn build(self) -> SkillFileWatcher {
        let internal_ttl = if self.use_polling {
            self.internal_write_ttl
                .max(POLLING_INTERNAL_WRITE_TTL_FLOOR)
        } else {
            self.internal_write_ttl
        };
        SkillFileWatcher {
            on_change: self.on_change,
            use_polling: self.use_polling,
            internal_ttl,
            poll_interval: self.poll_interval,
            internal_writes: Arc::new(Mutex::new(HashMap::new())),
            watcher: None,
            watched: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RenameMode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    fn never_internal(_: &str) -> bool {
        false
    }

    #[test]
    fn fires_on_skill_md_create_modify_delete() {
        let p = "/home/user/skills/foo/SKILL.md";
        assert!(should_fire(
            &event(EventKind::Create(CreateKind::File), &[p]),
            never_internal
        ));
        assert!(should_fire(
            &event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &[p]
            ),
            never_internal
        ));
        assert!(should_fire(
            &event(EventKind::Remove(RemoveKind::File), &[p]),
            never_internal
        ));
    }

    #[test]
    fn ignores_non_skill_md_file_and_dir_create() {
        // 非 SKILL.md 附属文件 create/modify → 忽略。
        assert!(!should_fire(
            &event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &["/home/user/skills/foo/helper.py"]
            ),
            never_internal
        ));
        // 目录 create → 忽略（Python on_created 仅对文件触发）。
        assert!(!should_fire(
            &event(
                EventKind::Create(CreateKind::Folder),
                &["/home/user/skills/foo"]
            ),
            never_internal
        ));
        // 非 SKILL.md 文件 remove（已知是文件）→ 忽略。
        assert!(!should_fire(
            &event(
                EventKind::Remove(RemoveKind::File),
                &["/home/user/skills/foo/helper.py"]
            ),
            never_internal
        ));
    }

    #[test]
    fn fires_on_dir_remove_and_rename() {
        // `rm -rf <skill>/` 在部分平台仅报目录删除 → 触发。
        assert!(should_fire(
            &event(
                EventKind::Remove(RemoveKind::Folder),
                &["/home/user/skills/foo"]
            ),
            never_internal
        ));
        // 不可判定的 Remove(Any) → 保守触发（不漏删）。
        assert!(should_fire(
            &event(
                EventKind::Remove(RemoveKind::Any),
                &["/home/user/skills/foo"]
            ),
            never_internal
        ));
        // rename / move（目录改名或 → SKILL.md）→ 触发。
        assert!(should_fire(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/home/user/skills/foo", "/home/user/skills/bar"]
            ),
            never_internal
        ));
    }

    #[test]
    fn fires_on_rename_dest_skill_md() {
        // 原子保存：tmp → SKILL.md，dest basename 命中规则 1。
        assert!(should_fire(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[
                    "/home/user/skills/foo/.tmp123",
                    "/home/user/skills/foo/SKILL.md"
                ]
            ),
            never_internal
        ));
    }

    #[test]
    fn internal_write_suppresses_skill_md() {
        let p = "/home/user/skills/foo/SKILL.md";
        // SKILL.md 事件，但谓词判其为内部写 → 不触发。
        assert!(!should_fire(
            &event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &[p]
            ),
            |_| true
        ));
    }

    #[test]
    fn internal_write_marking_respects_ttl_with_injected_now() {
        let map: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        let base = Instant::now();
        let path = "/tmp/some/SKILL.md";
        let key = normalize_key(Path::new(path));
        map.lock()
            .unwrap()
            .insert(key.clone(), base + Duration::from_millis(100));

        // 窗口内（base+50ms）→ 仍是内部写。
        assert!(is_internal_write_at(
            &map,
            path,
            base + Duration::from_millis(50)
        ));
        // 过期后（base+150ms）→ 不再是内部写，且过期项被清理。
        assert!(!is_internal_write_at(
            &map,
            path,
            base + Duration::from_millis(150)
        ));
        assert!(map.lock().unwrap().is_empty(), "过期项应被清理");
    }

    #[test]
    fn watch_dedups_and_skips_missing_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("user");
        std::fs::create_dir_all(&root).unwrap();
        let missing = tmp.path().join("nonexistent");

        let fired = Arc::new(AtomicUsize::new(0));
        let fired_cb = fired.clone();
        let mut watcher = SkillFileWatcher::new(Arc::new(move || {
            fired_cb.fetch_add(1, Ordering::SeqCst);
        }));

        // 传入重复根 + 缺失根 → 去重 + 跳过缺失。
        watcher
            .watch([root.clone(), root.clone(), missing])
            .unwrap();
        assert!(watcher.is_running());
        assert_eq!(watcher.watched_roots().len(), 1, "去重 + 跳过缺失后仅 1 根");

        watcher.stop();
        assert!(!watcher.is_running());
        assert!(watcher.watched_roots().is_empty());
    }

    #[test]
    fn watch_no_roots_does_not_start() {
        let mut watcher = SkillFileWatcher::new(Arc::new(|| {}));
        let empty: Vec<PathBuf> = Vec::new();
        watcher.watch(empty).unwrap();
        assert!(!watcher.is_running(), "无可监控根 → 不启动");
    }

    /// 真实 FS 烟测（PollWatcher，确定性轮询 + 限时轮询断言）/ real-FS smoke via PollWatcher。
    ///
    /// 用轮询观察者（跨平台行为最稳定）+ 100ms 周期；创建 SKILL.md 后在 ≤3s 内轮询等待 emit。
    #[test]
    fn poll_watcher_fires_on_real_skill_md_create() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("user");
        std::fs::create_dir_all(&root).unwrap();

        let fired = Arc::new(AtomicUsize::new(0));
        let fired_cb = fired.clone();
        let mut watcher = SkillFileWatcher::builder(Arc::new(move || {
            fired_cb.fetch_add(1, Ordering::SeqCst);
        }))
        .use_polling(true)
        .poll_interval(Duration::from_millis(100))
        .build();
        watcher.watch([root.clone()]).unwrap();

        // 落一个 SKILL.md（发现单元 <root>/<skill>/SKILL.md）。
        let skill_dir = root.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), b"---\nname: my-skill\n---\n# x").unwrap();

        // 限时轮询等待事件传播（≤3s，30 × 100ms）。
        let mut ok = false;
        for _ in 0..30 {
            if fired.load(Ordering::SeqCst) > 0 {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        watcher.stop();
        assert!(ok, "PollWatcher 应在限时内对 SKILL.md 创建触发 on_change");
    }
}
