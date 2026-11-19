# Marketplace 身份模型（#272）：清单名、URL 派生名与期望名

> 协议依据：`a2c-smcp-protocol` `runtime-contract.md` §5.14 / `conformance-tests.md` §5。
> 相关：#272（本实现）、#289（存量迁移）、#294（reconcile 换源拒绝）。

## 1. 三种名称，别再混用

| 名称 | 来源 | 用途 | 现状 |
|------|------|------|------|
| **清单名（manifest name）** | `marketplace.json` 顶层 `name` | **唯一注册键**；`<plugin>@<name>` 与 `A2CSkillRef.source = marketplace:<name>` 都用它 | **新注册唯一权威** |
| **URL 派生名** | git URL 末段（去 `.git`、折叠为 kebab） | 仅存量兼容：0.5.1 之前注册的历史记录保留旧名 | legacy，不再用于新注册 |
| **期望名（`--name`）** | CLI 参数 / `AddMarketplaceParams.name` | **只做校验**：必须与清单名一致，否则报错 | 不覆盖、不派生 |

清单名必须是**严格 kebab-case**（`[a-z0-9-]`，1–64，不以 `-` 始末、无连续 `--`）。缺失或非法时
`marketplace add` **失败**，不会回退到 URL 派生名。

## 2. add 的行为

```text
marketplace add <git-url> [--name <expected>] [--trust] [--auto-update] [--no-clone]
```

1. 归一并校验 URL（`owner/repo` 简写展开为 GitHub URL）。
2. clone 到 `<home>/marketplace/.staging/<id>`，读取 `.tfrobot-plugin/marketplace.json` 顶层 `name`。
3. 若给了 `--name`：必须等于清单名，否则 `expected marketplace name ... does not match manifest name ...`。
4. **唯一性判定（注册提交点）**：
   - **同名同源** → 幂等成功：`marketplace "x" already registered (same source)`，不产生第二个注册。
   - **同名不同源** → 拒绝：`marketplace name conflict: "x" is already registered from a different source`。
     不替换、不自动加后缀；先 `marketplace remove` 旧源。
   - **同源已用旧名注册** → 拒绝：`this source is already registered as marketplace "old-name"`。
   - **同源无法判定**（如 `ssh://` 与 `https://` 等价写法）→ fail-closed 拒绝。
   - **remove 后残留 `<plugin>@<name>` 引用** → 拒绝，直到这些引用被显式移除。
5. 原子换入 `<home>/marketplace/<name>`，注册 SKILL，**成功后**才写入 `trustedMarketplaces`。
   任一步失败：账本 / 目录 / trust 零残留。

## 3. refresh 的行为

`marketplace refresh` 先在**不改变工作树**的前提下读取来料清单（`git fetch` +
`git show FETCH_HEAD:.tfrobot-plugin/marketplace.json`），再提交：

- 清单名与基线一致 → 正常更新（`updated` / `unchanged`）。
- 清单名变化 → 行状态 `rejected` + 结构化诊断；旧注册、旧内容与旧能力保持不变，**不进入 degraded**，
  既有 `<plugin>@<old-name>` 引用不被静默重写。
- legacy（无 `manifestNameBaseline`）首次成功 refresh → 记录基线（清单名）并保留旧名，给 advisory；
  之后再次改名才拒绝。
- legacy refresh 遇清单名缺失/非法 → 保留旧名 + 诊断，内容照常刷新。

## 4. 常见冲突示例

| 场景 | 结果 |
|------|------|
| 仓库 `team/plugins.git`，清单名 `team-tools` | 注册名为 `team-tools`（不是 `plugins`） |
| `marketplace add <url> --name other-name` | 失败：期望名与清单名不一致 |
| 两个不同仓库的清单都叫 `team-tools` | 第二个失败（同名不同源） |
| 同一仓库重复 add | 幂等成功，无第二份注册 |
| 同一仓库换个写法（ssh vs https）再 add | fail-closed 拒绝（无法证明同源） |
| 上游把清单 name 改成 `team-tools-v2` 后 refresh | `rejected`，旧名与旧内容保留 |
| legacy 注册（旧名 `plugins`）首次 refresh | 记录基线 `team-tools`，仍以 `plugins` 注册 |
| `marketplace remove <name> --keep-plugins` 后同名 **普通** re-add | 被拒：仍存在 `<plugin>@<name>` 引用（协议 §5.14 的 remove/re-add 守卫）。先逐插件 `plugin uninstall` 清除引用，或用默认（级联）`remove` |
| 账本丢失但 catalog 仍在，`--no-clone` 同源回填 | 允许：以 catalog 的 `origin` 证明同源，恢复的是既有绑定而非换源（`--keep-plugins` 残留引用不阻断这条恢复路径） |

## 5. 存量迁移

升级后建议对 legacy 源执行一次 `marketplace refresh` 建立基线（辅助后续检测）。**真正的改名迁移**
（把旧 URL 派生名统一为清单名、重写引用）见 #289；不要把 `remove` + `add` 当作无损迁移——旧
`<plugin>@<old-name>` 引用会失效。
