# 0.5.0 → 0.5.1 升级迁移：插件 catalog 子树恢复

> 适用：在 **0.5.0** 上卸载过「相对 source」插件的用户（升级 0.5.1 后重装仍失败）。
> 相关：#271（修复根因：卸载/GC 不再删除共享 catalog 树）、#278（本文档与 install 失败提示）。

## 1. 症状与原因

0.5.0 的 `uninstall` / `gc` 会直接删除插件安装目录。相对 source 插件（marketplace 清单里写成
`"./plugins/xxx"`）安装在 marketplace clone 目录内部，卸载会连带删除 marketplace 目录树；之后重装报：

```text
plugin root not found after source resolution
```

`marketplace refresh` **不能自愈**：install 先物化再 stage，refresh 的 `git pull` 会保留本地已删除的
文件，只有重新 clone 才能恢复。#271 起（0.5.1）卸载/GC 不再删除共享 catalog 子树，但**已被旧版删除
的历史文件不会自动回来**。

影响面：只影响相对 source 插件；git 源插件安装在独立的 `<home>/marketplace/.plugins/<mp>/<plugin>`，
不受影响。

## 2. 恢复步骤

> ⚠️ **警告**：`marketplace remove <name>` **默认会级联卸载该 marketplace 下所有已安装插件，并撤销
> 该 marketplace 的 trust**。恢复后需要重新安装插件，之前启用过的还要重新 `plugin enable`。
>
> **不要使用 `--keep-plugins`**——它只跳过级联卸载，仍会删除 clone 并撤销 trust，留下孤儿账本记录。

1. 移除已被破坏的 marketplace（重新 clone 前必须做）：

   ```text
   marketplace remove <name>
   ```

2. 重新添加并重新 clone，同时重新授予 trust：

   ```text
   marketplace add <git-url> --trust
   ```

   - 非交互 / 脚本方式必须带 `--trust`；REPL 交互模式会弹 y/N 确认。
   - 如需沿用原来的注册名，加 `--name <name>`。

3. 重装插件（以及此前启用过的插件，再执行 enable）：

   ```text
   plugin install <plugin>@<name>
   plugin enable <plugin>@<name>
   ```

## 3. 验证

```text
plugin info <plugin>@<name>
skill list
```

`plugin info` 能看到安装记录、`skill list` 能看到该插件的技能，即恢复完成。

## 4. 注意事项

- 一次性 CLI 的 `marketplace remove` 只清理账本，不会 detach 正在运行的 REPL 中的 live MCP server；
  建议在重启或普通安装场景执行恢复，避免与运行中会话交叉。
- 恢复只针对受影响的 marketplace，不影响其他 marketplace 与未受影响的插件。

## 5. Release Notes

发版时的迁移提示（GitHub Release 正文）见 [`docs/releases/v0.5.1.md`](../releases/v0.5.1.md)。
本仓库 `CHANGELOG.md` 由 git-cliff 全量重生成，手写段落会被覆盖，故迁移提示独立成文件。
