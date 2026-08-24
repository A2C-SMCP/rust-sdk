# `marketplace/` — Git 仓库种子

> 每条种子是一份**完整可 clone 的 marketplace 仓库工作树**（含 `.tfrobot-plugin/`）。
> Acceptance 期间临时 `git init` + 推到本地 bare 库 + `file://` URL 喂给
> `smcp-computer marketplace add`。

## 子目录结构

```
marketplace/
├── _helpers/
│   └── init_bare_repo.sh         ← 把工作树变成临时 bare repo + 输出 file:// URL（语言无关）
└── <name>/                        ← 一份完整工作树
    ├── .tfrobot-plugin/marketplace.json
    ├── plugins/<plugin>/
    │   ├── (可选) .tfrobot-plugin/plugin.json
    │   ├── skills/<skill>/_seeds.manifest   ← 派生关系 → _common/<x>
    │   └── (可选) mcp-servers/<mcp>.json
    ├── README.md
    └── acceptance.sh
```

## SKILL 内容派生（_seeds.manifest 模式）

每个 `skills/<skill>/` 目录里**只**留 `_seeds.manifest` 描述派生：

```
source: _common/valid-skill-pkg
```

Acceptance 启动时按 manifest 把 `_common/<x>/` 内容拷贝进该目录，再 `git init` 提交
推到 bare 库——保证 `_common` 为单一定义源。

## 不要嵌套 git

种子目录**不带** `.git/`。`_helpers/init_bare_repo.sh` 在 audit / scenario setup 时
即时构造。

## 清单格式兼容性

`.tfrobot-plugin/marketplace.json` 与 `plugin.json` 的目录名/字段 Rust 与 Python **完全一致**
（见 `crates/smcp-computer/src/skills/manifest.rs`：`MARKETPLACE_MANIFEST_DIR=".tfrobot-plugin"`），
因此仓库工作树本体可在两个 SDK 间逐字复用。

## 索引

参见上级 [`seeds/README.md`](../README.md) `marketplace/` 节。
