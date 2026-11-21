# 存量 Marketplace 注册名迁移（v0.5.1 / #289）

> 适用：0.5.1 之前注册的 marketplace 保留了**旧注册名**（URL 派生名或显式 `--name` 别名），与清单
> 顶层 `name` 不一致。相关：`marketplace add / refresh` 的身份语义见
> [Marketplace 身份模型](marketplace-identity.md)；协议 §5.14 把存量一致性交给本迁移。
>
> 设计细节见 [`docs/design-0.5.1-marketplace-name-migration.md`](../design-0.5.1-marketplace-name-migration.md)。

## 1. 什么时候需要迁移

- `marketplace list` 里的名字与你仓库 `marketplace.json` 顶层 `name` 不同；
- `marketplace refresh` 给出 `legacy_manifest_name_mismatch` 诊断（记录 baseline 但保留旧名）；
- 升级后看到「真正的改名迁移见 #289」提示。

新注册已经以清单名为准；本命令只处理**存量旧名**。**不会**自动运行，也不会静默改名。

## 2. 迁移前准备（重要）

1. **停止所有正在运行的 Computer / python 实例**。迁移是离线的文件级操作，不阻断其他进程写入；
   迁移命令只互斥另一个 migrate 进程。
2. 建议先跑一次 dry-run（默认行为）确认影响面：

   ```text
   marketplace migrate
   marketplace migrate --json     # 脚本可读
   ```

3. 迁移会自动把权威文件备份到 `<A2C_SKILL_HOME>/marketplace-migration-backup-<timestamp>/`
   （权限 0700/0600，不含 secret 值），成功后备份保留，便于回滚或人工核对。

## 3. 输出怎么看

| 列 | 含义 |
|----|------|
| `action` | `rename` 将迁移；`baseline` 只补写基线；`no-op` 已就绪；`unresolved` 离线/清单不可用，保留旧名；`blocked` 需人工处理 |
| `old -> new` | 旧注册名 → 清单名 |
| `source` | 脱敏后的来源 URL |
| `plugins` / `secrets` | 受影响 plugin 数与计划重键的 secret 数 |
| `reason` | unresolved/blocked 的原因（如 `catalog_missing` / `baseline_mismatch` / `keyring_unavailable`） |
| `manual` | project/local/policy 等**不会自动改写**的引用，含文件、JSON 路径与建议值 |

**冲突**（如两个旧名指向同一新名、新名已被其他来源占用、裸 plugin 名冲突）会让整次 `--apply`
**零写入中止**；先修复冲突再重试，v1 不提供跳过开关。

## 4. 执行迁移

```text
marketplace migrate --apply
# 同时改写 user scope 的 extraKnownMarketplaces：
marketplace migrate --apply --update-declarations
```

- 默认 dry-run；`--apply` 即确认，不需要额外 `--yes`。
- 退出码：`0` 成功/无操作；`1` 冲突或命令级错误；`3` 部分完成（有 `unresolved` / `blocked` /
  keyring 不可用 / project-local 启用项将失活）。
- `--update-declarations` **只改 user scope**；project/local 的声明永远只报告。
- 迁移完成后建议重启 Computer，让 SKILL registry / 已挂 server 按新名重建。

### 会改什么

- `known_marketplaces.json`：注册键、`installLocation`、`manifestNameBaseline`；
- `installed_plugins_intent.json`：`<plugin>@<旧名>` → `<plugin>@<新名>`；
- `installed_plugins.json`：pid 键与 `installPath`（多 scope / projectPath / version 保留）；
- user `settings.json`：`enabledPlugins`（含显式 `false`）、`trustedMarketplaces`、
  `blockedMarketplaces`；`extraKnownMarketplaces` 仅在 `--update-declarations` 时；
- `<A2C_SKILL_HOME>/marketplace/<旧名>` 与 `marketplace/.plugins/<旧名>` 目录改名；
- plugin-scoped **secret**：keyring 键 `<plugin>@<旧名>/<id>` → `<plugin>@<新名>/<id>`
  （先写新键、验证后删旧键；keyring 不可用且有 password input 时该 marketplace 整体跳过、零写入）。

### 不会改什么（需要你手动处理）

- **project / local scope** 的 `enabledPlugins` / `trustedMarketplaces` / `blockedMarketplaces` /
  `extraKnownMarketplaces`：输出里逐条列出文件与键路径。**旧 `enabledPlugins` 键在迁移后会失活**
  （报告标记 `will-be-inactive`，命令 exit 3）——请在对应项目的 `.tfrobot/settings.json` /
  `.tfrobot/settings.local.json` 中改成新键，或改用 user scope。
- **policy / flag scope** 的同名引用：迁移无法可靠定位这些来源（flag 路径由启动参数决定、policy 由系统管理），请自行核对并更新。
- **环境变量**：`A2C_SMCP_<ENV_SEGMENT>` 的变量名随 scoped id 变化（secret 与非 secret 都变），
  输出里给出 old/new 变量名；迁移无法修改宿主环境。
- 非 secret input 值：rust 已不落盘，无需迁移。

## 5. 离线 / 清单不可用

clone 缺失、清单缺失或不可读、清单名缺失/非法 → 该 marketplace 记为 `unresolved`，**保留旧名、
零写入**（grandfather）。修复网络/仓库后重新运行即可。已有 baseline 但清单又改名 → `blocked`
（与 `marketplace refresh` 的拒绝口径一致），需要上游确认或显式 remove/re-add（会丢 trust/安装
记录，慎用）。

## 6. 中断恢复与回滚

- 运行中断：会留下 `marketplace-migration-journal.json` 与备份。重新执行 `--apply` 会被拒绝并
  提示；用：

  ```text
  marketplace migrate --resume
  ```

  续做（按 journal 的旧名→新名映射，幂等；若期间文件被外部修改会拒绝）。
- 回滚到迁移前：

  ```text
  marketplace migrate --rollback <timestamp>
  ```

  `<timestamp>` 见成功输出或备份目录名。回滚会校验当前文件仍是迁移后的状态；若你迁移后又改过
  settings / 账本，会拒绝覆盖（`RollbackDrift`）。备份目录不会自动删除，确认无误后可手动清理。

## 7. 常见问题

| 现象 | 处理 |
|------|------|
| `exit 1` + `duplicate_target` | 两个旧名指向同一清单名：先 `marketplace remove` 掉不需要的那个（会级联卸载）或改上游清单名 |
| `target_name_conflict` / `target_dir_exists` | 新名已被其他来源占用：显式 remove 旧来源后重新 add，或先人工清理目标目录 |
| `extra_known_old_conflict` | user `extraKnownMarketplaces[旧名]` 的 source 与注册 source 不一致：先修正声明或改用 `--update-declarations` 前手工核对 |
| `bare_name_clash` | 同一裸 plugin 名已从其他 marketplace 安装：先卸载其中一个 |
| `install_path_unexpected` | 账本 `installPath` 不在预期目录（手编/异常迁移痕迹）：核对 `<A2C_SKILL_HOME>/marketplace` 后再处理，本命令不做部分改写 |
| `keyring_unavailable` | 当前环境没有可用的系统钥匙串：在带钥匙串的桌面会话中重试；不要用无 keyring 的 CI 环境迁移带 secret 的 marketplace |
| `exit 3` + `will-be-inactive` | 按第 4 节手动更新 project/local 的 `enabledPlugins` 键 |

> 迁移完成后，`marketplace refresh` 会以新 baseline 校验清单名；上游再次改名仍会被拒绝（协议
> §5.14），不会再静默漂移。
