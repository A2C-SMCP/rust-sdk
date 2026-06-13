# 场景：settings-scope（Rust SDK）

## 测试目标

验证 `smcp-computer settings` 子命令的五级 scope 体系：user / project / local / flag / policy，
包括 show、get、set 操作，scope merge 语义，以及只读 scope 的错误处理。

## 类型

CLI-only（不需要 Server/Computer/Agent 多进程）

## 前置条件

1. CLI 已编译：`cargo build -p smcp-computer --features cli`
2. 二进制可用：`target/debug/smcp-computer`（下文用 `$A2C`）

## ⚠️ 隔离要求（重要）

> `settings.json` **不**存放在 `A2C_SKILL_HOME` 下，而是 `XDG_CONFIG_HOME/a2c/settings.json`
> （回退 `$HOME/.config/a2c/`，见 `crates/smcp-computer/src/settings/scope.rs`
> `resolve_user_config_dir`）。**必须同时隔离 `XDG_CONFIG_HOME`（绝对路径）**，否则
> `settings set` 会写入用户真实配置！

```bash
A2C="$(pwd)/target/debug/smcp-computer"
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home"
export XDG_CONFIG_HOME="$U/config"          # 必须绝对路径
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
```

> 与 python-sdk 命令映射：`uv run a2c-computer settings ...` → `$A2C settings ...`。

## 测试用例

### G-01: settings show merged（默认合并视图）

- **优先级**: P0
- **步骤**: `$A2C settings show --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - 隔离环境下输出为空对象 `{}`（无任何 scope 设置时；非空环境则为合并对象）

### G-02: settings show --scope user

- **优先级**: P0
- **步骤**: `$A2C settings show --scope user --json`
- **预期结果**:
  - 退出码 0
  - 初始为空对象 `{}`

### G-04: settings set --scope user（先于 G-03 执行）

- **优先级**: P0
- **步骤**:
  1. `$A2C settings set strictKnownMarketplaces true --scope user --json`
  2. 验证：`$A2C settings show --scope user --json`
- **预期结果**（已对照 Rust 实际输出）:
  - set 退出码 0，输出 `{"key": "strictKnownMarketplaces", "scope": "user", "value": true}`
  - show --scope user 输出含 `"strictKnownMarketplaces": true`
  - 落盘文件：`$XDG_CONFIG_HOME/a2c/settings.json`

### G-03: settings get（获取单个 key）

- **优先级**: P0
- **前置**: G-04 成功
- **步骤**: `$A2C settings get strictKnownMarketplaces --json`
- **预期结果**:
  - 退出码 0
  - 输出 `{"strictKnownMarketplaces": true}`

### G-05: settings set --scope project（无 active workdir 应失败）

- **优先级**: P1
- **步骤**: `$A2C settings set strictKnownMarketplaces true --scope project --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 1
  - 输出 `{"error": "scope \"project\" requires an active workdir (use --add-dir at startup)"}`
  - 含 `requires an active workdir` 关键词

### G-06: scope merge 验证（覆盖后合并取最新）

- **优先级**: P1
- **前置**: G-04 成功
- **步骤**:
  1. `$A2C settings set strictKnownMarketplaces false --scope user --json`
  2. `$A2C settings get strictKnownMarketplaces --json`
- **预期结果**:
  - set 退出码 0
  - get 返回 `{"strictKnownMarketplaces": false}`（合并视图反映最新值）

### G-07: 只读 scope 错误（policy / flag 不可写）

- **优先级**: P0
- **步骤**:
  1. `$A2C settings set testKey value --scope policy --json`
  2. `$A2C settings set testKey value --scope flag --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 两条均退出码 1
  - 输出形如 `{"error": "scope \"policy\" is read-only (writable: user|project|local)"}`
    （flag 同理）
  - 含 `read-only` 与 `writable` 关键词

### G-08: flag scope with --settings（通过文件传入 flag scope）

- **优先级**: P1
- **步骤**:
  1. `echo '{"testFlag": true, "flagKey": "flagValue"}' > $U/flag.json`
  2. `$A2C --settings $U/flag.json settings show --scope flag --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - JSON 含 `"testFlag": true` 和 `"flagKey": "flagValue"`
  - 注意：`--settings` 是 root 级 flag，必须在子命令 `settings` **之前**

## 清理

```bash
rm -rf "$U"
```

## 日志收集

CLI-only 场景下日志即命令 stdout/stderr。每个用例执行后保存完整输出。
