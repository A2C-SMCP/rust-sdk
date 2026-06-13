# `user/home-user-basic`
**Axis**: US-VAL-01（user 源 happy，`$A2C_SKILL_HOME/user/`）
**派生**: `_seeds.manifest` → `_common/valid-skill-pkg`
**期望被测行为（Rust CLI）**:
- 把 `_common/valid-skill-pkg/` 拷进 `$A2C_SKILL_HOME/user/valid-skill-pkg/`（目录名=frontmatter name）
- `skill list --source user --json` 列出 `valid-skill-pkg`，`source: "user"`，`enabled:true`、`orphan:false`
- SKILL 文件就地，不被拷走
