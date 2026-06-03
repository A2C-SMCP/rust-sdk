/*!
* 文件名: registry.rs
* 作者: JQQ
* 创建日期: 2026/06/03
* 最后修改日期: 2026/06/03
* 版权: 2023 JQQ. All rights reserved.
* 依赖: smcp (A2CSkillRef), indexmap, crate::skills::naming
* 描述: Skill Registry —— name → A2CSkillRef 物化索引 + 孤儿状态机（对标 Python computer/skills/registry.py）
*       Skill Registry: name → A2CSkillRef materialized index + orphan state machine.
*/

//! Skill Registry：`name → A2CSkillRef` 物化索引 / `name → A2CSkillRef` materialized index。
//!
//! 协议依据 / Protocol: a2c-smcp-protocol `skill.md` §1.5（校验失败不入册）、§6（A2CSkillRef）、
//! §8（变更检测 / 孤儿）、§9.2（name 寻址防越权）。对标 Python `a2c_smcp/computer/skills/registry.py`。
//!
//! 职责 / Responsibilities：
//! - **`name → 包根路径` 唯一映射源**（§9.2：禁止从 name 推导 FS 路径；包根仅由本 Registry 经 name 解析）。
//! - **生命周期三动作**（§8）：[`register`](SkillRegistry::register)（新增 + 孤儿恢复）/
//!   [`update`](SkillRegistry::update)（刷新）/ [`mark_orphan`](SkillRegistry::mark_orphan) +
//!   [`recover`](SkillRegistry::recover)（source 消失/回归切孤儿态）。
//! - **校验失败不入册**：name 非法（lexer）/ 缺包根绝对路径 / 同名 active 冲突 → 记 ERROR、**不**写入、
//!   **绝不** panic（§1.5：batch 接口对部分失败健壮）。
//! - **读取返回克隆**：[`resolve`](SkillRegistry::resolve) / [`active_refs`](SkillRegistry::active_refs)
//!   返回 ref 克隆（对齐 Python deepcopy），调用方可安全改写不污染内部状态。
//!
//! 线程模型 / Threading：本结构**不**内建锁（对齐 Python 单线程 asyncio）；与 watcher / reconciler 并发
//! 访问时由上层以 `Arc<Mutex<SkillRegistry>>` 持有（见 SKL-06 / SKL-07 / INT-01）。
//!
//! 顺序 / Ordering：用 [`IndexMap`] 保持**插入顺序**（对齐 Python `dict`），故 [`active_refs`] /
//! [`all_refs`] 按注册先后返回（`get_skills` 不排序、不去重，Agent 当作集合）。

use indexmap::IndexMap;
use std::path::Path;

use smcp::A2CSkillRef;

use crate::skills::naming::parse_skill_name;

/// Registry 内部条目：物化 ref + 孤儿标记 / Internal entry: materialized ref + orphan flag。
#[derive(Debug, Clone)]
struct RegistryEntry {
    skill_ref: A2CSkillRef,
    orphaned: bool,
}

/// `name → A2CSkillRef` 的内存物化索引 / In-memory materialized index of `name → A2CSkillRef`。
///
/// 由 staging 物化后 [`register`](Self::register) / [`update`](Self::update)；变更检测经
/// [`mark_orphan`](Self::mark_orphan) / [`recover`](Self::recover) / [`unregister`](Self::unregister)
/// 维护活跃集。`client:get_skills` 取 [`active_refs`](Self::active_refs)，`client:get_skill` /
/// `client:get_blob` 经 [`resolve`](Self::resolve) 拿包根（孤儿视为不存在）。
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    entries: IndexMap<String, RegistryEntry>,
}

impl SkillRegistry {
    /// 新建空 Registry / Create an empty registry。
    pub fn new() -> Self {
        Self::default()
    }

    /// 共享入参校验：name 非空 + §1 lexer 合法 + path 必选且绝对 / Shared input validation。
    ///
    /// 任一失败 → 记 ERROR、返回 `None`（调用方据此 no-op，**不** panic）。
    fn validated_name(skill_ref: &A2CSkillRef, op: &str) -> Option<String> {
        if skill_ref.name.is_empty() {
            tracing::error!(op, "SkillRegistry skipped: missing 'name' in ref");
            return None;
        }
        if skill_ref.path.is_empty() || !Path::new(&skill_ref.path).is_absolute() {
            tracing::error!(op, name = %skill_ref.name, path = %skill_ref.path,
                "SkillRegistry skipped: 'path' missing or not absolute");
            return None;
        }
        if let Err(e) = parse_skill_name(&skill_ref.name) {
            tracing::error!(op, name = %skill_ref.name, reason = %e.reason,
                "SkillRegistry skipped: invalid name");
            return None;
        }
        Some(skill_ref.name.clone())
    }

    // ── 写入 / mutation ──────────────────────────────────────────────────
    /// **新增**一个物化 SKILL / Register a newly materialized SKILL。
    ///
    /// 校验通过后：同名条目不存在 → 写入活跃条目；同名 **active** 冲突 → §1.5「拒绝第二注册者、保留
    /// 先到者」→ ERROR + `false`；同名 **orphan** → 视为 source 回归，用新 ref 替换并清孤儿 → `true`。
    /// 活跃 SKILL 的内容刷新请用 [`update`](Self::update)。
    pub fn register(&mut self, skill_ref: A2CSkillRef) -> bool {
        let Some(name) = Self::validated_name(&skill_ref, "register") else {
            return false;
        };
        // 同名 active 冲突 → 拒绝（保留先到者）；不存在 / 孤儿 → 写入（孤儿恢复）。
        if matches!(self.entries.get(&name), Some(e) if !e.orphaned) {
            tracing::error!(name = %name,
                "SkillRegistry.register skipped: already registered (keeping first registrant)");
            return false;
        }
        self.entries.insert(
            name,
            RegistryEntry {
                skill_ref,
                orphaned: false,
            },
        );
        true
    }

    /// **刷新**已注册 SKILL 的 ref（§8.2 ResourceUpdated）/ Refresh an already-registered SKILL's ref。
    ///
    /// 原地替换同名条目（active 或 orphan 皆可，替换后置**活跃**）。name 未注册 → ERROR + `false`
    /// （更新不创建；新增走 [`register`](Self::register)）。
    pub fn update(&mut self, skill_ref: A2CSkillRef) -> bool {
        let Some(name) = Self::validated_name(&skill_ref, "update") else {
            return false;
        };
        if !self.entries.contains_key(&name) {
            tracing::error!(name = %name,
                "SkillRegistry.update skipped: not registered (use register for new skills)");
            return false;
        }
        self.entries.insert(
            name,
            RegistryEntry {
                skill_ref,
                orphaned: false,
            },
        );
        true
    }

    /// 幂等写入：已注册同名 → [`update`](Self::update)，否则 [`register`](Self::register) / Idempotent upsert。
    ///
    /// staging 重扫 / 跨 run 复现的统一入口（mcp 与 user 源共用）。
    pub fn register_or_update(&mut self, skill_ref: A2CSkillRef) -> bool {
        if !skill_ref.name.is_empty() && self.entries.contains_key(&skill_ref.name) {
            self.update(skill_ref)
        } else {
            self.register(skill_ref)
        }
    }

    /// 把已注册 SKILL 标为孤儿（从活跃集排除，保留以便恢复）/ Mark a registered SKILL orphaned。
    ///
    /// 未注册 → `false`；已孤儿 → 幂等 `true`。
    pub fn mark_orphan(&mut self, name: &str) -> bool {
        match self.entries.get_mut(name) {
            Some(entry) => {
                entry.orphaned = true;
                true
            }
            None => false,
        }
    }

    /// 恢复孤儿 SKILL（重新纳入活跃集）/ Recover an orphaned SKILL into the active set。
    ///
    /// 未注册或本就活跃 → `false`（防误用）；孤儿 → 置活跃 + `true`。
    pub fn recover(&mut self, name: &str) -> bool {
        match self.entries.get_mut(name) {
            Some(entry) if entry.orphaned => {
                entry.orphaned = false;
                true
            }
            _ => false,
        }
    }

    /// 彻底移除一个 SKILL（孤儿与活跃皆可）/ Remove a SKILL entirely (active or orphaned)。
    ///
    /// 用 `shift_remove` 保持其余条目插入顺序（对齐 Python `dict.pop`）。
    pub fn unregister(&mut self, name: &str) -> bool {
        self.entries.shift_remove(name).is_some()
    }

    // ── 读取（返回克隆，调用方可安全改写）/ lookup (returns clones) ───────
    /// O(1) 活跃精确匹配 / O(1) active exact lookup。
    ///
    /// 仅返回**活跃**（非孤儿）条目 ref 的克隆；name 未注册或已孤儿 → `None`。name→包根唯一解析入口（§9.2）。
    pub fn resolve(&self, name: &str) -> Option<A2CSkillRef> {
        match self.entries.get(name) {
            Some(entry) if !entry.orphaned => Some(entry.skill_ref.clone()),
            _ => None,
        }
    }

    /// 活跃 SKILL 的 ref 克隆列表（排除孤儿，插入序）/ Cloned active SKILL refs (insertion order)。
    pub fn active_refs(&self) -> Vec<A2CSkillRef> {
        self.entries
            .values()
            .filter(|e| !e.orphaned)
            .map(|e| e.skill_ref.clone())
            .collect()
    }

    /// 全部条目的 `(ref 克隆, orphaned)`（含孤儿，插入序）/ Cloned `(ref, orphaned)` for every entry。
    ///
    /// 供 CLI `skill list`（§4.4）——需展示 orphan 标志。
    pub fn all_refs(&self) -> Vec<(A2CSkillRef, bool)> {
        self.entries
            .values()
            .map(|e| (e.skill_ref.clone(), e.orphaned))
            .collect()
    }

    /// name 是否为已注册的孤儿条目 / Whether name is a registered-but-orphaned entry。
    pub fn is_orphan(&self, name: &str) -> bool {
        self.entries.get(name).is_some_and(|e| e.orphaned)
    }

    /// name 是否已注册（含孤儿）/ Whether name is registered (active or orphaned)。
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// 已注册条目总数（含孤儿）/ Total registered entries (active + orphaned)。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否无任何条目 / Whether the registry is empty。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_ref(name: &str) -> A2CSkillRef {
        A2CSkillRef {
            name: name.to_string(),
            source: "user".to_string(),
            uri: None,
            path: format!("/abs/skills/{name}"),
            description: "d".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            version: None,
            skill_metadata: None,
        }
    }

    #[test]
    fn test_register_resolve_and_active() {
        let mut reg = SkillRegistry::new();
        assert!(reg.register(user_ref("my-helper")));
        assert_eq!(reg.resolve("my-helper").unwrap().name, "my-helper");
        assert_eq!(reg.active_refs().len(), 1);
        assert!(reg.contains("my-helper"));
        assert!(reg.resolve("absent").is_none());
    }

    #[test]
    fn test_validation_failures_not_registered() {
        let mut reg = SkillRegistry::new();
        // 非法 lexer name。
        assert!(!reg.register(user_ref("Bad_Name")));
        // 缺 name。
        let mut r = user_ref("x");
        r.name = String::new();
        assert!(!reg.register(r));
        // path 非绝对。
        let mut r = user_ref("rel-path");
        r.path = "relative/p".to_string();
        assert!(!reg.register(r));
        assert!(reg.is_empty());
    }

    #[test]
    fn test_duplicate_active_rejected_keeps_first() {
        let mut reg = SkillRegistry::new();
        let mut first = user_ref("dup");
        first.description = "first".to_string();
        assert!(reg.register(first));
        let mut second = user_ref("dup");
        second.description = "second".to_string();
        assert!(!reg.register(second));
        assert_eq!(reg.resolve("dup").unwrap().description, "first");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_orphan_state_machine() {
        let mut reg = SkillRegistry::new();
        reg.register(user_ref("s"));
        // mark_orphan：从活跃集排除、resolve→None、仍在 registry。
        assert!(reg.mark_orphan("s"));
        assert!(reg.resolve("s").is_none());
        assert!(reg.active_refs().is_empty());
        assert!(reg.is_orphan("s"));
        assert!(reg.contains("s"));
        assert_eq!(reg.all_refs().len(), 1);
        // recover：回活跃。
        assert!(reg.recover("s"));
        assert!(reg.resolve("s").is_some());
        assert!(!reg.is_orphan("s"));
        // recover 已活跃 → false；recover/ mark_orphan 缺失 → false。
        assert!(!reg.recover("s"));
        assert!(!reg.recover("absent"));
        assert!(!reg.mark_orphan("absent"));
    }

    #[test]
    fn test_reregister_orphan_recovers_with_new_ref() {
        let mut reg = SkillRegistry::new();
        reg.register(user_ref("s"));
        reg.mark_orphan("s");
        let mut fresh = user_ref("s");
        fresh.description = "refreshed".to_string();
        assert!(reg.register(fresh)); // 孤儿恢复 + 替换 ref
        assert!(!reg.is_orphan("s"));
        assert_eq!(reg.resolve("s").unwrap().description, "refreshed");
    }

    #[test]
    fn test_update_semantics() {
        let mut reg = SkillRegistry::new();
        // 未注册 → update 拒绝。
        assert!(!reg.update(user_ref("s")));
        reg.register(user_ref("s"));
        let mut upd = user_ref("s");
        upd.description = "v2".to_string();
        assert!(reg.update(upd));
        assert_eq!(reg.resolve("s").unwrap().description, "v2");
        assert_eq!(reg.len(), 1);
        // update 孤儿 → 复活。
        reg.mark_orphan("s");
        assert!(reg.update(user_ref("s")));
        assert!(!reg.is_orphan("s"));
    }

    #[test]
    fn test_register_or_update_and_unregister() {
        let mut reg = SkillRegistry::new();
        assert!(reg.register_or_update(user_ref("s"))); // 不存在 → register
        let mut upd = user_ref("s");
        upd.description = "v2".to_string();
        assert!(reg.register_or_update(upd)); // 存在 → update
        assert_eq!(reg.resolve("s").unwrap().description, "v2");
        assert_eq!(reg.len(), 1);
        // unregister。
        assert!(reg.unregister("s"));
        assert!(!reg.unregister("s"));
        assert!(reg.is_empty());
    }

    #[test]
    fn test_active_excludes_orphans_all_includes() {
        let mut reg = SkillRegistry::new();
        reg.register(user_ref("a"));
        reg.register(user_ref("b"));
        reg.register(user_ref("c"));
        reg.mark_orphan("b");
        let active: Vec<String> = reg.active_refs().into_iter().map(|r| r.name).collect();
        assert_eq!(active, vec!["a", "c"]); // 插入序、排除孤儿
        assert_eq!(reg.all_refs().len(), 3);
        let orphaned: Vec<bool> = reg.all_refs().into_iter().map(|(_, o)| o).collect();
        assert_eq!(orphaned, vec![false, true, false]);
    }

    #[test]
    fn test_resolve_returns_clone_isolated() {
        let mut reg = SkillRegistry::new();
        reg.register(user_ref("s"));
        let mut got = reg.resolve("s").unwrap();
        got.description = "MUTATED".to_string();
        // 内部不受调用方改写影响。
        assert_eq!(reg.resolve("s").unwrap().description, "d");
    }
}
