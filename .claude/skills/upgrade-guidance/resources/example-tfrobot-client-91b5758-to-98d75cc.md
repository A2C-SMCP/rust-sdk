# Golden 示例 — `upgrade-guidance` 的第一次真实运行

> **这是 [`../SKILL.md`](../SKILL.md) 的参考产物，不是一份需要维护的文档。**
>
> - 生成于 2026-07-17，区间 `91b5758..98d75cc`（7 提交）
> - 收件方：tfrobot-client `feature/split_runtime_config`
> - 实际交付：[tfrobot-client#32](https://github.com/A2C-SMCP/tfrobot-client/issues/32)
> - 净结论：1 处编译期破坏 + 2 处静默行为变更 + 3 处顺带发现的既存缺陷
>
> 写新报告时照本文的**结构**和**颗粒度**抄：按严重度排序、每条给「旧→新 / 首次受影响 commit / 触发条件 / before-after / 是否需迁移 / 建议动作」、"无影响"也要给理由、既存缺陷单独成节。

---

> 来自 **rust-sdk** 维护者。本报告依据你们维护的《tfrobot-client 使用的 Rust SDK 接口清单》（基线 2026-07-17）逐条核对生成，**所有结论均已对 SDK 源码逐行验证**，不依据 commit message 推断。

## 概览

| 项 | 值 |
| --- | --- |
| **你们的基线** | `91b5758bf4162c0a02308e8426114ddbda2bb003` |
| **目标** | `98d75cc883a23a1e5494bbd6aaf3b4218e0470dc`（`develop` HEAD @ 2026-07-17） |
| **区间** | 7 个提交，其中 3 个带 `!` 破坏性标记 |
| **workspace 版本** | `0.3.0-dev.0` → `0.3.0-dev.0`（**未变**） |
| **净结论** | **1 处编译期破坏（在测试里）+ 2 处条件触发的静默行为变更**；生产代码零改动可升级 |

### ⚠️ 收件分支说明

清单声称的基线 `91b5758` 在你们仓库里**只存在于 `feature/split_runtime_config`**（`b0be38e6`，2026-07-16）。其余分支的 SDK 依赖是：

| 分支 | 依赖 |
| --- | --- |
| `main` | `smcp-computer = "0.1.14"`（crates.io） |
| `develop` | `smcp-computer = "0.2.3"`（crates.io） |
| `feature/TFRC-39-sdk-computer-skills-marketplace` | `a2c-smcp` git rev `f63796a`（= rust-sdk #91，早于 `91b5758`） |
| **`feature/split_runtime_config`** | **`a2c-smcp` git rev `91b5758`** ← **本报告的适用对象** |

**本报告只对 `feature/split_runtime_config` 有效。** 若 main/develop 也计划升级，那是另一件事（要先从 crates.io `smcp-computer` 独立 crate 迁到 `a2c-smcp` umbrella + features，跨度大得多），需要单独评估。

### ⚠️ 版本号在这条链路上是假信号

两端 workspace 版本都是 `0.3.0-dev.0`，**期间发生了 3 次破坏性变更但版本号一动没动**（0.3.0 尚未发布，develop 持续演进）。请勿用版本号判断兼容性，**只认 commit hash**。

---

## 🔴 P0：必须处理（1 处，编译期）

### 1. `ComputerError` 两个变体字段改名 —— 你们的 `contract_test.rs` 会编译失败

- **首次受影响 commit**：`937f0bb`（#132 卫生批次）
- **注意**：该提交 **未标 `!`**，commit message 自述「纯改名/零行为变化」——但它含一处真编译期破坏。

| | 旧 | 新 |
| --- | --- | --- |
| `ComputerError::ServerNotActive` | `{ server_name: String }` | `{ bundle_id: String }` |
| `ComputerError::McpCapabilityNotSupported` | `{ server_name, capability }` | `{ bundle_id, capability }` |

**你们的命中点**（我已替你们 grep 过全仓，只此一处）：

`src-tauri/tests/contract_test.rs:304`
```rust
let e3 = ComputerError::ServerNotActive {
    server_name: "srv".into(),   // ← 升级后编译失败
};
```

**修法**：改一个字段名即可，**值语义完全不变**（原来这个字段名叫 `server_name`、装的却是 bundle_id，属于 #119 的「撒谎命名」残留，本次统一订正）。

```rust
let e3 = ComputerError::ServerNotActive {
    bundle_id: "srv".into(),
};
```

**其余全部安全**（已验证）：变体集合与 arity 未变 ⇒ 穷尽 `match` 不受影响；`Display` 输出逐字节相同（插值的值是同一个）⇒ `to_string()` 断言安全；`error_code()` 用 `{ .. }` 匹配 ⇒ 不变；`ComputerError` 无 serde ⇒ 零 wire 影响。

> 顺带一提：你们这个 contract_test 的注释写着「If smcp-computer removes/renames these, this test will fail at compile time」——它**正在按设计工作**。这次它是唯一拦住这个破坏的东西。

- **需要数据迁移**：否

---

## 🟡 P1：需评估（2 处，静默运行期变更）

### 2. `default_tool_meta.alias` 不再被继承 —— 工具名会变、被吞的工具会回来

- **首次受影响 commit**：`bb0af80`（#134，标了 `!`）
- **命中你们清单**：§7（工具名）、§4.2（`ComputerStatusSnapshot.tools` 计数）
- **编译期**：不破。`default_tool_meta()` 方法、`ToolMeta` 四字段、serde 形态全部未动 ⇒ 你们的配置编辑/回显 UI、`contract_test` 的 ToolMeta 断言全部继续通过。

**改了什么**：SDK 此前把 `default_tool_meta.alias` 回落到该 server 的**每一个**工具，导致所有工具的 exposed 名塌成同一个 `{bundle_id}__{alias}`，然后 first-wins **静默丢弃**其余工具。修复后 **alias 只取自具体的 `tool_meta[<工具名>]`，绝不从 default 继承**。

**触发条件**：某 server 的 `default_tool_meta.alias` 非空，且其工具没有各自的 `tool_meta[T].alias`。
> ⚠️ 易漏的第二条件：即使你为某工具写了 `tool_meta[T]`（比如只设了 `auto_apply`），只要该条目的 `alias` 是 `None`，旧码仍会让 default alias 漏进来。**「配了 per-tool meta」≠「不中招」。**

**前后对比**——多工具 server `test`（暴露 `alpha`/`beta`/`gamma`），`default_tool_meta.alias = "123"`：

| | exposed 名 | 工具数 |
| --- | --- | --- |
| 升级前（`91b5758`） | `test__123`（只有 alpha 幸存） | **1** |
| 升级后（`98d75cc`） | `test__alpha`、`test__beta`、`test__gamma` | **3** |

单工具 server 则是纯改名：`test__def_alias` → `test__t`。**这是本次唯一「原本可用的功能被拿掉」的场景**——单工具时 default alias 此前是个正常工作的改名手段。

**对你们的具体影响**：

1. **`__` 分割逻辑不用改** —— 反而比升级前更贴合契约（升级前在 default-alias server 上分割得到的是 alias，不是 raw name）。
2. **UI 上用户会看到工具名变化 + 工具数量增加**，`ComputerStatusSnapshot.tools` 计数会跳变（例：1 → 3）。建议给用户一句 release note。
3. **`MCPServerConfig.tool_meta` 里的 auto_apply/tags 安全** ✅ —— 该 map 的键是**原始工具名**，与 exposed 名解耦。
4. **若你们按 exposed 名持久化过任何状态**（收藏 / 置顶 / per-tool UI 状态 / 历史记录），旧的 `test__123` 会变成悬空引用。**这条我无法替你们判断，需自查。**
5. **新增的诊断 WARN 你们拿不到** ❌ —— 它走 `tracing::warn!`，**不进** `GovernanceDiagnostic`、不进 `ComputerStatusSnapshot`、不发 `ComputerEvent`。若未装 tracing subscriber 则完全无感。**这意味着单工具改名场景对用户是彻底静默的。** 若你们需要结构化观测，请提 issue，我们可以把它提升为 `GovernanceDiagnostic`。

#### 🔗 与你们的 #31「填写"别名前缀"后 MCP 启动失败」的关系（重要，定性需修正）

我读了 #31。**它报告的「启动失败」硬报错，在你们当前基线 `91b5758` 上其实已经消失了** —— 被 #117 BundleID 模型的分域消掉了（你们 `mcp_integration_test.rs:710` 的 `test_default_tool_meta_alias_is_scoped_by_bundle_id` 断言启动成功，就是证据）。基线上**残留的是静默丢工具**，而这正是 #134 修的。

关于 #31 里等的那个「真正的 prefix 能力（`123__first-tool`）」：**rust-sdk#101 已 closed / not_planned**，理由是该需求被重写为「统一 server-level tool namespace + 稳定 server_id + exposed-name 路由」，由 #116 承接 → 最终落地为 **#117（BundleID 模型）+ #134**。

**换句话说：你们要的 prefix 语义已经有了，它就是 `{bundle_id}__{tool}`，而且是自动的、不需要用户填任何东西。** 所以建议：

- **UI 层直接移除 default 位的「别名前缀」输入框**（SDK 已判定 `default_tool_meta.alias` 无合理用例；保留它只会让用户配了一个静默 no-op）。
- 如需给单个工具改名，改用 per-tool `tool_meta.<工具名>.alias`。
- **升级后 #31 应该可以关闭。**

#### ⚠️ 一个静默死角，而你们正好踩在上面

`default_tool_meta.alias = Some("")`（空串）时，SDK 会把 alias 抹成 `None`（行为变了：旧码 exposed 名是 `test__`），**但 WARN 的守卫是 `!a.is_empty()`，不会放行 ⇒ 这种配置行为变更且无任何诊断**。

你们 `mcp_integration_test.rs:742` 的 `test_blank_default_tool_meta_alias_does_not_collide_on_first_start` **正在构造 `alias: Some("".to_string())`** ⇒ 说明你们的 UI/序列化确实会产出 `Some("")` 而非 `None`。清单 §6.2 说的「全字段 None 归一化为 None」不处理「alias 是空串」这一情况。**建议确认这条归一化路径。**

#### 你们那两个 alias 测试会怎样

**都继续绿** —— 但它们绿的理由变了，而且**都不断言工具数量**，所以捕捉不到本次真正的行为变化（工具 1 → 3）。若想守住这个契约，建议补一条工具数断言。

- **需要数据迁移**：视第 4 点自查结果而定

### 3. `TRUSTED_SCOPE_ONLY_FIELDS` —— `validate_config` / `migrate_config` / `import_config` 条件性行为变更

- **首次受影响 commit**：`cb4d87c`（#143，标了 `!`）
- **命中你们清单**：§11.1、§11.2

**改了什么**：`enabledMcpjsonServers` / `enableAllProjectMcpServers` 两个字段现在**在 Project scope 下被拒绝**（安全修复：堵住「clone 来的仓库自带 project settings.json 自我批准 MCP」这条授权绕过）。`User`/`Local`/`Flag`/`Policy` 全部放行。

**先说不受影响的**（这是我最初的头号怀疑，已证伪）：你们 §10.5 → §10.1 的 `resolve_settings(...).settings` → `reconcile_governance(hooks, declared)` **完全不受影响** —— `ResolvedSettings` 结构与 `resolve_settings` 签名一个字节没动（`errors` 字段在你们基线就已存在）；`reconcile_governance` 只读 `enabledPlugins`/`autoUpdate`，从不读被过滤的那两个字段。commit message 里说的「`resolved_settings` 吞 errors」指的是 **CLI 内部的同名 helper**，而你们没启 `cli` feature，那段代码**根本不编译**。

同理：`InstallOptions`/`EnableOptions`/`DisableOptions` 固定传 `scope: "user"` ⇒ 三条路径全部放行，不受影响。审批门是 CLI-only 的东西，你们够不着。

**真正受影响的**——当且仅当某个 project 层 `settings.json`（或你们手工构造的 `ProjectConfigDoc.settings`，**注意不是 `settings_local`**）含那两个字段时：

| API | 变化 |
| --- | --- |
| `validate_config(&doc)` | `ValidationReport.errors` 新增条目 ⇒ `is_valid()` 由 `true` 翻 `false` |
| `migrate_config(dir)` | 返回值由 `false` 翻 `true`，并把这两个字段**从磁盘上的 project `settings.json` 里静默删除** ← **本次唯一会改用户文件的路径** |
| `import_config(dir, doc)` | 返回的 `ValidationReport` 新增错误条目；落盘照常（非阻断语义未变） |

**建议动作**：

1. 在你们仓库 grep `enabledMcpjsonServers` / `enableAllProjectMcpServers`，确认没有 fixture / 默认模板 / 集成测试 / benchmark 往 `ProjectConfigDoc.settings` 里塞这两个字段并断言 `is_valid() == true`（清单 §13.3 提到测试与 benchmark 直接构造 `ProjectConfigDoc`）。这是唯一可能的测试回归。
2. 若有「导入/迁移配置」的 UI 流程：`migrate_config` 现在会静默删字段。若想向用户解释「为什么我的字段没了」，可在调用前先跑 `validate_config` 预览将被清理的条目，把 `SettingsValidationError.reason` 展示出来（SDK 的错误文案已带可操作迁移指引：「move it to settings.local.json (not git-tracked) or the user scope」）。**这是可选体验增强，非必须。**
3. **无需任何 API 适配** —— 无签名变更、无新枚举变体、无字段增删。`TRUSTED_SCOPE_ONLY_FIELDS` 是新增 pub const（纯 additive）。

- **需要数据迁移**：否（`migrate_config` 的删除行为本身就是期望语义）

---

## ✅ 已核实：对你们无影响

### 4. `SMCPTool` 新增必填 wire 字段 `bundle_id`（`98d75cc` #136，标了 `!`）

**对你们生产代码零影响。** 理由：`Computer::get_available_tools()` 返回的是 **rmcp 原生 `Tool`**，与 `SMCPTool`（协议 wire 类型）是**两个不同类型**，无继承/别名关系。本次未触碰 rmcp `Tool`，其 `name`/`description`/`input_schema`/`meta` 逐字节不变。agent 侧新增的 `get_computer_config` 在 `features = ["computer"]` 下根本不编译。

我已 grep 过你们全仓：**`SMCPTool` 零命中** ✅（该类型经 `pub use smcp::*` 对你们可见且无 `#[non_exhaustive]`，理论上字面量构造会因缺字段编译失败——但你们没用它）。

**`smcp-server-core = "0.2.3"` dev-dep：保持不动，本次无需同步升级。** 已验证：两个版本的 server-core **都从不反序列化 `SMCPTool`** —— 它对 `get_tools` ack 是纯 `serde_json::Value` 透传，对 `update_tool_list` 只读 `computer` 字段。版本歪斜对 #136 完全惰性，§13.1 集成测试不会因此挂掉。（这回答了你们清单 §13.2 提出的问题。）

**⚠️ 但有一条升级排序提醒**：破坏方向与你们相反。升级后你们发出的 ack 多一个 `bundle_id` 键，老 Agent 因 serde 默认忽略未知字段照常解析（**你们升级是安全的**）。真正会炸的是「**新 Agent（要求 `bundle_id`）↔ 老 Computer（不发 `bundle_id`）**」。⇒ **若对端 Agent 先升到 0.3.0，你们必须一并升级**，否则新 Agent 无法解析你们的 `get_tools` 应答。**建议 client 不晚于 Agent 升级。**

### 5. `3b88308`（#133 契约注释订正）、`280801b`（#146 rustdoc 清零）、`8a6acf7`（#143 测试跟进）

**零影响**，已机械核验：

- `3b88308`：剔除注释行后 diff 增删集合**为空**。`AUTH_MCP_SERVER_KEY` 常量值仍是 `"mcp_server"` 未变。且改动只落在 `smcp` / `smcp-agent`，后者你们不编译。
- `280801b`：全 workspace 唯一的非注释改动是**一个 lint 属性** `#[allow(rustdoc::invalid_html_tags)]`。其余是 CI / `.cargo/config.toml` / doc。
- `8a6acf7`：全部落在 `cli` feature 或 `mod tests` 内。

---

## 🎁 顺带发现的既存缺陷（**非本次引入**，但你们大概想知道）

这三条都在你们基线 `91b5758` 上就已存在，升级不会让它们变好或变坏。我在核对清单时发现的，白送。

### A. 🔴 `Tool.meta.server_name` 是一处死读取 —— 恒返回 `None`

你们清单 §7 写「Client 从 `Tool.meta.server_name` 读取工具所属 Server」。**SDK 从不写这个 key。** 往 `Tool.meta` 里写的 key **有且只有** `a2c_tool_meta` 和 `a2c_vrl_transformed`（`crates/smcp-computer/src/mcp_clients/model.rs:24-25`）。我在整个 `smcp-computer` + `smcp` 源码里 grep `"server_name"` 字符串字面量：**零命中**，基线上亦然。

**你们的现场**：`src-tauri/src/commands/debug.rs:450`
```rust
fn tool_server(tool: &Tool, running_servers: &[String]) -> String {
    tool.meta.as_ref()
        .and_then(|m| m.get("server_name"))        // ← 恒 None
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| (running_servers.len() == 1).then(|| running_servers[0].clone()))  // ← 实际靠这个兜底
        .unwrap_or_else(|| "unknown".to_string())  // ← 多 server 时必走这里
}
```

⇒ **单 server 时靠兜底碰巧正确；多 server 时工具归属一律 `"unknown"`。** 你们那个测试 `test_debug_get_available_tools_keeps_unknown_server_for_multiple_running_servers` 的名字已经把这个症状固化成「预期行为」了。

**正确修法**：exposed 名就是 `{bundle_id}__{raw_tool_name}`（你们 §7 已正确记录 `__` 分割约定）。切前缀拿到 bundle_id，再用 `list_mcp_servers_with_metadata()` 的 `bundle_id` → `name` 映射成展示名即可。

### B. 🟡 `get_resources` 是 bundle_id-only —— server 名含空格或中文时**必吃 4014**

你们清单 §7 把三个**寻址语义不同**的 API 都记作 `server_name`，但它们的真实语义并不一致：

| API | 实际接受/返回 |
| --- | --- |
| `get_resources(mcp_server, …)` | **只认 bundle_id，无 name 回退** ⇒ 传 display 名必 4014 |
| `list_all_windows(…)` → `(String, Resource)` | `.0` 是 **display 名** |
| `get_window_detail(server_name, …)` | **name 优先、bundle_id 回退**（两者都吃） |

好消息：`list_all_windows` → `get_window_detail` 的往返是自洽的。

**风险点**：`src-tauri/src/services/computer.rs:1055` 的 `.get_resources(server_name, cursor)` 把 Tauri command 传来的 `server_name` 直接透传。

**为什么现在没炸**：`bundle_id = normalize_name(name)`，而 `normalize_name` 只把非 `[A-Za-z0-9_-]` 码点映射成 `_`。所以 `debug-tools` 这类名字规范化后**恒等**，display 名 == bundle_id，碰巧能用。

**什么时候会炸**：server 名一旦含**空格、中文或任何非 ASCII 字符** —— 例如用户起名「我的服务器」或「My Server」—— `bundle_id` 就变成 `___` / `My_Server`，**此时传 display 名必 4014**。考虑到你们的用户群体，中文 server 名几乎是必然。

**建议**：该调用点改为从 §6.3 的 `list_mcp_servers_with_metadata()` 取 `bundle_id` 再传入。

### C. 🟡 清单 §7 的 `ToolCallRecord` 字段列表与 SDK 类型对不上

清单写 `ToolCallRecord { timestamp, req_id, server, tool, parameters, timeout, success, error }`，SDK 实际是（`crates/smcp-computer/src/desktop/model.rs:22-35`）：

```rust
ToolCallRecord { bundle_id, server, tool, timestamp, metadata }
```

既多了 `bundle_id`/`metadata`，也不含 `req_id`/`parameters`/`timeout`/`success`/`error`。**清单描述的可能是你们自己的 DTO 而非 SDK 类型**，建议订正清单（不影响本次升级）。

---

## 📅 前瞻预警：下一个会打到你们的破坏性变更

**`ProvenanceScope` 即将新增变体，而你们的 match 没有 fallback。**

你们清单 §11.3 明确写着「Client 完整匹配 `ProvenanceScope`：`User`/`Project`/`Local`/`Flag`/`Policy`/`Intent`。**该 match 当前没有 fallback，新增变体会直接导致编译失败**」。

- **现状**：本区间**未动** `ProvenanceScope`（已验证），升级到 `98d75cc` 安全。
- **在途**：[rust-sdk#147](https://github.com/A2C-SMCP/rust-sdk/issues/147)（OPEN，`breaking-change`、`P1:high`）将落地协议裁决的 **`embed` origin**，层位钉死为 `plugin < user < project < local < embed < flag < policy`。**这大概率会给 `ProvenanceScope` 加一个变体 ⇒ 你们的穷尽 match 会编译失败。**
- **建议**：现在就给那个 match 加一条 `_ => "unknown"` 兜底（你们对 `MarketplaceStatus`/`PluginStatus` 的 `non_exhaustive` 已经是这么做的），把这次未来的硬中断降级成软退化。#147 落地时我们会再发一份报告。

---

## 附：本报告的验证方法

- 区间：`git log --oneline 91b5758..98d75cc`（7 提交）
- 每个提交逐 diff 核对，**剔除注释行后**判断是否触及 pub 符号的签名 / 字段 / serde 形态 / 运行期行为
- 所有「命中你们清单」的结论均对 SDK 源码定位到 `file:line`
- 所有「需你们自查」的点，我已浅克隆 `feature/split_runtime_config` 替你们 grep 并给出具体行号
- 实测 `cargo check -p a2c-smcp --no-default-features --features computer` → EXIT=0

**升级后建议在你们侧跑**（对齐清单 §15）：
```bash
cargo check --manifest-path src-tauri/Cargo.toml --all-targets
cargo test --manifest-path src-tauri/Cargo.toml --test contract_test
cargo test --manifest-path src-tauri/Cargo.toml --test mcp_integration_test
```

有疑问直接在本 issue 下回复，或到 rust-sdk 开 issue。
