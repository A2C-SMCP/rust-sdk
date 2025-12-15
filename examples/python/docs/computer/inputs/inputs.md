## Computer Inputs 子系统概览

`a2c_smcp/computer/inputs` 目录下的代码，主要实现了 **Computer 侧的动态输入定义解析与渲染能力**，用于在运行时按需解析 MCP 配置中的 `${input:<id>}` 占位符，并通过 CLI 与用户交互获取真实值。它与 `mcp_clients` 模块中的 `MCPServerInput*` 配置模型协同工作，为 MCP Server 连接参数等提供惰性解析能力。

在整体架构中，inputs 子系统承担的角色可以概括为：

- **输入定义到实际值的桥梁**：把 `MCPServerPromptStringInput` / `MCPServerPickStringInput` / `MCPServerCommandInput` 三类输入定义解析成具体值。
- **统一缓存与会话管理**：在解析后缓存每个 input 的值，避免重复询问或重复执行命令；抽象出通用的 Session 类型，便于未来接入 GUI/Web 等交互环境。
- **配置渲染器**：对任意嵌套结构（dict/list/str）的配置进行递归扫描与渲染，按需调用输入解析器获得真实值。

后续工程师在继续开发该模块时，主要会涉及：

- 为新的交互环境（例如 GUI、远程 Web 前端）实现新的 InputResolver 子类；
- 扩展 CLI 交互体验，例如更丰富的提示样式或历史记录；
- 在 `Computer` 启动流程中新增依赖 inputs 的配置字段；
- 增强 ConfigRender 的能力，例如支持新的占位符前缀或更复杂的嵌套结构处理策略。

下面按文件说明核心职责与扩展方式。

---

## 抽象基类：`base.py`

### BaseInputResolver 泛型基类

`BaseInputResolver[S]` 是输入解析器的抽象基类，负责统一管理输入定义与解析结果缓存，同时把“会话上下文”抽象为泛型 `S`，便于在不同交互环境中复用：

- **输入定义快照**：
  - 构造函数接收 `Iterable[MCPServerInput]`，立即转为 `{id -> input}` 的字典：
    - 确保解析过程中不会受外部配置变化影响；
    - 便于按 id 快速查找对应的输入定义。

- **缓存管理接口**：
  - `clear_cache(key: str | None)`：清空全部缓存或指定 id 的缓存；
  - `get_cached_value(input_id)`：获取某个输入的当前缓存值；
  - `set_cached_value(input_id, value)`：仅当 id 存在于定义中时才设置缓存，返回是否成功；
  - `delete_cached_value(input_id)`：删除指定缓存项并返回是否真的删除；
  - `list_cached_values()`：返回当前所有缓存值的浅拷贝，用于 CLI 展示或调试。

- **会话抽象**：
  - 成员 `self.session: S | None` 存放默认会话对象；
  - 子类在实现具体解析逻辑时可以根据需要传入或覆盖 session（例如 CLI 中使用 `PromptSession`，GUI 中使用 WebSocket 会话等）。

- **抽象解析接口**（由子类实现）：
  - `aresolve_by_id(input_id, *, session: S | None = None)`：按 id 解析输入，可能触发用户交互或命令执行；
  - `_aresolve_prompt(cfg: MCPServerPromptStringInput, *, session)`：解析 promptString 类型输入；
  - `_aresolve_pick(cfg: MCPServerPickStringInput, *, session)`：解析 pickString 类型输入；
  - `_aresolve_command(cfg: MCPServerCommandInput)`：解析 command 类型输入。

从设计上看，`BaseInputResolver` 把“要解析什么”（MCPServerInput）和“在什么上下文/怎样解析”（会话 & 交互实现）解耦，为后续接入不同交互形态铺平了道路。

---

## CLI 交互 I/O 工具：`cli_io.py`

`cli_io.py` 提供了若干与 CLI 交互相关的异步工具函数，供 `InputResolver` 等上层组件重用。这些函数本身与 MCP 协议无关，专注于本地命令行交互体验：

### ainput_prompt

```python
async def ainput_prompt(message: str, *, password: bool = False, default: str | None = None, session: PromptSession | None = None) -> str
```

- 功能：提示用户输入字符串，可选密码模式（输入内容不回显），支持默认值。
- 行为细节：
  - 若未提供 `session`，则内部创建新的 `PromptSession`，并通过 `patch_stdout(raw=True)` 避免与上层 `a2c>` 交互循环冲突；
  - 若提供了 `session`，则直接复用外层交互循环的会话，避免光标抖动与额外换行；
  - 捕获 `EOFError` / `KeyboardInterrupt` 时返回默认值或空字符串。

### ainput_pick

```python
async def ainput_pick(message: str, options: list[str] | tuple[str, ...], *, default_index: int | None = None, multi: bool = False, session: PromptSession | None = None) -> None | list[str] | str
```

- 功能：让用户通过序号选择一个或多个字符串选项，支持默认选项与多选。
- 实现要点：
  - 使用 `rich.Table` 以表格方式展示所有选项（索引 + 内容）；
  - 支持单选与多选：
    - 单选：输入单个序号；
    - 多选：用逗号分隔多个序号，并在结果中去重保持顺序；
  - 出错提示为中英文双语（提示序号越界或格式错误），与整体风格一致；
  - 当用户直接回车且存在默认索引时，会自动返回默认选项。

### arun_command

```python
async def arun_command(command: str, *, cwd: str | None = None, timeout: float | None = None, shell: bool = True, parse: str = "raw") -> Any
```

- 功能：在本地异步执行命令，支持超时、工作目录以及输出解析模式。
- 行为说明：
  - `shell=True` 时使用 `asyncio.create_subprocess_shell`，command 作为完整字符串执行；
  - `shell=False` 时简化处理，只把 `command` 作为可执行名，不拆分参数；
  - 若进程超时，会尝试 `kill()` 并抛出 `TimeoutError`；
  - 若返回码非 0，抛出 `RuntimeError`，并附带 STDERR 输出；
  - `parse` 支持三种模式：
    - `raw`：返回去掉首尾空白的原始字符串；
    - `lines`：按行分割，过滤空行；
    - `json`：尝试 `json.loads`，失败则回退为原始文本。

在 inputs 子系统中，`arun_command` 主要被 command 类型输入解析器复用，用于从 IDE 内部命令或外部脚本获取动态配置值。

---

## 配置渲染器：`render.py`

`render.py` 提供了 `ConfigRender` 类，用于在 `Computer` 启动或重载 MCP 配置时，对其中的占位符 `${input:<id>}` 进行按需渲染。它是连接 `InputResolver` 与 MCP 配置模型的关键一环。

### 占位符规则与匹配

- 使用正则 `PLACEHOLDER_PATTERN = re.compile(r"\$\{input:([^}]+)}")` 匹配形如 `${input:CLAUDE_CWD}` 的占位符；
- 仅支持 `input:` 前缀，后续若扩展其他类型占位符（例如 `${env:VAR}`）可以在此处增加新的解析逻辑。

### ConfigRender.arender

```python
async def arender(self, data: Any, resolve_input: Callable[[str], Awaitable[Any]], _depth: int = 0) -> Any
```

- 功能：对任意结构的数据进行递归渲染：
  - dict：对每个 value 递归调用 `arender`；
  - list：对每个元素递归调用 `arender`；
  - str：调用 `ConfigRender.arender_str` 处理占位符；
  - 其他类型：原样返回。
- 深度控制：
  - 通过 `max_depth` 参数限制递归深度（默认 10 层），超出时报错日志并停止继续展开，防止循环引用或异常嵌套导致无限递归。

### ConfigRender.arender_str

```python
@staticmethod
async def arender_str(s: str, resolve_input: Callable[[str], Awaitable[Any]]) -> Any
```

- 功能：对单个字符串中的所有 `${input:<id>}` 占位符进行按需替换。
- 特殊情况处理：
  - 若字符串本身仅包含一个占位符，且没有其他字符：
    - 直接返回解析后的值本身，允许返回任意类型（例如 dict/list/number），便于在配置中嵌入复杂结构；
  - 否则：
    - 对每个占位符依次解析，并把解析结果转为字符串插入到原始文本中；
    - 若某个 id 不存在或解析失败，会记录 warning/error 日志，但保留原始字符串不变。

通过这种设计，`ConfigRender` 既支持简单的“字符串插值”，也支持将整个字段当作一个动态值来源，从而在 MCP 配置中灵活引入 secrets、动态路径等信息。

---

## 输入解析器实现：`resolver.py`

`resolver.py` 基于 `BaseInputResolver` 和 `cli_io`，给出了在 CLI 环境下的具体实现：

### InputResolver

```python
class InputResolver(BaseInputResolver[PromptSession]):
    ...
```

- 角色：用于在 CLI 下按需解析 `MCPServerInput`，并缓存解析结果。
- 构造：
  - 接收 `Iterable[MCPServerInput]` 与可选的 `PromptSession`；
  - 通过 `super().__init__` 初始化定义快照与缓存。

#### aresolve_by_id

```python
async def aresolve_by_id(self, input_id: str, *, session: PromptSession | None = None) -> Any
```

- 行为流程：
  1. 若在 `_cache` 中已存在该 id，对应值直接返回；
  2. 否则从 `_inputs` 中查找定义，不存在则抛出 `InputNotFoundError`；
  3. 根据定义的具体类型分派到不同的解析方法：
     - `MCPServerPromptStringInput` → `_aresolve_prompt`；
     - `MCPServerPickStringInput` → `_aresolve_pick`；
     - `MCPServerCommandInput` → `_aresolve_command`；
  4. 将解析得到的值写入 `_cache`，再返回给调用方。

- 会话选择：
  - 优先使用调用时传入的 `session`；
  - 若不存在，则回退到构造时设置的 `self.session`；
  - 这样既支持在交互式 CLI 主循环内复用 Session，也支持在单次脚本场景下临时创建 Session。

#### _aresolve_prompt

- 行为：
  - 使用输入定义的 `description` 作为提示信息；若为空，则退化为 `请输入 <id>` 的默认提示；
  - 根据 `password` 字段决定是否启用密码模式；
  - 调用 `ainput_prompt` 获取用户输入，自动处理默认值与中断异常。

#### _aresolve_pick

- 行为：
  - 使用 `description` 或默认提示 `请选择 <id>`；
  - 从 `options` 中计算默认索引（若 `default` 在 options 列表中）；
  - 调用 `ainput_pick` 提示用户选择；
  - 最终返回字符串形式的结果：
    - 当用户没有选择时，退化为 `cfg.default` 或空字符串。

#### _aresolve_command

- 行为：
  - 约定 `cfg.command` 是完整可执行命令字符串，由 `shell=True` 的 `arun_command` 直接执行；
  - 当前不拼接 `args` 字段，后续可按需要扩展；
  - 返回命令执行的原始输出字符串或解析结果，供上层进一步处理。

### InputNotFoundError

- 自定义异常类型，继承自 `KeyError`，在找不到指定输入 id 时抛出；
- 主要用于帮助 CLI 与上层调用方做更明确的错误区分和提示。

---

## 与其他模块的集成关系

### 与 `a2c_smcp/computer/mcp_clients` 模块

- `mcp_clients.model` 中定义了三类 `MCPServerInput`：
  - `MCPServerPromptStringInput` / `MCPServerPickStringInput` / `MCPServerCommandInput`；
  - 这些定义被 `Computer` 持有，并在启动或运行期按需交给 `InputResolver` 解析。
- 在 `Computer` 启动流程中：
  - 会根据当前 inputs 定义创建一个 `InputResolver` 实例；
  - 在渲染 MCP Server 配置（例如 `StdioServerConfig` 的 `server_parameters`）时，通过 `ConfigRender.arender` 将 `${input:<id>}` 占位符替换为实际值；
  - 渲染完成后再将结果序列化并传入 `MCPServerManager.ainitialize`。

### 与 `a2c_smcp/computer/computer.py` 主模块

- `Computer` 持有一组 inputs 定义与 MCP Server 配置：
  - inputs 用于描述所有可能的动态变量；
  - MCP Server 配置中使用 `${input:<id>}` 引用这些变量；
  - 通过 `InputResolver + ConfigRender` 实现从“配置模板”到“运行时配置”的转换。
- 在 CLI 场景下，当用户在 `interactive_impl.py` 中执行诸如 `inputs load`、`inputs value set` 等命令时：
  - 实际调用的是 inputs 子系统中提供的缓存管理与解析能力；
  - 例如：`inputs value set <id>` 会根据定义中的 `default` 值或用户输入更新缓存；
  - 再次渲染 MCP 配置时，会优先读取这些缓存值，避免重复询问。

### 与 CLI 交互实现

- `a2c_smcp/computer/cli/interactive_impl.py` 中的交互命令（如 `inputs list`、`inputs value list`、`inputs value set` 等）是 inputs 子系统在 CLI 中的直接入口：
  - 这些命令底层依赖 `InputResolver` 的缓存与解析接口；
  - `cli_io` 中的 I/O 函数提供了交互体验的基础能力；
  - 通过把“定义加载/变更”和“缓存查看/修改”解耦，保持 inputs 子系统内部状态的可控性。

---

## 典型使用流程梳理

以在本地配置一个需要动态路径和密钥的 MCP Server 为例，整体流程大致如下：

1. **编写 inputs 定义与 MCP 配置**：
   - 在配置文件中定义若干 `MCPServerInput`，例如：
     - `CLAUDE_CWD`：用于指定工作目录；
     - `FEISHU_APP_ID` / `FEISHU_APP_SECRET`：用于存放密钥；
   - 在 MCP Server 的 `server_parameters` 中使用 `${input:CLAUDE_CWD}` 等占位符引用这些 id。

2. **加载 inputs 与配置**：
   - 在 CLI 中通过 `inputs load @config.json` 等命令加载定义；
   - `Computer` 内部创建 `InputResolver` 并保存输入定义；
   - 此时缓存 `_cache` 仍为空，仅存储定义快照 `_inputs`。

3. **按需设置或解析输入值**：
   - 用户可以通过 CLI：
     - `inputs value set <id>` 显式设置某个输入的值（可使用 default）；
     - 或在第一次启动 MCP Server 时，由 `ConfigRender` 触发 `InputResolver.aresolve_by_id`，通过 CLI 交互询问用户。

4. **渲染 MCP 配置并启动客户端**：
   - `Computer` 使用 `ConfigRender.arender` 对 MCP Server 配置进行渲染；
   - 占位符 `${input:<id>}` 被替换为缓存或实时解析得到的值；
   - 渲染结果传入 `MCPServerManager.ainitialize`，建立到 MCP Server 的实际连接。

5. **后续变更与重载**：
   - 当 inputs 定义或缓存发生变化时，可以重新渲染配置并重启相关 MCP Client；
   - 对于需要频繁变更的参数（例如临时 token），可以只更新 cache，而不修改原始定义文件。

---

## 开发与扩展建议

在继续演进 inputs 子系统时，可以参考以下原则：

- **分层清晰**：
  - 把“输入定义”（`MCPServerInput`）与“解析逻辑”（InputResolver 子类）拆开；
  - 把“配置渲染”（`ConfigRender`）与“配置持久化/加载”（CLI 命令与外部文件）拆开。

- **缓存语义明确**：
  - `_inputs` 始终表示定义快照，不随运行时变化；
  - `_cache` 仅表示当前解析出的值，可以通过 CLI 清空或单个删除；
  - 默认值 `default` 不自动写入缓存，只在交互时作为提示或兜底值。

- **可替换的交互后端**：
  - 在需要支持 GUI 或远端 Web UI 时，可以：
    - 复用 `BaseInputResolver` 的缓存与调度逻辑；
    - 替换 `cli_io` 相关实现为 WebSocket/RPC 调用；
    - 保持 `ConfigRender` 与 MCP 配置结构不变，减少整体改动范围。

通过保持上述边界与约定，inputs 子系统可以在不影响 `mcp_clients` 与 `Computer` 主模块的前提下灵活演进，同时为 IDE4AI 的多种运行形态提供一致、可预测的动态输入体验。

