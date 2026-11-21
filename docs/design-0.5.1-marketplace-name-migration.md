# v0.5.1 设计：Marketplace 存量注册名迁移（#289）

> 协议依据：`a2c-smcp-protocol` `skill.md §1.2` + `runtime-contract §5.14`（协议 develop @ `a303f3e`）。
> 相关：rust-sdk#272（方向 A / `manifestNameBaseline`）、#289（本设计）、python-sdk#268（镜像）、
> protocol Discussion#81（双端对齐）、#284/#286/#294/#295（账本 / 恢复 / 换源）。
> 实现：`crates/smcp-computer/src/settings/marketplace_migration.rs`；CLI `marketplace migrate`。

## 1. 背景与目标

协议把 marketplace **注册名唯一性 / 禁止静默改名**收归管辖：新注册与 refresh 必须以清单顶层
`name` 为身份；**存量注册不追溯改名，由 SDK 迁移方案负责**（冲突检测、幂等与回滚）。

rust-sdk#272 已落地：`marketplace add` 以清单名注册、refresh 遇改名拒绝、账本新增
`manifestNameBaseline` 一等字段。但 0.5.1 之前的存量注册仍可能是 **URL 派生名**或显式 `--name`
别名，引用（`<plugin>@<old-name>`、trust、secret 等）都绑定旧名。没有迁移通道时，用户只能
`remove` + `add`：丢 trust、丢已装插件、丢 secret，release notes 已明确禁止把它当作无损迁移。

本设计提供一条**显式、默认 dry-run、冲突拒绝、幂等可续、可回滚**的迁移通道；不做 boot 自动迁移，
不静默改名。

## 2. 权威 / 派生分层

**权威（迁移改写；project/local 只报告（逐文件逐键）；flag/policy 由文档提示手动核对）**

| 面 | 位置 | 处理 |
|----|------|------|
| 安装意图 | `installed_plugins_intent.json` 的 `<plugin>@<mp>` | 改写 pid |
| 启用意图 | user settings `enabledPlugins` | 旧键值（含显式 `false`）复制到新键后删旧键 |
| 信任 / 阻断 | user settings `trustedMarketplaces` / `blockedMarketplaces` | 成员旧名→新名（去重、保留非字符串兄弟值） |
| 可选声明 | user settings `extraKnownMarketplaces` | 默认只报告；`--update-declarations` 才改 user scope |
| plugin secret | OS keyring `service=a2c-computer`，key `<plugin>@<mp>/<id>` | 旧键→新键（先写后删） |

`strictKnownMarketplaces` 是 bool（strict 模式），不按键名引用，无需迁移。非 secret input 值在 rust
D1 已退役明文存储，无迁移面；env 注入值（`A2C_SMCP_<ENV_SEGMENT>`）随 scoped id 变化，只能报告。

**派生（重建 / 定向改写）**

| 面 | 处理 |
|----|------|
| `known_marketplaces.json` | key old→new；`installLocation` 指向新目录；`manifestNameBaseline = new`；source / autoUpdate / commitSha / lastUpdated / 未知扩展键保留 |
| `installed_plugins.json` | pid 键改名；`installPath` 受控前缀改写（保留 scope / projectPath / version / installedAt / 未知字段） |
| `<home>/marketplace/<old>/` clone | 文件系统 rename 到新名 |
| `<home>/marketplace/.plugins/<old>/` 外部 plugin 树 | 文件系统 rename 到新名 |
| SKILL `A2CSkillRef.source = marketplace:<name>` | 运行期派生，不持久化，不迁移 |

## 3. 检测与分类

1. **严格只读预检**：不使用日常 `load_*`（损坏文件会被改名备份，产生写入）；新增
   `load_*_strict`。任一状态文件损坏 → 命令级 blocked（exit 3）、零写入、不产生 `.corrupt-*`。
2. 对 `known_marketplaces` 每条注册（按旧名排序，**不依赖 reload 顺序**）：
   - 旧名不适合做路径段（空 / `.` / `..` / 以 `.` 起 / 含 `/` `\` NUL）→ **unresolved**；
     clone 是 symlink → **unresolved**（不猜解析目标）。
   - 读 clone 清单顶层 `name`：
     - `manifest == registered`：baseline 缺失 → **BaselineOnly**（apply 补写）；baseline 相等 →
       **NoOp**；baseline 不等 → **Blocked(BaselineMismatch)**。
     - `manifest != registered` 且合法：baseline 存在且 ≠ 当前清单名 → **Blocked**（协议 refresh
       也会拒绝的改名，不隐式接受）；否则 **Rename** 候选。
     - 清单缺失 / 不可读 / name 缺失或非法 / clone 不在 → **Unresolved**（离线 grandfather：保留旧名、
       零写入、下次可重跑）。
3. **冲突预检**（任一 → 整体 abort、零写入、exit 1）：
   - 两个旧名映射同一新名（`duplicate_target`）；仅大小写不同（`case_only_rename`）；
   - 新名已注册给不同 / 不可判定 source（`target_name_conflict`，fail-closed）；
   - 新名 clone 目录 / 外部树已存在（`target_dir_exists` / `target_external_tree_exists`）；
   - 目标 pid `<plugin>@<new>` 已存在于 intent / ledger / user `enabledPlugins`（`target_pid_exists`）；
   - **裸名冲突**：目标 plugin 的裸名已被其他 marketplace 的已装 pid 占用（`bare_name_clash`）；
   - `extraKnownMarketplaces[<new>]` 已存在且不同源（仅 `--update-declarations`，`extra_known_target_conflict`）；
   - 账本任一非空 `installPath` 不在两条预期旧根下（`install_path_unexpected`，整体 unresolved，不部分改写）。
4. **同源目标合并**：新名已注册且 source 可证相同、目标目录不存在 → 不算冲突；迁移时以旧 entry
   为基底，目标独有键补齐，`autoUpdate` 取 OR，baseline / installLocation 重算。

## 4. 执行顺序（per marketplace，按旧名排序）

0. 持 `<home>/marketplace-migration.lock`（只互斥 migrate 进程）；`apply` 在锁内**重新检测 + 全量
   预检**（不接受调用方传入的 plan，消除 plan/apply 漂移）。
1. 备份到 `<home>/marketplace-migration-backup-<ts>/`（目录 0700、文件 0600）。备份保存四份权威
   文件的**原始字节**（`known_marketplaces.json` 的 source URL / user settings 内容会原样落盘，回滚
   依赖这些字节；**keyring secret 值不备份**），记录 **pre-hash**，写 journal（phase=prepared）。执行前复核当前
   hash == journal pre-hash，不一致 → `ConcurrentChange` abort。
2. **secret 重键（先于目录/文件改写）**：枚举清单内 plugin 的 `mcp-servers/inputs.json`（严格解析；
   畸形 / 已装 plugin 根不可读 → 保守 abort）。`password:true` inputs：
   - keyring 不可用 → **abort 该 marketplace、零写入**（无法证明旧 secret 不存在）；
   - 目标键已存在且值不同 → **abort**（不覆盖既有 secret）；
   - `get(old)` → `set(new)` → 回读校验；**中途失败删除本次已 set 的新键**；
   - 旧键在收尾（验证通过后）才删除。
3. FS rename：clone → 外部 plugin 树（同 FS 原子 rename；跨 FS / 异常 abort，不做 copy+delete）。
4. `known_marketplaces` RMW：key / `installLocation` / baseline（同源合并规则见 §3.4）。
5. `installed_plugins_intent.json` RMW：保序重建 `<plugin>@<old>` → `<plugin>@<new>`。
6. `installed_plugins.json` RMW：pid 键改名 + `installPath` 组件边界前缀改写（`Path::strip_prefix`，
   `foo` 不匹配 `foobar`）。
7. user settings 单次 raw RMW：`enabledPlugins` / `trustedMarketplaces` / `blockedMarketplaces`；
   `extraKnownMarketplaces` 仅 `--update-declarations`。
8. 验证（known / intent / ledger / 目录 / secret 回读）→ 删除旧 secret（失败仅告警）→ journal
   phase=done → 删除 journal，保留备份。

**失败处理**：当前 marketplace 按逐项 before-image + 反向 rename 回滚，已 set 的新 secret 删除；
已完成项保留；journal 留在 `aborted`，可 `--resume` 续做或 `--rollback` 整体还原。不做自动半回滚全部。

## 5. journal / 备份 / 恢复 / 回滚

- journal：`<home>/marketplace-migration-journal.json`（原子写；version / phase / backupDir /
  updateDeclarations / items[old,new,action,pids,secretKeys,status] / pre-hash / 最近 post-hash）。
  备份目录内 `backup.json` 同步一份，供回滚。
- `--apply` 遇残留 journal → 报错并提示 `--resume` / `--rollback`；`--resume` 以 **journal 的
  old/new 映射**驱动（崩溃后旧目录可能已不在，不重新按旧名检测），每步先复查磁盘成员关系；
  续做前校验当前文件 hash == pre-hash 或最近 post-hash，否则拒绝（并发修改）。
- `--rollback <timestamp>`：journal 处于 prepared/running 时拒绝（先 resume）；校验当前文件 hash ==
  迁移后 post-hash（未被迁移写过的文件 == pre-hash），不符 → `RollbackDrift` 不覆盖用户改动；
  反向 rename（外部树先、clone 后）+ 还原四份权威文件 + secret 反写（new→old，keyring 不可用则告警
  exit 3）。`--rollback` 对 prepared/applying 的中断态也可执行：跳过漂移校验、恢复预迁移备份并
  反向 rename（会丢弃迁移自身的半写；用户需在 resume/rollback 之间二选一，勿先行编辑）。
  备份永久保留，文档说明手动清理。

## 6. CLI / UX

```text
smcp-computer marketplace migrate [--apply] [--update-declarations] [--json]
                                  [--resume] [--rollback <timestamp>]
```

- 默认 dry-run（零写入）；`--apply` 写入（显式 flag 即确认，不额外 `--yes`）。
- 输出表：old name / new name / source（脱敏）/ action / plugins / secrets / reason；冲突与手动引用
  逐条列出（文件 + JSON pointer + 建议值）。
- `--json`：`{ dryRun, written, status, items[], conflicts[], manualReferences[], backup, corrupt }`；
  apply 报告另含 `warnings`。
- 退出码：`0` 成功 / 无操作；`1` 冲突或命令级错误（沿用 `EXIT_USER_ERROR`；提案原写 2 与既有
  `EXIT_NETWORK_ERROR` 语义冲突，本实现改为 1）；`3` 部分完成（unresolved / blocked /
  keyring-abort / project-local `enabledPlugins` 失活诊断，新增 `EXIT_PARTIAL`）。
- 名称展示一律经 `untrusted_name_for_display`，source 经脱敏助手；journal/备份保存权威文件
  **原字节**（可能含 source URL / settings 内容），权限 0700/0600，**不含 keyring secret 值**。

## 7. 安全前提与限制

- **离线迁移**：不 clone、不触网、不写 SKILL registry。建议**先停所有 Computer / python 实例**再
  `--apply`；迁移锁只互斥 migrate 进程，不阻断既有写入方（全局 home 锁另记 follow-up）。
- 迁移不改变 wire 行为，不触一致性向量；`marketplace refresh` 在迁移后按新 baseline 正常工作。
- 已知限制：project/local 的 `enabledPlugins` / `extraKnownMarketplaces` / 信任 / 阻断引用只报告
  不写；flag/policy 无法可靠定位，需手动核对；env 注入值需用户自行更新；不处理跨 FS、symlink clone、
  仅大小写改名（均保守 abort / unresolved）。

## 8. 与 python-sdk#268 的差异（Discussion#81 已留痕）

- python 采用 **forward-only**：无 journal / 备份 / `--rollback`，崩溃恢复靠状态自描述 + 重跑；
  rust 保留 journal / 备份 / 回滚。两端迁移语义（检测、冲突整体 abort、secret 重键、baseline、
  `extraKnown` 只报告 + `--update-declarations` 仅 user、CLI 默认 dry-run）一致。
- 存储形态差异：rust 安装意图在独立 `installed_plugins_intent.json`；账本 `installed_plugins.json`
  是派生缓存；secret 键空间两侧同为 `<plugin>@<mp>/<id>`。

## 9. 测试覆盖

- 库级：`crates/smcp-computer/tests/marketplace_name_migration.rs`（33 用例）——dry-run 零写、
  全量 apply、幂等、冲突四类、离线 grandfather、baseline mismatch、secret 重键 / keyring 不可用 /
  目标键冲突、installPath 越界、损坏状态零写、多 scope + 未知字段保真、外部树改名、同源合并、
  大小写冲突、rollback。
- 模块单测（13）：安全名 / 时间戳、installPath 组件边界、baseline-only、journal 阻断、aborted
  resume、rollback drift。
- CLI 级（10）：`crates/smcp-computer/tests/marketplace_name_migration_cli_test.rs`——默认 dry-run、
  apply + backup、冲突 exit 1、unresolved exit 3、resume 无 journal exit 1、rollback、help flags、
  user settings 隔离、cwd project 诊断（exit 3）。
