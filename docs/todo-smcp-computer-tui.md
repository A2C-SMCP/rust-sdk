<!--
文件名: todo-smcp-computer-tui.md
作者: JQQ
创建日期: 2025/12/18
最后修改日期: 2025/12/18
版权: 2023 JQQ. All rights reserved.
依赖: None
描述: smcp-computer Rust CLI 终端美化（TUI）改造计划与TODO清单
-->

# smcp-computer CLI TUI 美化改造 TODO

## 硬性前提（必须遵守）

- **长期保证 `TUI` 与 `Plain` 两种输出模式同时可用**
  - 即使未来 TUI 体验成熟，用户仍可选择继续使用当前 `Plain` 模式。
  - `Plain` 是稳定/脚本/CI 的默认与兜底路径；`TUI` 是交互增强路径。

## 目标

- **对齐 Python CLI 体验**
  - 表格化展示（多块区域/分组）
  - 颜色/高亮（连接状态、错误、关键字段）
  - 交互（刷新、筛选、分页/滚动、复制/导出）
- **保持脚本友好**
  - 非 TTY/管道输出自动回退为纯文本
  - 提供 `--ui <plain|tui>` 等开关

## 推荐技术选型

本改造按**全屏 TUI**落地，技术栈固定如下：

- **TUI 渲染**：`ratatui`
- **终端事件/控制**：`crossterm`
- **中文宽度/对齐**：`unicode-width`
- **折行/截断**：`textwrap`
- **日志**：`tracing` + `tracing-subscriber`（TUI 模式日志固定输出到 stderr，避免破坏屏幕）

## 改造原则（强制）

- **数据采集与输出解耦**：命令逻辑不要直接拼字符串输出。
- **分层**：`Model -> ViewModel -> Renderer`。
- **Plain 路径必须长期保留**：任何新增能力优先通过 Presenter/ViewModel 扩展，禁止把“只有 TUI 才能显示/拿到的数据”写死在 TUI 渲染层。
- **异常可见性不退化**：TUI 模式下发生 `Err` 或 `panic` 时，必须先恢复终端状态（退出 raw mode / alternate screen），再将详细错误信息输出到 stderr，保证排障体验不比当前 Plain 模式更差。
- **统一回退策略**：
  - `stdout` 非 TTY：禁止进入 TUI，默认 `plain`。
  - 支持 `NO_COLOR`：关闭颜色。
- **不破坏现有命令语义**：默认行为尽量兼容旧版（除非明确引入新参数）。

## 分层设计（建议落地结构）

### 1) Domain / Model（数据模型层）

- 负责从 socketio/MCP 客户端获取原始状态数据。
- 输出结构化 `struct`（例如 `StatusModel`、`ToolsModel`）。

### 2) Presenter（展示模型层）

- 把 Domain 数据转换为适合展示的 `ViewModel`：
  - 字符串格式化（时间、人类可读大小）
  - 状态枚举（Connected/Disconnected/Warning/Error）
  - 表格行（rows）与列定义（columns）

### 3) Renderer（渲染层）

- `TuiRenderer`：ratatui 全屏渲染 + 交互循环
- `PlainRenderer`：纯文本输出（兼容旧行为，且作为长期保留的稳定路径）

Renderer 层必须满足：

- **同一份 ViewModel 同时可被 TUI 与 Plain 渲染**（避免两套展示逻辑分叉导致维护成本与行为不一致）。
- **Plain 输出格式长期可用**：即使后续新增 TUI 页面/字段，仍必须有对应的 Plain 输出（至少保证关键信息可读）。

## CLI 参数设计（建议）

- `--ui <plain|tui>`
  - `plain`：纯文本交互（兼容现有行为）
  - `tui`：全屏 TUI（后续改造完成后生效）
- 环境变量 `A2C_RUST_UI`
  - 值为 `plain` 或 `tui`
  - 用于设置“启动默认 UI”，便于发布/灰度/紧急回退
- `--watch <duration>`（可选）
  - 定时刷新，例如 `--watch 2s`

### UI 模式优先级（强制）

为了避免在每个命令里重复填写 `--ui`，UI 模式采用“启动默认 + 子命令覆盖”的策略。

生效优先级（从高到低）：

- 子命令显式指定的 `--ui <plain|tui>`
- 启动参数 `--ui <plain|tui>`
- 环境变量 `A2C_RUST_UI`（`plain`/`tui`）
- 程序内置默认值（建议改造期默认 `plain`，稳定后可调整默认值以实现发布）

补充约束（强制）：

- **`--ui plain` 必须在任何情况下可用**（包括 TTY 环境下也必须可强制走 Plain）。
- **`--ui tui` 只能在 TTY 环境下生效**；不满足 TTY 时必须自动降级为 `plain`，且不影响输出可读性。

示例：

```bash
# 中文: 设置启动默认 UI 为 tui
# English: Set the default UI to tui
smcp-computer --ui tui

# 中文: 全局默认 tui，但某个具体命令强制 plain（用于降级）
# English: Global default tui, but override to plain for a specific command
smcp-computer --ui tui <command> --ui plain

# 中文: 通过环境变量指定默认 UI
# English: Use env var to set default UI
A2C_RUST_UI=plain smcp-computer
```

## 并存策略与命名建议（强制）

### 为什么要并存（不直接替换）

- **风险隔离**：TUI 会引入 raw mode、事件循环、持续刷新、中文宽度对齐等新复杂度，改造期和初期难免出现边缘 Bug。
- **脚本兼容**：现有用户/CI 可能依赖纯文本输出；TUI 全屏模式不适合管道/重定向，需要稳定的 `plain` 路径。
- **灰度发布**：通过“显式开启 -> 默认值逐步切换”的路径，可以快速收集反馈并随时回退。

### 命名建议

- **对外称呼**：建议叫 **`TUI` 模式**，不建议叫 `CLI2`。
  - `CLI2` 容易让人误解为“命令语义/参数/输出协议全面升级 v2”。
  - `TUI` 精确表达“交互形态/渲染层”的变化。

### 入口方式（固定）

- 统一命令 + `--ui` 开关（必须实现）
  - `smcp-computer --ui tui`
  - `smcp-computer run --ui plain`
  - 要求：每个命令可逐步接入 TUI；不引入新的“产品入口”。

### 分阶段默认策略（执行要求）

- **阶段 1（改造期）**：默认 `plain`，必须显式开启（`--ui tui`）。
- **阶段 2（稳定初期）**：仍默认 `plain`，允许通过“启动命令默认值 / 环境变量默认值”灰度开启 `tui`。
- **阶段 3（成熟期）**：如需废弃旧输出，必须给出清晰周期与迁移指引，并保留 `plain` 作为紧急回退路径。

## 交互规范（TUI）

- **通用按键**
  - `q`：退出
  - `r`：刷新
  - `/`：筛选（进入输入模式）
  - `Esc`：退出输入模式/清空筛选
  - `↑/↓`：选择行（表格）
  - `PgUp/PgDn`：分页（可选）
  - `?`：帮助（快捷键提示）
- **导出/复制（可选）**
  - `e`：导出当前数据为文本（stdout）
  - `c`：复制选中行关键字段（如需跨平台剪贴板可后续加 `arboard`）

## UI 布局（以 `status` 为样板）

- 顶部：连接状态 / Server URL / Client ID / Office ID
- 中部：Namespace Connections 表
- 底部：MCP Servers Status 表
- Footer：快捷键提示与最后刷新时间

## TODO 清单（按里程碑）

### Milestone 1：基础设施（1-2 天）

- [ ] 新增 `src/cli/ui/` 模块
- [ ] 实现终端生命周期管理（raw mode / alternate screen / panic 恢复）
- [ ] 实现“异常可见性”兜底：TUI 退出/恢复后将错误详情输出到 stderr（含错误链）；并对 `panic` 设置 hook，确保用户可看到异常信息
- [ ] 实现 Theme（颜色、边框风格、标题风格）
- [ ] 封装通用 Table 渲染组件（列宽、截断、省略号、中文宽度）
- [ ] 增加 `--ui` / 回退逻辑（含 `A2C_RUST_UI` 与命令级覆盖）
- [ ] 定义并固化 `Plain` 输出契约（字段/顺序/关键行），作为长期兼容基线

### Milestone 2：迁移 `status`（2-4 天）

- [ ] 抽取 `StatusService::fetch() -> StatusModel`
- [ ] 增加 `StatusPresenter::present(StatusModel) -> StatusViewModel`
- [ ] 实现 `render_status_tui(view_model)`：多块表格 + 状态高亮
- [ ] 保留 `render_status_plain(view_model)`：兼容旧纯文本输出
- [ ] `status` 的 Plain 输出需覆盖 TUI 关键信息（连接状态、Server URL、Client/Office 标识、关键表格字段）

### Milestone 3：迁移其它命令（3-7 天）

- [ ] `list-tools`：支持筛选、分页/滚动、列对齐
- [ ] `list-servers` / `list-clients`：状态列高亮
- [ ] `connect/disconnect`：统一成功/失败提示样式

### Milestone 4：交互增强（可选，2-5 天）

- [ ] `--watch` 定时刷新
- [ ] `/` 搜索框（filter 状态持久化）
- [ ] `?` 帮助页
- [ ] `c` 复制 / `e` 导出

### Milestone 5：测试与发布（1-3 天）

- [ ] Presenter/ViewModel snapshot 测试（优先文本 snapshot）
- [ ] 非 TTY 回退测试（CI 环境）
- [ ] 强制 Plain 测试：在 TTY 环境使用 `--ui plain` 仍输出稳定且可读
- [ ] `NO_COLOR` 测试
- [ ] macOS/Linux/Windows Terminal 手动验收

## 验收标准（DoD）

- **功能**
  - `status` 在 TTY 下呈现表格 + 颜色 + 分组标题
  - 支持 `q` 退出、`r` 刷新、`/` 筛选（至少对一个表生效）
- **兼容**
  - `smcp-computer | cat` 不进入 TUI，输出可读 plain
  - TTY 环境下 `--ui plain` 必须强制走 Plain，且输出与非 TTY 行为一致（除颜色差异）
- **质量**
  - TUI 模式不污染 stdout（日志不破屏）
  - TUI 模式下发生错误或 `panic` 时：终端状态可恢复，且 stderr 可看到详细异常信息（用于继续排查）
  - 中文列对齐基本正确

## 风险与注意事项

- **中文对齐**：必须用 `unicode-width` 计算展示宽度，否则混排会错位。
- **日志与 TUI 冲突**：TUI 模式下将日志输出到 stderr，避免破坏 UI。
- **管道输出**：必须支持回退，否则会影响脚本/CI。
