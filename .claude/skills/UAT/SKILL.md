---
name: uat
description: 执行 A2C-SMCP Rust SDK 的用户验收测试（UAT）。通过 tmux MCP 工具（或直接 Bash）在真实终端环境中验证 smcp-computer CLI 命令和端到端协议流程。
---

# UAT - User Acceptance Testing Skill（Rust SDK）

## Description

用户验收测试（UAT）技能。以真实用户视角对 A2C-SMCP Rust SDK 的 `smcp-computer` CLI
命令和协议流程进行端到端验收测试。CLI-only 场景可直接用 Bash 执行；完整链路场景用
tmux 终端自动化协调多进程。

> 本框架移植自 python-sdk 的 UAT 体系，保留其方法论（seed 库 / 诊断三问 / 报告格式），
> 命令层适配为 Rust CLI（`cargo run -p smcp-computer --features cli` / `target/debug/smcp-computer`）。

## Instructions

### 角色定位

你是一名 QA 测试工程师，负责对 A2C-SMCP Rust SDK 执行用户验收测试。验证 CLI 命令和
协议交互是否符合预期。

### 前置条件检查

执行任何测试前，**必须**确认：

1. **CLI 已编译**：`cargo build -p smcp-computer --features cli` 成功，`target/debug/smcp-computer` 存在
2. **（完整链路场景）tmux MCP 可用**：能调用 `mcp__tmux__*` 系列工具
3. **（marketplace 类场景）git 可用**：用于构造本地 bare 仓库 fixture

任一条件未满足，**停止测试**并提示用户先完成准备。

### CLI 调用约定

```bash
# 推荐：预编译一次，多用例复用二进制（避免每条命令重复 cargo 检查开销）
cargo build -p smcp-computer --features cli
A2C="$(pwd)/target/debug/smcp-computer"

# 对齐 python-sdk 习惯的等价写法：
# A2C="cargo run -q -p smcp-computer --features cli --"
```

### ⚠️ 环境隔离（所有场景必须遵守）

UAT 必须把**两类**持久化都重定向到临时目录，否则会污染用户真实配置：

```bash
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home"   # skill 包 / marketplace clone
export XDG_CONFIG_HOME="$U/config"      # settings.json（必须绝对路径！）
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
```

| 持久化对象 | 落盘位置 | 隔离 env |
|---|---|---|
| marketplace clone / skill 包 | `$A2C_SKILL_HOME/marketplace/...` | `A2C_SKILL_HOME` |
| settings.json（trust 决策 / enabledPlugins / scope 设置） | `$XDG_CONFIG_HOME/a2c/settings.json`（回退 `~/.config/a2c/`） | `XDG_CONFIG_HOME`（绝对路径） |

> **教训**：`marketplace add --trust`、`settings set`、`plugin install` 都会写
> settings.json。只设 `A2C_SKILL_HOME` 不够——trust 列表/enabledPlugins 会落到
> `~/.config/a2c/settings.json`。务必同时隔离 `XDG_CONFIG_HOME`。
> 见 `crates/smcp-computer/src/settings/scope.rs::resolve_user_config_dir`。

| python-sdk | rust-sdk |
|---|---|
| `uv run a2c-computer <cmd>` | `$A2C <cmd>` |
| `A2C_SKILL_HOME=...` | `A2C_SKILL_HOME=...`（两端通用） |
| `a2c-computer run --url ...` | `$A2C run --url ...` |

### 执行协议

1. **加载场景**：读取 `resources/scenarios/<scenario>.md`
2. **种子前置 audit**：识别场景引用的 `resources/seeds/<source>/<name>`；逐条跑
   acceptance（`bash <seed>/acceptance.sh` 或 `acceptance.md` 内脚本）。任一 ❌ →
   进入 [Seed 升级流程](#seed-升级流程)；任一缺失 → [Seed 缺口流程](#seed-缺口流程)
3. **准备环境**：按场景类型创建 tmux session / 启动进程，或直接 Bash
4. **逐用例执行**：发送命令 → 捕获输出 → 对比预期 → 标记 PASS / FAIL。FAIL 时先走
   [诊断三问](#诊断三问)，判定 SUT 病还是 seed 病
5. **收集日志**：捕获完整输出
6. **输出报告**：汇总结果 + 「Seed 反馈」节

### 一键运行

#### CLI-only 套件

已迁移并跑通的 5 个场景（CLI 部分）+ 全部种子 acceptance 可一键执行：

```bash
bash .claude/skills/UAT/resources/run-cli-uat.sh
# 自动编译 → 跑 marketplace-ops / settings-scope / plugin-management / strict-mode /
# skill-discovery(CLI) + 6 个种子 acceptance，输出 PASS/FAIL 汇总（当前 29/29 ✅）。
```

#### 完整链路套件（真三进程）

两种等价编排，覆盖 full-protocol + resource-discovery + blob-transfer + skill-discovery(D-05) +
error-codes（当前 agent 10 modes + F-05 全绿）：

```bash
# ① 正式 tmux 流程（真实 tmux 三 window，可 attach 观察）—— 对齐本 skill「完整链路用 tmux」约定
bash .claude/skills/UAT/resources/full-protocol-uat-tmux.sh
#   server/computer/agent 各占一个 tmux window；失败保留 session（KEEP_ON_FAIL=1 默认）供
#   `tmux attach -t a2c-uat-fp` 调试。端口/就绪标记一律读 tee 日志文件（pane 80 列会折行误判）。

# ② 轻量 bash 流程（后台 & + FIFO 驱动 REPL，无需 tmux，CI 友好）
bash .claude/skills/UAT/resources/full-protocol-uat.sh
```

> 两者进程模型相同（真实 Server + Computer + Agent 三 OS 进程 + Computer fork 的 node MCP 子进程，
> 真实 socket.io over loopback）。tmux 版用于人工可观测/对齐文档，bash 版用于快速/CI 一键判定。
> **依赖 #82（ack 拆封）+ #83（run 补 boot_up）修复**——否则 skill/blob/error 场景必失败。

### CLI-only 场景 vs 完整链路场景

#### CLI-only 场景（如 marketplace-ops）

直接 Bash 执行子命令即可，无需 tmux：

```bash
A2C_SKILL_HOME=$HOME_DIR $A2C marketplace add file://$BARE --trust --json
```

如需在 tmux 内运行：单 session `a2c-uat`，单 window，`execute-command` 发命令 +
`capture-pane` 取输出。

#### 完整链路场景（如 full-protocol）

需要 Server / Computer / Agent 多进程，用 tmux 多 window 协调。详见
`resources/test-env-setup.md`。

> Rust 端进程：Server = `smcp-server-hyper` 二进制；Computer = `$A2C run --url ...`；
> Agent = `smcp-agent` 客户端（驱动脚本可用 Rust 写，或跨语言复用 python-sdk 的
> Agent 驱动脚本连接 Rust 进程——协议同构）。

### 测试原则

- **真实进程视角**：启动真实二进制，不 mock
- **可观测性**：每步捕获输出；失败时全量收集
- **幂等性**：测试不留残留（marketplace remove 清理、tmp 目录隔离）
- **独立性 / 容错性**：每场景独立；单用例失败不阻塞后续
- **种子可信**：场景引用的 seed 必须当次 audit PASS 才作前置

### Seed 依赖与诊断

> Scenario 中所有 fixture（marketplace 仓库、SKILL 包、MCP Server 等）**只能**通过
> `resources/seeds/<source>/<name>` 路径引用——不在 scenario 文档里 inline 长 fixture。

#### 诊断三问（FAIL 时逐条回答）

1. **是 SUT bug 还是 seed bug？**
   - 判据：独立跑 `bash <seed>/acceptance.sh` 能否复现 FAIL？能 → seed 病；不能 → SUT 病
   - SUT 病 → 修 Rust 代码（不动 seed）；seed 病 → 走 seed upgrade
2. **协议依据是否还在？**
   - 在 a2c-smcp-protocol 规范找 seed 的 axis 对应条款；找不到 → seed 不应存在或协议有变更未同步
3. **seed 期望与 scenario 期望一致吗？**
   - 不一致 → 改 scenario 期望（向 seed 看齐）或改 seed（向 scenario 看齐）；两边都不能动 → 写新 seed

#### Seed 升级流程

```
1. bash <seed>/acceptance.sh              # 独立复现
2. 决定 upgrade 维度：资产本体 / acceptance 断言 / axis 定义
3. bash <seed>/acceptance.sh              # 验收
4. 若 seed 派生自 _common/<x> → 跑全部派生方 audit
5. 回到 scenario 重跑相关用例
6. 在报告"Seed 反馈"节登记
```

#### Seed 缺口流程

```
1. 当前报告暂列该用例为 ⏭️ Skipped + 注明"待 seed: ..."
2. 完成本场景其他用例
3. 场景结束统一汇报缺口（缺口名 / 触发用例 / 期望行为）
4. 由用户决策：立刻补 seed 回测，或留待下个周期
```

**反模式（禁止）**：scenario 临时 inline fixture；改 seed 勉强匹配错的期望；跳过 FAIL 的 seed。

### 二次复验机制

对所有 **FAIL** 结果二次复验：等待 2-3 秒重跑 → 增加输出行数复查 → 两次一致采信，
不一致标记 **Flaky**。

### 报告格式

```
## UAT 报告 - [场景名称]
日期：YYYY-MM-DD
分支：[当前分支]

### 测试结果摘要
- 总用例数 / 通过 ✅ / 失败 ❌ / 跳过 ⏭️

### 用例详情
| # | 用例 | 优先级 | 引用 seed | 结果 | 复验 | 备注 |

### 失败用例详情
#### [用例名称]
- 步骤 / 预期 / 实际 / 诊断（SUT bug / Seed bug / Scenario 期望不准）
- 引用 seed + 当次 audit 状态
- 输出粘贴

### Seed 反馈
| 类型 | seed | axis | 触发用例 | 动作 | 状态 |
```

## User-invocable

- name: uat
- description: 执行 A2C-SMCP Rust SDK 用户验收测试（UAT）。指定场景名执行单场景；不指定则全量。

## Arguments

- scenario: （可选）测试场景名称（如 marketplace-ops）。不填则全量执行。

## Prompt

请执行 UAT 测试。

首先确认前置条件：
1. `cargo build -p smcp-computer --features cli` 是否成功、`target/debug/smcp-computer` 是否存在？
2. （完整链路场景）tmux MCP 工具是否可用？

**判断执行模式：**

### 单场景模式（`$scenario` 已指定）

加载 `resources/scenarios/$scenario.md` 并执行。

### 全量测试模式（未指定 `$scenario`）

扫描 `resources/scenarios/`，按顺序执行：CLI-only 场景优先（marketplace-ops →
settings-scope → plugin-management → strict-mode），完整链路场景随后（full-protocol
→ resource-discovery → blob-transfer → skill-discovery → error-codes）。

每个场景：环境准备 → 执行全部用例 → 输出报告 → 清理（kill session / 删 tmp）→
`/compact` 压缩上下文 → 全通过则继续，存在失败则中止并汇总进度。
