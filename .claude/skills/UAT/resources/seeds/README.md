# UAT 种子库 — 索引（Rust SDK）

> 本目录是 A2C-SMCP Rust SDK UAT 场景的可复用种子库。多数种子工作树与 python-sdk
> 共用（协议清单格式一致），差异集中在 acceptance 驱动（Rust CLI vs Python 函数）。

## 顶层布局

```
seeds/
├── _common/        ← 跨源共享 SKILL 包原料（单一定义源）
├── marketplace/     ← Git 仓库种子
└── (后续) mcp/ user/ ← 随场景迁移逐步补充
```

## 索引

> 维护规则：每条种子**先登记后创建**；废弃前**先**检查"引用 scenarios"列。

### `_common/`

| name | axis | 形态 | acceptance | 派生引用方 |
|---|---|---|---|---|
| valid-skill-pkg | CM-01 | happy | _common/valid-skill-pkg/acceptance.md | marketplace/valid-single-plugin, marketplace/plugin-with-bundled-mcp, user/home-user-basic |
| minimal-greet | CM-04 | happy minimal | (随种子) | marketplace/strict-* |
| minimal-review | CM-05 | happy minimal | (随种子) | marketplace/strict-true-merge, strict-false-clean |
| minimal-scan | CM-06 | happy minimal | (随种子) | marketplace/strict-true-merge |

### `marketplace/`

| name | axis | acceptance | 引用 scenarios |
|---|---|---|---|
| valid-single-plugin | MK-VAL-01 | acceptance.sh | marketplace-ops, skill-discovery |
| plugin-with-bundled-mcp | MK-BMC-01 | acceptance.sh | plugin-management |
| strict-true-merge | MK-STR-TRUE | acceptance.sh | strict-mode (S-01) |
| strict-false-clean | MK-STR-FALSE-CLEAN | acceptance.sh | strict-mode (S-02) |
| strict-false-conflict | MK-STR-FALSE-CONFLICT | acceptance.sh | strict-mode (S-03) |

### `user/`

| name | axis | acceptance | 引用 scenarios |
|---|---|---|---|
| home-user-basic | US-VAL-01 | acceptance.sh | skill-discovery (D-02) |

## 迁移进度（自 python-sdk）

> Rust UAT 采用「纵向切片」迁移：先打通 marketplace-ops 一个场景闭环，再批量补齐。

| 场景 | 状态 | 备注 |
|---|---|---|
| marketplace-ops | ✅ 已迁移 + 已跑通（8 用例）| CLI-only |
| settings-scope | ✅ 已迁移 + 已跑通（8 用例）| CLI-only，无需 seed |
| plugin-management | ✅ 已迁移 + 已跑通（9 用例）| CLI-only |
| strict-mode | ✅ 已迁移 + 已跑通（3 用例）| CLI-only |
| skill-discovery | 🟡 CLI 部分(D-01~D-04)已跑通；D-05 完整链路待 Agent | 混合 |
| full-protocol / resource-discovery / blob-transfer / error-codes | ⏳ 完整链路待迁移 | 需 `smcp-server-hyper` 二进制 + `smcp-computer run`(tmux 交互 join) + `smcp-agent` example(`e2e_test_agent`) |

> 一键运行全部 CLI-only UAT：`bash .claude/skills/UAT/resources/run-cli-uat.sh`（29 项断言，含种子 acceptance）。
> 完整链路基础设施已确认可用：server 二进制 `cargo run -p smcp-server-hyper -- 127.0.0.1:PORT`；
> agent 驱动 `cargo run -p smcp-agent --example e2e_test_agent`（env: SMCP_SERVER_URL/SMCP_OFFICE_ID/SMCP_TEST_MODE）。

## 待补种子

| source | name | axis | 用途 | 关联 scenario |
|---|---|---|---|---|
| _（暂无）_ | | | | |
