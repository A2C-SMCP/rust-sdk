# 测试环境搭建指南（Rust SDK）

## 前置准备

```bash
# 编译 CLI（带 cli feature）
cargo build -p smcp-computer --features cli

# 确认二进制
./target/debug/smcp-computer --help

# 日志目录
mkdir -p /tmp/a2c-uat-logs
```

## CLI-only 场景环境（如 marketplace-ops / settings-scope）

CLI-only 场景**无需 tmux**，直接 Bash 执行即可（多进程协调才需要 tmux）。

```bash
A2C="$(pwd)/target/debug/smcp-computer"
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home"   # marketplace clone / skill 包
export XDG_CONFIG_HOME="$U/config"      # settings.json（必须绝对路径！）
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"

"$A2C" marketplace list --json
# ... 按场景用例逐条执行，保存 stdout/stderr

rm -rf "$U"
```

> ⚠️ **双隔离**：`A2C_SKILL_HOME` 只隔离 skill 包；`settings.json`（trust 决策 /
> enabledPlugins / scope 设置）落在 `$XDG_CONFIG_HOME/a2c/`。两者都必须重定向，
> 否则污染用户真实 `~/.config/a2c/settings.json`。

如确需 tmux（统一日志/交互）：

```
mcp__tmux__create-session  name: a2c-uat
mcp__tmux__execute-command  paneId: <pane>  command: cd <rust-sdk> && A2C_SKILL_HOME=... ./target/debug/smcp-computer marketplace list --json
mcp__tmux__capture-pane     paneId: <pane>  lines: 50
mcp__tmux__kill-session     sessionId: <id>
```

## 完整链路场景环境（如 full-protocol）

需要三个 window：Server → Computer → Agent。

### 进程角色（Rust）

| 角色 | 启动方式 |
|---|---|
| Server | `target/debug/examples/uat_test_server 127.0.0.1:0`（日志报告动态端口） |
| Computer | `./target/debug/smcp-computer --url http://127.0.0.1:<PORT> --approve-all-mcp run --mcp-config <cfg>` |
| Agent | `target/debug/examples/e2e_test_agent`（env 驱动） |

### 启动顺序（严格）

1. 创建带 PID/随机后缀的唯一 tmux session + 3 window；若显式名称碰撞则失败，不删除已有 session
2. **先 Server**：等日志出现 `listening on 127.0.0.1:<PORT>`
3. **再 Computer**：使用上表命令，等 `Enter interactive mode` 出现
4. **Computer 加入 Office**：执行 `start all`，再执行 `socket join <office_id> friday_hands`，等 `Joined office`
5. **最后 Agent**：连接同一 Server 并加入**同一 Office**

> 关键：未加入 Office 的 Computer 无法响应 Agent 的 `client:*` 请求；Agent 的
> `client:*` 请求中 `computer` 字段用 Computer **名称**（join 时设置），不是 SID。

### 日志收集

```bash
# 进程用 tee 双写
... 2>&1 | tee /tmp/a2c-uat-logs/{server,computer,agent}.log
# 失败时 capture-pane 各 200 行 + cat 日志文件
```

## 关键注意事项

1. **端口动态分配**：从 UAT server 日志的 `listening on` 行解析
2. **启动顺序**：Server 就绪 → Computer → 加入 Office → Agent
3. **环境隔离**：`A2C_SKILL_HOME` 用临时目录，避免污染用户真实配置（`~/.a2c/skills`）
4. **唯一命名**：临时目录和 tmux session 都使用 PID/随机后缀
5. **等待策略**：轮询 tee 日志（不依赖会折行的 pane）中的 `listening on` /
   `Enter interactive mode` / `Joined office`
