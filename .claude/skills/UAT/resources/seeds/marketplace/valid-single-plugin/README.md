# `marketplace/valid-single-plugin`

**Axis**: MK-VAL-01 (happy: 1 plugin 1 skill)

**形态**: 完整 marketplace 工作树 → acceptance 转为本地 bare repo → `marketplace add` 注册

**派生**:

- `plugins/foo/skills/valid-skill-pkg/_seeds.manifest` 指向
  [`_common/valid-skill-pkg`](../../_common/valid-skill-pkg/)
- 目录名 `valid-skill-pkg` 与 `_common` SKILL.md frontmatter `name` 一致（marketplace
  §4 包根目录名 = frontmatter.name 契约）

**期望被测行为（Rust CLI 视角）**:

- `marketplace add file://<bare> --trust --json` 成功，退出码 0
- 注册 1 个 SKILL，name = `foo:valid-skill-pkg`（`<plugin>:<skill>` 合成，与
  `crates/smcp-computer/src/skills/staging.rs` 一致）
- `source` 字段 = `marketplace:uat-seed-mp`
- 物化目录 `<home>/marketplace/uat-seed-mp/` 存在
- 物化包根 `<home>/marketplace/uat-seed-mp/plugins/foo/skills/valid-skill-pkg/SKILL.md` 存在

> 仓库工作树本体与 python-sdk 完全一致（`.tfrobot-plugin` 清单格式 Rust/Python 共用，
> 已对照 `crates/smcp-computer/src/skills/manifest.rs` 确认）。差异仅在 acceptance 驱动：
> 此处驱动 Rust CLI `marketplace add`，而非 Python `stage_marketplace_skills` 函数。
