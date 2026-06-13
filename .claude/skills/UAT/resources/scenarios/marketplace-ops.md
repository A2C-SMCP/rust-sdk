# 场景：marketplace-ops（Rust SDK）

## 测试目标

验证 `smcp-computer marketplace` 子命令的完整生命周期：添加、列出、查看详情、刷新、设置、删除。

## 类型

CLI-only（不需要 Server/Computer/Agent 多进程）

## 前置条件

1. CLI 已编译：`cargo build -p smcp-computer --features cli`
2. 二进制可用：`target/debug/smcp-computer`（下文用 `$A2C` 指代）
3. 测试 marketplace Git 仓库已准备（见下方「测试仓库搭建」）

## CLI 调用约定

```bash
# 方式一（推荐，跑多用例更快）：预编译后直接用二进制
cargo build -p smcp-computer --features cli
A2C="$(pwd)/target/debug/smcp-computer"

# 方式二（对齐 python-sdk 的 `uv run a2c-computer` 习惯）：
# A2C="cargo run -q -p smcp-computer --features cli --"
```

> 与 python-sdk 的命令映射：`uv run a2c-computer marketplace ...` → `$A2C marketplace ...`。
> 环境变量 `A2C_SKILL_HOME` 两端通用（Rust 见 `crates/smcp-computer/src/skills/home.rs`）。

## 测试仓库搭建

> **复用 seed**: 本场景使用 `seeds/marketplace/valid-single-plugin` seed。
> marketplace 名: `uat-seed-mp`，plugin: `foo`，skill: `foo:valid-skill-pkg`。

搭建脚本（在 tmux 或 Bash 中执行）：

```bash
SEEDS_ROOT=<项目根>/.claude/skills/UAT/resources/seeds
SEED="$SEEDS_ROOT/marketplace/valid-single-plugin"
TMPROOT=$(mktemp -d) && WORK="$TMPROOT/work" && BARE="$TMPROOT/test-mp.git"
bash "$SEEDS_ROOT/marketplace/_helpers/init_bare_repo.sh" "$SEED" "$WORK" "$BARE"
echo "BARE_URL=file://$BARE"
```

## 环境变量（⚠️ 必须双隔离）

```bash
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home"   # marketplace clone
export XDG_CONFIG_HOME="$U/config"      # settings.json（trust 决策落盘处，必须绝对路径）
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
```

> `marketplace add --trust` 会把信任决策写入 `$XDG_CONFIG_HOME/a2c/settings.json`。
> 只隔离 `A2C_SKILL_HOME` 不够——trust 列表会污染用户真实 `~/.config/a2c/settings.json`。
> 下文用例中 `$HOME_DIR` 即 `$A2C_SKILL_HOME`。测试完 `rm -rf "$U"` 清理。

## 测试用例

> 所有命令前缀 `A2C_SKILL_HOME=$HOME_DIR`；`$A2C` 为 CLI 二进制；`$BARE_URL` 为上方仓库 URL。

### M-01: marketplace add --trust（添加 marketplace）

- **优先级**: P0
- **步骤**:
  1. 干净的 `$HOME_DIR`
  2. 执行：`$A2C marketplace add $BARE_URL --name uat-seed-mp --trust --json`
  3. 捕获输出
- **预期结果**（字段名以 Rust 实际输出为准）:
  - 退出码 0
  - JSON 输出形如 `{"added": "uat-seed-mp", "skills": 1, "url": "file://…"}`
    - `added` = `"uat-seed-mp"`
    - `skills` = `1`（发现 1 个 SKILL）
    - `url` 含 `$BARE`
  - `$HOME_DIR/marketplace/uat-seed-mp/` 目录出现
  - `uat-seed-mp/` 下有 `.git/`（clone 成功）
  - 物化包根 `uat-seed-mp/plugins/foo/skills/valid-skill-pkg/SKILL.md` 存在

> 注：`add` 命令返回精简结果（`added`/`skills`/`url`）；完整字段（`trusted`/`commitSha`/
> `autoUpdate`/`lastUpdated`）见 `list`/`info`（M-02/M-03）。

### M-02: marketplace list（列出 marketplace）

- **优先级**: P0
- **前置**: M-01 成功
- **步骤**: `$A2C marketplace list --json`
- **预期结果**:
  - 退出码 0
  - JSON 输出为数组，长度 ≥ 1
  - 含元素 `name` = `"uat-seed-mp"`、`trusted` = `true`、`autoUpdate`（布尔）、`url`

### M-03: marketplace info（查看详情）

- **优先级**: P0
- **前置**: M-01 成功
- **步骤**: `$A2C marketplace info uat-seed-mp --json`
- **预期结果**:
  - 退出码 0
  - JSON 含非空字段（已对照 Rust 实际输出）：
    - `name` = `uat-seed-mp`
    - `url`（含 `$BARE`）
    - `commitSha`（40 字符 hex）
    - `autoUpdate`（布尔）
    - `trusted` = `true`
    - `installLocation`（本地 clone 路径，如 `/private/tmp/.../marketplace/uat-seed-mp`）
    - `installedPlugins` = `[]`（初始为空数组）
    - `lastUpdated`（ISO 8601 时间戳）

### M-04: marketplace refresh（刷新 marketplace）

- **优先级**: P0
- **前置**: M-01 成功
- **步骤**:
  1. 记录 M-03 中的 `commitSha`（`SHA_BEFORE`）
  2. `$A2C marketplace refresh uat-seed-mp --json`
  3. 再次 `info` 取 `commitSha`（`SHA_AFTER`）
- **预期结果**（已对照 Rust 实际输出）:
  - refresh 退出码 0
  - JSON 形如 `[{"name": "uat-seed-mp", "skills": 1, "status": "unchanged"}]`
    - `status` = `"unchanged"`（bare repo 无新提交，git pull 不报错）
  - `SHA_AFTER` = `SHA_BEFORE`（commitSha 不变）

### M-05: marketplace set auto-update（设置自动更新）

- **优先级**: P1
- **前置**: M-01 成功
- **步骤**: `$A2C marketplace set uat-seed-mp auto-update=true --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - JSON 形如 `{"autoUpdate": true, "name": "uat-seed-mp"}`
  - 持久化（再次 `info` 可见 `autoUpdate` = true）

### M-06: marketplace add（不带 --trust 应失败）

- **优先级**: P1
- **步骤**:
  1. 干净的 `$HOME_DIR`
  2. `$A2C marketplace add $BARE_URL --name uat-seed-mp --json`（无 --trust）
  3. 捕获输出（含 stderr）
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 1（非交互模式下必须 --trust）
  - JSON 形如 `{"error": "untrusted marketplace \"uat-seed-mp\" (...); pass --trust to confirm non-interactively"}`
    - 含 `untrusted` 与 `--trust` 关键词
  - `$HOME_DIR/marketplace/` 不出现（或不含 uat-seed-mp 目录）

### M-07: marketplace remove（删除 marketplace）

- **优先级**: P0
- **前置**: M-01 成功
- **步骤**:
  1. `$A2C marketplace remove uat-seed-mp --json`
  2. `$A2C marketplace list --json`
  3. 验证 clone 目录已不存在
- **预期结果**（已对照 Rust 实际输出）:
  - remove 退出码 0
  - JSON 形如 `{"removed": "uat-seed-mp", "pruned": ["uat-seed-mp"], "uninstalledPlugins": [], "keptPlugins": false}`
  - `list` 返回空数组 `[]`
  - `$HOME_DIR/marketplace/uat-seed-mp/` 已被清理

### M-08: marketplace add 重复添加

- **优先级**: P1
- **前置**: M-01 成功（且未 remove）
- **步骤**: 再次 `$A2C marketplace add $BARE_URL --name uat-seed-mp --trust --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 1
  - JSON 形如 `{"error": "marketplace name conflict: \"uat-seed-mp\" already exists"}`
    - 含 `already exists` 关键词
  - 原有 marketplace 数据不受影响（`list` 仍 1 条）

## 清理

```bash
rm -rf /tmp/a2c-uat-skill-home-* "$TMPROOT"
```

## 日志收集

CLI-only 场景下日志即命令 stdout/stderr。每个用例执行后保存完整输出。
