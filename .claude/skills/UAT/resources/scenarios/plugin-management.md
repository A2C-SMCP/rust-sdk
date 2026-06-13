# 场景：plugin-management（Rust SDK）

## 测试目标

验证 `smcp-computer plugin` 子命令的完整生命周期：安装、列出、查看详情、禁用、启用、
卸载、垃圾回收，以及 scope 和 --keep-servers 选项。

## 类型

CLI-only（不需要 Server/Computer/Agent 多进程）

## 前置条件

1. CLI 已编译：`cargo build -p smcp-computer --features cli`，`$A2C` 指向二进制
2. 测试 marketplace Git 仓库已准备（见「测试仓库搭建」）

## 隔离要求（⚠️ 双隔离）

```bash
A2C="$(pwd)/target/debug/smcp-computer"
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home"
export XDG_CONFIG_HOME="$U/config"     # enabledPlugins / trust 落盘处，必须绝对路径
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
```

## 测试仓库搭建

> **复用 seed**: `seeds/marketplace/plugin-with-bundled-mcp`
> marketplace 名: `mp-bundled-mcp`（用 `--name` 显式指定）；plugin: `foo`；
> skill: `foo:valid-skill-pkg`；捆绑 MCP: `figma-mcp`

```bash
SEEDS_ROOT=<项目根>/.claude/skills/UAT/resources/seeds
WORK="$U/work"; BARE="$U/bare.git"
bash "$SEEDS_ROOT/marketplace/_helpers/init_bare_repo.sh" \
  "$SEEDS_ROOT/marketplace/plugin-with-bundled-mcp" "$WORK" "$BARE"
$A2C marketplace add "file://$BARE" --name mp-bundled-mcp --trust --json
```

## 测试用例

> 命令前提：已设置双隔离 env + marketplace 已添加（除非用例自带 setup）。

### P-01: plugin install（安装 plugin）

- **优先级**: P0
- **步骤**: `$A2C plugin install foo@mp-bundled-mcp --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - JSON 形如 `{"bundledMcpServers": ["figma-mcp"], "installed": "foo@mp-bundled-mcp", "scope": "user"}`
  - `bundledMcpServers` 含 `figma-mcp`

### P-02: plugin list（列出已安装）

- **优先级**: P0 / **前置**: P-01
- **步骤**: `$A2C plugin list --json`
- **预期结果**:
  - 退出码 0
  - 数组含 `{"id": "foo@mp-bundled-mcp", "enabled": true, "scopes": ["user"], "bundledMcpServers": ["figma-mcp"]}`

### P-03: plugin info（查看详情）

- **优先级**: P0 / **前置**: P-01
- **步骤**: `$A2C plugin info foo@mp-bundled-mcp --json`
- **预期结果**:
  - 退出码 0
  - JSON 含 `id`、`enabled`、`records[]`，record 内含 `scope`、`installPath`、`version`、
    `commitSha`、`bundledMcpServers:["figma-mcp"]`、`installedAt`、`lastUpdated`

### P-04: plugin disable（禁用）

- **优先级**: P0 / **前置**: P-01
- **步骤**:
  1. `$A2C plugin disable foo@mp-bundled-mcp --json`
  2. `$A2C plugin list --available --json`
  3. `$A2C plugin list --json`
- **预期结果**（已对照 Rust 实际输出）:
  - disable 输出 `{"disabled": "foo@mp-bundled-mcp", "scopes": ["user"]}`，退出码 0
  - `list --available` 显示该 plugin，`enabled: false`
  - `list`（不带 --available）返回 `[]`（不显示已禁用）

### P-05: plugin enable（重新启用）

- **优先级**: P0 / **前置**: P-04
- **步骤**:
  1. `$A2C plugin enable foo@mp-bundled-mcp --json`
  2. `$A2C plugin list --json`
- **预期结果**:
  - enable 输出 `{"enabled": "foo@mp-bundled-mcp", "scopes": ["user"]}`，退出码 0
  - `list` 显示该 plugin，`enabled: true`

### P-06: plugin uninstall（卸载，级联移除捆绑 server）

- **优先级**: P0 / **前置**: P-01
- **步骤**:
  1. `$A2C plugin uninstall foo@mp-bundled-mcp --json`
  2. `$A2C plugin list --json`
- **预期结果**（已对照 Rust 实际输出）:
  - uninstall 输出 `{"keptServers": false, "uninstalled": "foo@mp-bundled-mcp"}`，退出码 0
  - `list` 返回 `[]`
  - 捆绑 MCP server 一并移除（`keptServers: false`）

### P-07: plugin gc（垃圾回收孤立 plugin）

- **优先级**: P1
- **步骤**:
  1. 全新 setup + `$A2C plugin install foo@mp-bundled-mcp --json`
  2. 从隔离 settings 移除启用意图，制造孤儿：
     ```bash
     SET="$XDG_CONFIG_HOME/a2c/settings.json"
     python3 -c "import json;p='$SET';d=json.load(open(p));d.pop('enabledPlugins',None);json.dump(d,open(p,'w'),indent=2)"
     ```
  3. `$A2C plugin gc --json`
  4. `$A2C plugin list --json`
- **预期结果**（已对照 Rust 实际输出）:
  - gc 退出码 0，输出 `{"removed": ["foo@mp-bundled-mcp"]}`
  - `list` 返回 `[]`

### P-08: plugin install --scope（指定 scope）

- **优先级**: P1 / **前置**: marketplace 已添加（全新 setup）
- **步骤**:
  1. `$A2C plugin install foo@mp-bundled-mcp --scope user --json`
  2. `$A2C plugin info foo@mp-bundled-mcp --json`
- **预期结果**:
  - install 退出码 0，输出 `scope: "user"`
  - info 的 records 含 `scope: "user"`

### P-09: plugin uninstall --keep-servers（卸载但保留 MCP server）

- **优先级**: P1 / **前置**: P-01
- **步骤**: `$A2C plugin uninstall foo@mp-bundled-mcp --keep-servers --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - 输出 `{"keptServers": true, "uninstalled": "foo@mp-bundled-mcp"}`
  - 捆绑的 MCP server（figma-mcp）未被移除（`keptServers: true`）

## 清理

```bash
rm -rf "$U"
```

## 日志收集

CLI-only 场景下日志即命令 stdout/stderr。每个用例执行后保存完整输出。
