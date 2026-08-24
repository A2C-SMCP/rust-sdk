# Issue #196 实验报告

## 实验目标

- 假设：真实 MCP `notifications/tools/list_changed` 会刷新 `Computer` 的工具投影，但不会推进
  `capability_revision`，也不会经 `Computer::subscribe_events()` 发布
  `CapabilityRevisionBumped`。
- 成功标准：真实 stdio 通知链路下能观察到工具投影变化，同时 revision 和事件保持不变；重复相同投影作为
  阴性对照，不应产生 revision 或事件。

## 项目与实验边界

- 被测版本：`develop@07d7cc04dfce51ec9c0951ee94e3556f4c99b585`。
- 被测链路：真实 Node.js stdio MCP 子进程 → rmcp 通知处理器 → Computer change channel →
  `McpChangeReactor` → `MCPServerManager::refresh_tool_routes` → runtime status/event。
- 环境：macOS，Node.js `v23.5.0`，Rust `1.94.0`，Cargo `1.94.0`。
- 每轮使用独立临时 skill/config/cache/XDG 根；未修改业务代码、正式测试、锁文件或用户已有改动。
- 重复次数：3；每轮执行新增、同名 schema 变化、相同投影、移除四种转换。

## 执行记录

| 命令 | 样本 | 退出码 | 备注 |
|---|---:|---:|---|
| `cargo fmt --manifest-path experiments/codex-issue-196/Cargo.toml -- --check` | 实验源码 | 0 | 格式检查通过 |
| `cargo run --manifest-path experiments/codex-issue-196/Cargo.toml` | 3 轮 × 4 转换 | 0 | 真实启动 3 个可变 stdio MCP 子进程 |

构建仅出现仓库业务代码现存的 `manager.rs:1508 unused_mut` 警告，与本实验无关。

## 数据结果

| 转换 | 样本 | 工具投影 | capability revision | CapabilityRevisionBumped |
|---|---:|---|---|---|
| 新增工具：phase 0 → 1 | 3/3 | 工具数 `1 → 2`，出现 `dyn_tool{x}` | 全部 `2 → 2` | 0/3 |
| schema 变化：phase 1 → 2 | 3/3 | 工具数仍为 2，`dyn_tool{x}` 变为 `{y}` | 全部 `2 → 2` | 0/3 |
| 相同投影：phase 2 → 2 | 3/3 | 工具数及 schema 均不变 | 全部 `2 → 2` | 0/3 |
| 移除工具：phase 2 → 3 | 3/3 | 工具数 `2 → 1`，`dyn_tool` 消失 | 全部 `2 → 2` | 0/3 |

- 12/12 转换都在 5 秒上限内达到预期工具投影。
- 新增投影等待为 27–28 ms；schema 变化为 0–2 ms；移除为 27–28 ms。
- 每个转换的日志都出现真实 `ToolListChangedNotification`、stdio handler 转发和
  `Tool routes refreshed successfully`，排除“通知没有送达”或“路由未刷新”。
- 9/9 个真实能力变化（新增、schema 变化、移除）均未推进 revision、未产生事件。
- 3/3 个相同投影阴性对照均未产生事件，这一行为本身符合 Issue 的去误报要求。

原始逐轮状态、完整工具 JSON 投影和事件数组见 `results.json`。

## 数据结论

- **事实**：Issue #196 在指定 commit 上稳定属实，复现率 3/3；缺口不仅影响工具数量变化，也影响工具数量不变的
  schema 变化。
- **根因推断（高置信）**：`McpChangeReactor::on_tool_list_changed()` 只刷新 manager 路由并广播 Office 工具更新；
  reactor 未持有 `RuntimeStatus`，没有本地 revision/event 出口。同时 `refresh_tool_routes()` 返回 `Result<()>`，
  没有把“提交后的 Agent-facing 工具投影是否真实变化”反馈给调用方。
- **限制**：实验覆盖真实 stdio transport。三种 transport 共用 reactor 消费路径，因此根因适用于共用段；本轮没有
  分别启动 SSE/HTTP server 重复验证各自的通知生产器。

## 方案评审

### 不建议：每次收到通知都 bump

实现简单，但 MCP 通知只是“请重新枚举”的提示，server 可以发送重复通知。该方案会让 phase 2 → 2 阴性对照产生
虚假 revision，违反 Issue 验收标准。

### 不建议：只比较工具数量或 `tool_routes`

可以识别新增/移除，却漏掉本实验已经证实的同名 schema 变化。描述、input schema、annotations、`_meta` 或本地合并的
tool metadata 变化都属于 Agent-facing 投影变化，不能只比较名称路由。

### 推荐：manager 原子提交完整工具投影并返回 changed outcome

1. 在 `MCPServerManager` 内维护与 `tool_routes` 同一轮 `tools/list` 构建出的规范化 Agent-facing 工具投影快照；
   投影至少包含 exposed name、description、input/output schema、annotations、`_meta` 以及合并后的
   `a2c_tool_meta`。
2. 在 `refresh_tool_routes` 的 generation/current-client 校验通过后，把新 routes、disabled set 和投影快照作为同一提交
   临界区更新，并以结构化 outcome 返回 `projection_changed`。比较完整规范化值，不比较数量，也不依赖哈希碰撞。
3. 保持现有公开 `refresh_tool_routes() -> Result<()>` 和兼容别名不变，新增 crate 内部 outcome 入口，避免破坏 SDK API。
4. `McpChangeReactor` 持有共享 `RuntimeStatus`：仅在 refresh 成功且 `projection_changed=true` 后调用
   `bump_capability()`，随后广播 Office 工具更新；相同投影则同时跳过本地事件和无意义的 Office 重载。
5. 保持事件顺序为“新投影已原子提交 → revision bump/event → 消费方重新读取 status”，确保事件到达时
   `ComputerStatusSnapshot.tools` 已是新值。刷新失败不得 bump 或广播成功信号。

该方案根治“事件缺失”和“重复通知误报”两个方向，并覆盖 schema-only 变化，不引入轮询。

## 建议测试

- manager 单元测试：新增、schema-only、相同投影、移除分别返回 `changed=true/true/false/true`。
- Computer 编排测试：事件只在真实变化时产生，revision 每次严格 `+1`，且事件后读取的 status 工具数已更新。
- 真实 stdio 集成测试：扩展现有 `mcp_change_notifications.rs`，用可变 server 覆盖完整通知生产与消费链；实现后由 AI
  当次运行到 PASS。
- 并发回归：通知刷新与 start/stop/client replacement 竞争时沿用 generation 校验，禁止陈旧刷新覆盖新投影或重复 bump。

## 实验文件

- `Cargo.toml`：隔离实验 crate。
- `src/main.rs`：真实 stdio 驱动、三轮采样与结果记录。
- `results.json`：原始结构化结果。
- `Cargo.lock`、`target/`：实验独立依赖锁与构建产物；用户认可报告后可清理。
