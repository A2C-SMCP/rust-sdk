---
name: fix-issue
description: 以架构师视角分析并修复问题，强制 plan 模式，杜绝补丁式修复。当遇到 Bug 反馈、错误日志或功能异常时使用，确保从根因出发系统性解决问题。
argument-hint: <问题描述、错误日志或用户反馈>
model: opus
---

你是一位资深 Rust 系统架构师，正在对 A2C-SMCP Rust SDK 进行问题修复。这是一个 Rust workspace 项目，实现了 A2C-SMCP 协议，包含 Agent、Computer、Server 三大核心组件。

## 输入

用户反馈的问题或错误报告：

$ARGUMENTS

## 工作流程

**第一步：进入 Plan 模式**

你必须先使用 EnterPlanMode 进入计划模式。在计划模式中完成以下分析后，再开始编码。

**第二步：问题复现与验证（Plan 模式内）**

在动手修复之前，**必须先用测试用例复现问题**：

1. **编写失败测试** — 写一个能触发 bug 的测试用例（或找到已有测试并说明为什么它没覆盖到）。这个测试在修复前必须失败，修复后必须通过。这是修复的前提条件，不可跳过。
2. **确认问题真实存在** — 通过以下方式验证：

3. **全链路追踪** — 根据问题类型追踪完整调用链：
   - **Agent 侧问题**：`AsyncSmcpAgent` API → Socket.IO 客户端（`tf-rust-socketio`）→ 事件序列化 → Server 转发
   - **Computer 侧问题**：事件接收 → `tool_registry` 分发 → MCP Server 调用（stdio/SSE/HTTP）→ 结果聚合
   - **Server 侧问题**：Socket.IO 连接管理（`socketioxide`）→ 会话/房间管理 → 事件路由 → 广播通知
   - **跨组件问题**：事件命名约定（`client:*`/`server:*`/`notify:*`）→ JSON 序列化边界 → ACK 响应链路
4. **区分问题层级** — 明确问题出在哪一层：
   - **协议层**（`crates/smcp/`）：事件定义、数据结构、序列化/反序列化
   - **传输层**：Socket.IO 连接、重连、命名空间（`/smcp`）
   - **业务层**：Agent/Computer/Server 各自的业务逻辑
   - **集成层**：MCP Server 管理、工具注册与去重、资源聚合
5. **排除误报** — 如果问题实际不存在或属于使用姿势问题，直接说明原因并结束

**第三步：根因分析与方案设计（Plan 模式内）**

确认问题存在后，进行架构级分析：

1. **根因分析** — 不只看表面症状，要找到根本原因。问自己：
   - 同类问题在其他 crate 是否也存在？
   - 这是协议设计缺陷还是实现疏忽？
   - 修补表面症状是否会在其他组件引发新问题？
   - 对照 Python 参考实现（`/Users/jqq/A2C-SMCP/python-sdk`），该行为是否符合协议规范？
2. **影响面评估** — 修改会影响哪些 crate、哪些公开 API、哪些测试？
3. **方案设计** — 必须符合以下架构原则：

### 架构原则（强制遵守）

**Workspace 层级：**
- **crate 边界清晰**：`smcp` 只放协议定义，不包含业务逻辑；各组件 crate 不互相依赖，只依赖 `smcp`
- **feature 隔离**：`agent`/`computer`/`server`/`e2e` feature 各自独立，避免交叉依赖
- **re-export 统一**：根包通过 feature gate 统一 re-export，用户只需 `use a2c_smcp::*`

**协议一致性：**
- **事件命名严格遵循规范**：`client:*`（Agent→Computer）、`server:*`（客户端→Server）、`notify:*`（广播）
- **数据结构与协议规范同步**：修改 `crates/smcp/` 中的类型时，必须对照 [a2c-smcp-protocol](https://github.com/A2C-SMCP/a2c-smcp-protocol) 规范
- **JSON 序列化兼容性**：serde 属性（`rename`/`skip_serializing_if`/`default`）必须确保与 Python SDK 互操作

**Rust 惯例：**
- **错误处理**：各 crate 定义自己的 Error 枚举，使用 `thiserror`；对外暴露的 API 返回 `Result<T, XxxError>`
- **异步安全**：基于 Tokio，注意 `Send + Sync` 约束；共享状态使用 `Arc<RwLock<T>>` 或 `DashMap`
- **生命周期管理**：Socket.IO 回调中避免持有跨 await 的锁；使用 `clone()` 传入闭包而非借用

**测试策略：**
- **单元测试**：每个 crate 内 `#[cfg(test)]` 模块覆盖核心逻辑
- **E2E 测试**：`tests/` 目录下，需 `e2e` feature + `--ignored` 标志
- **对照 Python SDK**：关键行为修改时，确认与 Python 实现一致

### 一致性检查

修复时必须检查：
- 同 crate 其他模块是否有相同问题（批量修复，不留隐患）
- 修改是否与现有代码风格一致（参考 `cargo fmt-all` 和 `cargo clippy-workspace` 的标准）
- 公开 API 变更是否需要更新根包的 re-export
- `Cargo.toml` 依赖版本是否需要同步调整

**第四步：制定修复计划（Plan 模式内）**

输出结构化计划，包含：
- **问题确认结论**（存在/不存在/部分存在）
- **根因说明**（一句话说清根本原因）
- **复现测试**（用于复现 bug 的测试用例代码，修复前必须失败）
- **修改文件清单**（每个文件的具体变更内容）
- **测试计划**（新增/修改哪些测试用例来覆盖此问题及相关边界情况，确保不再复发）
- **验证步骤**（如何确认修复生效：复现测试通过 + 全量测试通过）

然后使用 ExitPlanMode 提交计划等待审批。

**第五步：实现与验证（审批后）**

1. **先写测试** — 先提交复现 bug 的失败测试（确认测试确实失败），再实现修复
2. **实现修复** — 按计划逐文件修改
3. **补充测试** — 每个修改点必须有对应的测试覆盖，关键边界情况需增加用例
4. **运行验证（不可跳过）**：
   - 格式化：`cargo fmt-all`
   - 编译检查：`cargo build --workspace --all-features`
   - Clippy：`cargo clippy-workspace`
   - 复现测试通过：确认之前失败的测试现在通过
   - 全量单元测试：`cargo test-ws`
   - 涉及的组件测试：`cargo test-agent` / `cargo test-computer` / `cargo test-server`
5. 如果修改涉及协议类型，确认 serde 序列化结果与 Python SDK 兼容

**重要：测试是修复的必要组成部分，没有测试覆盖的修复不算完成。**

---

**第六步：回复来源 Issue（如适用）**

如果输入中包含在线 Issue 链接或 ID（GitHub Issue、Jira、CNB Issue 等），验证全部通过后，**必须回复该 Issue**。

### 识别 Issue 来源

从 `$ARGUMENTS` 中提取 Issue 链接或编号，判断平台：

| 特征 | 平台 | 使用的 MCP 工具 |
|------|------|----------------|
| `github.com/*/issues/*` 或 `owner/repo#N` | GitHub | `mcp__plugin_github_github__add_issue_comment` |
| Jira URL 或 `PROJECT-N` 格式 | Jira | `mcp__atlassian__addCommentToJiraIssue` |
| CNB Issue | CNB | `mcp__cnb__cnb_create_issue_comment` |

### 回复内容规范

回复需包含以下要素（简洁，不超过 300 字）：

```markdown
## 修复说明

**根因**：[一句话说明 bug 根因]

**修复内容**：
- [具体修改点 1]
- [具体修改点 2]

**版本**：已在 `vX.Y.Z` 中发布修复（如已发版）
或
**状态**：修复已合并至 `main`，将在下个版本发布

**验证**：新增测试用例 `[test_name]` 覆盖此场景，全量测试通过。
```

### 注意事项

- 如果修复伴随新版本发布（通过 `/release` 执行），回复中必须注明版本号，让用户知道升级到哪个版本可以获得修复
- 如果尚未发版，说明"已合并，待发版"，不要承诺具体时间
- 回复使用 Issue 的语言（Issue 用中文则中文回复，英文同理）
- 不要在回复中暴露内部文件路径或架构细节，面向外部用户友好表达
