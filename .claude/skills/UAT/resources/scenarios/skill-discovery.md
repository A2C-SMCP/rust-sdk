# 场景：skill-discovery（Rust SDK）

## 测试目标

验证 `smcp-computer skill` 子命令的多源 skill 发现能力：marketplace、user drop-in、MCP
三种来源的列出和详情查看；以及完整链路下的三级渐进披露（get_skills → get_skill → get_blob）。

## 类型

混合 — **D-01~D-04 为 CLI-only（已跑通）**，**D-05 为完整链路（需 Server+Computer+Agent）**

## 前置条件

1. CLI 已编译，`$A2C` 指向 `target/debug/smcp-computer`
2. 双隔离 env（`A2C_SKILL_HOME` + 绝对路径 `XDG_CONFIG_HOME`）

## 测试仓库 / 种子搭建

> **复用 seed**:
> - D-01/D-04: `seeds/marketplace/valid-single-plugin`（mp `uat-seed-mp`，skill `foo:valid-skill-pkg`）
> - D-02: `seeds/user/home-user-basic`（skill `valid-skill-pkg`，source=user）

```bash
A2C="$(pwd)/target/debug/smcp-computer"
SEEDS_ROOT=<项目根>/.claude/skills/UAT/resources/seeds
U="/tmp/a2c-uat-$$"
export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"

# marketplace seed
bash "$SEEDS_ROOT/marketplace/_helpers/init_bare_repo.sh" \
  "$SEEDS_ROOT/marketplace/valid-single-plugin" "$U/work" "$U/bare.git" >/dev/null
$A2C marketplace add "file://$U/bare.git" --name uat-seed-mp --trust --json

# user drop-in seed
mkdir -p "$A2C_SKILL_HOME/user/valid-skill-pkg"
cp -R "$SEEDS_ROOT/_common/valid-skill-pkg"/. "$A2C_SKILL_HOME/user/valid-skill-pkg/"
```

## 测试用例

### D-01: skill list --source mp（marketplace 技能）

- **优先级**: P0 / **前置**: marketplace seed 已添加
- **步骤**: `$A2C skill list --source mp --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - 数组含 `{"name": "foo:valid-skill-pkg", "source": "marketplace:uat-seed-mp", "enabled": true, "orphan": false}`

### D-02: skill list --source user（用户 drop-in 技能）

- **优先级**: P0 / **前置**: user seed 已 drop
- **步骤**: `$A2C skill list --source user --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - 数组含 `{"name": "valid-skill-pkg", "source": "user", "enabled": true, "orphan": false}`

### D-03: skill list --source mcp（MCP 技能）

- **优先级**: P1
- **步骤**: `$A2C skill list --source mcp --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - 非交互 CLI 模式下无活跃 MCP server 连接 → 返回空数组 `[]`

### D-04: skill info（查看单个技能详情）

- **优先级**: P0 / **前置**: D-01 成功
- **步骤**: `$A2C skill info foo:valid-skill-pkg --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 0
  - JSON 含 `name: "foo:valid-skill-pkg"`、`source`、`description`、`path`、`enabled`、
    `license: "MIT"`、`version`（marketplace 源的 git commit hash）、`allowed_tools`、`skill_metadata`

### D-05: 渐进披露（get_skills → get_skill → get_blob）⏳ 完整链路

- **优先级**: P0 / **类型**: 完整链路（需 Server + Computer + Agent 三进程）
- **状态**: ⏳ **待完整链路基础设施**（见下「完整链路待办」）
- **引用 seed**: `seeds/marketplace/valid-single-plugin` + `seeds/_helpers/skill-discovery`（Agent 驱动，待迁移）
- **预期行为**:
  - D-05-1: `client:get_skills` 返回 skill 引用列表（name/source/description）
  - D-05-2: `client:get_skill` 返回完整内容（小资源 inline，frontmatter 已剥离）
  - D-05-3: `client:get_skill` + rel_path 返回子资源（inline 或 blob handle）
  - D-05-4: A2CSkillRef 4 必选字段（name/source/path/description）契约满足
- **完整链路待办**:
  1. Computer 进程内注册 marketplace（同进程 `a2c> marketplace add` 后内存 registry 才有 skill；
     独立 CLI add 仅写盘）
  2. 按 `test-env-setup.md` 完整链路搭建 Server(`smcp-server-hyper`) + Computer + Agent
  3. Agent 驱动脚本（Rust 重写，或跨语言复用 python-sdk 的
     `_helpers/skill-discovery/agent_skill_driver.py` 连 Rust 进程）

## 清理

```bash
rm -rf "$U"
```

## 日志收集

D-01~D-04（CLI-only）：每用例保存 stdout/stderr。D-05：三端 capture-pane 各 ≥50 行。
