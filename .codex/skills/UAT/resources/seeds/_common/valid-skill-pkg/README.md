# `_common/valid-skill-pkg`

**Axis**: CM-01

**形态**: happy（well-formed 最小可用 SKILL 包）

**期望被派生使用方式**:

- **marketplace**: `valid-single-plugin/plugins/foo/skills/valid-skill-pkg/_seeds.manifest`
  指向本目录，acceptance 装配时拷入。
- **mcp / user**: 后续场景按需派生（暂未在 Rust 端落地）。

**SKILL.md 关键字段**:

- `name`: valid-skill-pkg
- `description`: 非空，含触发关键词
- `license` / `version` / `allowed-tools` / `compatibility` / `metadata.axis` 均完备

**包结构**:

```
valid-skill-pkg/
├── SKILL.md
├── scripts/run.py
├── references/usage.md
├── README.md
└── acceptance.md
```

**已派生引用**:

- seeds/marketplace/valid-single-plugin/plugins/foo/skills/valid-skill-pkg/

> 注：本包语言无关，逐字复用自 python-sdk。Rust / Python 共用同一 SKILL 包定义源。
