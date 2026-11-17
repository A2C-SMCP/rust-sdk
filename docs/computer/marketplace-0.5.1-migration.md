# 旧版本 → 0.5.1 升级迁移：插件 catalog 子树恢复

> 适用：在 **0.2.2–0.5.0**（0.5.1 之前已发布的全部版本）上卸载过「相对 source」插件的用户
> （升级 0.5.1 后重装仍失败）。
> 相关：#271（修复根因：卸载/GC 不再删除共享 catalog 树）、#278（本文档与 install 失败提示）。

## 1. 症状与原因

0.2.2–0.5.0 的 `uninstall` / `gc` 会无条件删除插件安装目录。相对 source 插件（marketplace 清单里
写成 `"./plugins/xxx"`）安装在 marketplace clone 目录内部，卸载会连带删除 catalog 子树；之后重装报：

```text
plugin root not found after source resolution
```

`marketplace refresh` **无法可靠自愈**：install 先物化再 stage，refresh 的 `git pull` 通常保留本地
已删除的文件；只有当 pull 失败、触发全量重克隆时才可能恢复。#271 起（0.5.1）卸载/GC 不再删除共享
catalog 子树，但**已被旧版删除的历史文件不会自动回来**。

影响面：只影响相对 source 插件；git 源插件安装在独立的 `<home>/marketplace/.plugins/<mp>/<plugin>`，
不受影响。

## 2. 恢复前确认（重要）

同一条错误也可能是**上游 manifest 失真**造成的：marketplace 仓库更新后把 `source` 指向了不存在的
`./plugins/xxx`，或你手工删过目录。这种情况 `remove` + `add` **不会修复**，只会白白丢掉该 marketplace
的 trust 与已装插件。

注意：本地 `<home>/marketplace/<mp>` 就是被破坏的 clone，直接看它会把「本地缺失」当成「上游缺失」。
请在 **marketplace 上游仓库**（远端提交树）确认该路径确实存在，例如：

```text
git ls-tree -r <commit> -- plugins/xxx
```

或在托管平台（GitHub/CNB 等）的 Web UI 查看对应分支。确认上游存在而本地缺失 → 按第 3 节恢复
（旧版卸载遗留）；上游本身已无该路径 → 属 manifest 问题，应更新 marketplace 仓库或修正 `source`，
不要 remove。

## 3. 恢复步骤

> ⚠️ **警告**：`marketplace remove <name>` **默认会级联卸载该 marketplace 下所有已安装插件，并撤销
> 该 marketplace 的 trust**。恢复后需要**逐一重装**这些插件，之前启用过的还要重新 `plugin enable`。
>
> **不要使用 `--keep-plugins`**——它只跳过级联卸载，仍会删除 clone 并撤销 trust，留下孤儿账本记录。

0. 先记下恢复所需信息（`remove` 后 `marketplace info` 查不到 URL；展示值会脱敏凭据，含 token 的 URL
   请从你自己的记录取。`plugin list` 默认只显示已启用插件，须加 `--available` 才含 installed-disabled）：

   ```text
   marketplace info <name>
   plugin list --available
   plugin info <plugin>@<name> --json
   ```

   记录：**源 URL**、**注册名**、该 marketplace 下**已安装插件清单**（含每个插件的 **scope**、
   **projectPath**、**version** 与是否启用）、marketplace 的 **autoUpdate** 旗标。
   `projectPath` / `version` / `commitSha` 只出现在 `plugin info --json` 的 `records[]` 里（文本模式
   不打印 `projectPath`）；原安装用过 `--version` 锁定的，务必记下该值。

1. 移除已被破坏的 marketplace（重新 clone 前必须做）：

   ```text
   marketplace remove <name>
   ```

2. 重新添加并重新 clone，同时重新授予 trust：

   ```text
   marketplace add <git-url> --trust --name <name>
   ```

   - **必须带 `--name <name>` 沿用原注册名**：不指定时新名会从 URL 派生，与旧名不一致的
     `plugin install <plugin>@<name>` 会报 not added。
   - 非交互 / 脚本方式必须带 `--trust`；REPL 交互模式会弹 y/N 确认。
   - 原来开启过 auto-update 的，加 `--auto-update`（或事后 `marketplace set <name> auto-update=true`）。

3. 逐一按**原 scope** 重装插件；此前启用过的再执行 enable：

   ```text
   plugin install <plugin>@<name> --scope <scope> [--version <version>]
   plugin enable <plugin>@<name>
   ```

   - 未指定 `--scope` 时为 user。
   - **多 scope 的插件要对每个 scope 各执行一次 `plugin install`**（一次只装一个 scope）。
   - **project / local scope 必须以原项目目录为 cwd 执行**：CLI 把当前工作目录记为 `projectPath`，
     不在原目录执行会装到错误路径。
   - 原安装用 `--version` 锁定过的，按第 0 步记录的值回填 `--version`；否则会装到 marketplace 当前版本。

## 4. 验证

```text
plugin info <plugin>@<name>
skill list
```

`plugin info` 能看到安装记录、`skill list` 能看到该插件的技能，即恢复完成。

## 5. 注意事项

- 旧版卸载只删**该插件自己的 installPath**（即 catalog clone 内的那棵子树），不删整个
  marketplace clone 或 `.plugins/<mp>`；而本文的 `marketplace remove` **会** prune 掉整个
  `<home>/marketplace/<mp>` 与 `.plugins/<mp>` 树（因此必须走重新 clone）。
- 一次性 CLI 的 `marketplace remove` 只改账本，**不会停/摘除任何 bundled MCP server**；摘除延迟到下次
  REPL 启动。建议在重启或普通安装场景执行恢复，避免与运行中会话交叉。
- 恢复影响该 marketplace 下**所有**已安装插件（级联卸载，需逐一重装，见警告）；其他 marketplace
  与不受影响的插件不受影响。

## 6. Release Notes

发版时的迁移提示（GitHub Release 正文）见 [`docs/releases/v0.5.1.md`](../releases/v0.5.1.md)。
本仓库 `CHANGELOG.md` 由 git-cliff 全量重生成，手写段落会被覆盖，故迁移提示独立成文件。
