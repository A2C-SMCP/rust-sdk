# 场景：strict-mode（Rust SDK）

## 测试目标

验证 marketplace strict 模式：strict=true 时 entry.skills + plugin.json.skills 追加合并；
strict=false 时 marketplace entry 是唯一组件权威（plugin.json 不得声明组件字段）；
strict=false + plugin.json 声明组件字段时冲突降级（plugin 跳过，marketplace 仍添加）。

## 类型

CLI-only（strict 模式在 `marketplace add` 时解析，非运行时）

## 前置条件

1. CLI 已编译，`$A2C` 指向 `target/debug/smcp-computer`
2. 双隔离 env（`A2C_SKILL_HOME` + 绝对路径 `XDG_CONFIG_HOME`）

## strict 模式核心语义

> 协议依据：a2c-smcp-protocol marketplace/loading-behavior；Rust 实现见
> `crates/smcp-computer/src/skills/manifest.rs`（冲突检测 `assert_no_conflict` /
> 字段 commands/agents/hooks/skills/mcpServers/lspServers）。

| 场景 | 行为 |
|---|---|
| 只有 `<plugin>/skills/` 约定目录 | 自动发现（始终扫描，不受 strict 影响） |
| entry.skills + plugin.json 存在 + strict=true（默认） | entry.skills + plugin.json.skills **追加合并** |
| entry.skills + plugin.json 存在 + strict=false + plugin.json 声明组件 | **冲突降级**：plugin 跳过，marketplace 仍添加 |
| entry.skills + plugin.json 无组件 + strict=false | 仅取 entry.skills |

> **关键路径**: plugin.json 位于 `<plugin>/.tfrobot-plugin/plugin.json`。

## 测试仓库搭建

> **复用 seed**（3 个 marketplace 工作树）:
> - `seeds/marketplace/strict-true-merge`
> - `seeds/marketplace/strict-false-clean`
> - `seeds/marketplace/strict-false-conflict`

```bash
A2C="$(pwd)/target/debug/smcp-computer"
SEEDS_ROOT=<项目根>/.claude/skills/UAT/resources/seeds

# ⚠️ 必须拆成两步：env export 不能放进 $() 子shell（否则不传播到父shell，
#    会回退到真实用户目录并污染 ~/.config/a2c）。
iso(){ # 直接调用（不要用 $()）：每条用例独立隔离目录
  U="/tmp/a2c-uat-$1-$$"
  export A2C_SKILL_HOME="$U/skill-home" XDG_CONFIG_HOME="$U/config"
  mkdir -p "$A2C_SKILL_HOME" "$XDG_CONFIG_HOME"
}
build(){ # 可在 $() 中用：仅 echo bare URL，不动 env
  bash "$SEEDS_ROOT/marketplace/_helpers/init_bare_repo.sh" \
    "$SEEDS_ROOT/marketplace/$1" "$U/work" "$U/bare.git" >/dev/null
  echo "file://$U/bare.git"
}
# 用法：  iso strict-true-merge;  BARE=$(build strict-true-merge)
```

## 测试用例

### S-01: strict=true 追加合并（entry.skills + plugin.json.skills）

- **优先级**: P0
- **步骤**:
  1. `iso strict-true-merge; BARE=$(build strict-true-merge)`
  2. `$A2C marketplace add "$BARE" --name strict-true-merge --trust --json`
  3. `$A2C skill list --source mp --json`
- **预期结果**（已对照 Rust 实际输出）:
  - add 退出码 0，输出 `skills: 3`
  - skill list 显示 3 个 skill（均 `enabled:true`、`orphan:false`、
    `source:"marketplace:strict-true-merge"`）：
    - `audit:greet`（来自默认 `skills/`，始终扫描）
    - `audit:review`（来自 entry.skills 指定的 `extra-skills/`）
    - `audit:scan`（来自 plugin.json.skills 指定的 `more-skills/`）

### S-02: strict=false + plugin.json 无组件 → 仅取 entry.skills

- **优先级**: P0
- **步骤**:
  1. `iso strict-false-clean; BARE=$(build strict-false-clean)`
  2. `$A2C marketplace add "$BARE" --name strict-false-clean --trust --json`
  3. `$A2C skill list --source mp --json`
- **预期结果**（已对照 Rust 实际输出）:
  - add 退出码 0，输出 `skills: 2`
  - skill list 显示 2 个 skill：`audit:greet`、`audit:review`
  - 无 `scan`（plugin.json 为 `{}`，不提供额外目录）

### S-03: strict=false + plugin.json 组件冲突降级

- **优先级**: P0
- **步骤**:
  1. `iso strict-false-conflict; BARE=$(build strict-false-conflict)`
  2. `$A2C marketplace add "$BARE" --name strict-false-conflict --trust --json`（分离 stdout/stderr）
  3. `$A2C skill list --source mp --json`
  4. `$A2C marketplace info strict-false-conflict --json`
- **预期结果**（已对照 Rust 实际输出）:
  - 退出码 **0**（降级处理，非硬错误）
  - add 输出 `skills: 0`（冲突 plugin 被跳过，不入册）
  - `skill list --source mp` 返回 `[]`
  - marketplace 仍被添加（`marketplace list` 可见），`installedPlugins` 为 `[]`

> ⚠️ **与 python-sdk 的行为差异（已验证）**：Python 在冲突时向 **stderr** 输出
> `conflicting manifests ... strict=false ... plugin.json declares components` 警告；
> **Rust 当前静默降级，stderr 为空**。故本用例断言改为基于可观测结果（skills=0 +
> marketplace 仍添加 + installedPlugins 空），不断言 stderr 文案。
>
> 这是潜在的 UX 缺口（Rust 宜补一条降级提示），可作为后续 SUT 改进项；当前 UAT
> 以协议要求的「降级语义正确」为通过标准。

## 清理

```bash
rm -rf /tmp/a2c-uat-strict-* /tmp/a2c-uat-*
```

## 日志收集

CLI-only 场景下日志即命令 stdout/stderr。每个用例执行后保存完整输出。
