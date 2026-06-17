# 场景：mcp-fastmcp-skill（Rust SDK）

## 测试目标

验证当通用 / FastMCP-style MCP provider 以 **`smcp-computer 0.2.2` 可注册的形状**暴露 skill 资源时，
经 `restage_mcp_skills` 能被注册为 MCP skill 并被 `computer.get_skills()` 收集。

> 背景（AS-40 / comment 13849）：FastMCP **默认** Provider 暴露的裸 `skill://<name>/SKILL.md`
> （无 `_meta.source`、技能名在 host 段）**当前不会**被 `restage_mcp_skills` 注册——这属于 provider
> 侧适配范畴（SDK 暂不放宽兼容）。可注册形状要求 provider 以 `_meta.source = "resources"` 的 **skill 根**
> + 其下子资源（至少 `SKILL.md`）暴露。本场景覆盖「provider 已适配后」的可注册形状。

## 类型

MCP / runtime-boot — **非 CLI-only**。

> ⚠️ CLI 局限：`smcp-computer` CLI 的 MCP skill staging 仅在 `boot_up` 触发，而 MCP server 的批准/连接
> 发生在 boot **之后**，故 `skill list --source mcp` 在 CLI 流程中观测不到 live MCP skill。可注册性的
> 自动化验证落在 gated Rust 集成测试（boot → 连接 → restage → `get_skills`）。

## 可注册的资源形状（provider 暴露）

```
skill://fastmcp-demo/root              _meta = { "source": "resources" }   ← skill 根
skill://fastmcp-demo/root/SKILL.md     ← 入口（YAML frontmatter，name=fastmcp-demo）
skill://fastmcp-demo/root/reference.md ← supporting file（resources 模式子资源）
```

- `_meta.source=resources` 必须挂在 **skill 根 resource** 上，不能直接挂在 `.../SKILL.md`。
- 根 URI 下须至少有 `SKILL.md` 子资源。
- 注册结果：`name = mcp:<server>:<frontmatter.name>`、`source = mcp:<server>`、`uri = <root URI>`、
  物化进 `mcp/<server>/<frontmatter.name>/`。

## 种子 / Seed

- MCP server fixture（可注册形状）：`tests/fastmcp-skill-server/index.js`（项目根，最小 stdio MCP server，
  `resources/list` + `resources/read`）。亦经 `seeds/mcp/fastmcp-skill-server/README.md` 索引。

## 测试用例

### MF-01: FastMCP（resources-mode 形状）skill 被注册并可发现

- **优先级**: P0 / **类型**: gated 集成测试（需 Node.js）
- **步骤**:
  ```bash
  cargo test --package smcp-computer --test fastmcp_skills_integration -- --ignored
  ```
- **预期结果**（已对照实际输出，current SDK 直接通过、无需改码）:
  - 退出码 0，1 passed
  - `restage_mcp_skills` 返回含 `mcp:fastmcp-skill-test:fastmcp-demo`
  - `get_skills()` 含该 skill：`source=mcp:fastmcp-skill-test`、`uri=skill://fastmcp-demo/root`、
    `description` 来自 SKILL.md frontmatter
  - runtime skill home 落盘 `mcp/fastmcp-skill-test/fastmcp-demo/{SKILL.md,reference.md}`

### MF-02（负向，文档化）: 裸 `skill://<name>/SKILL.md`（无 _meta.source）不注册

- **优先级**: P1 / **状态**: 现状契约（SDK 暂不兼容裸 FastMCP 布局）
- **说明**: 若 provider 仅暴露 `skill://fastmcp-demo/SKILL.md`（无 `_meta.source`、无 root 子资源），
  `restage_mcp_skills` 不会注册该 skill；这是预期现状，由 provider 侧改为 MF-01 形状解决，非 SDK bug。

## 清理

集成测试用 `TempDir`，自动清理；无外部副作用。

## 日志收集

`cargo test ... -- --ignored --nocapture` 保存 stdout；失败时附 `RUST_LOG=smcp_computer=debug` 重跑。
