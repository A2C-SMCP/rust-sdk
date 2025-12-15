"""
文件名: interactive_impl.py
作者: JQQ
创建日期: 2025/9/25
最后修改日期: 2025/9/25
版权: 2023 JQQ. All rights reserved.
依赖: prompt_toolkit, rich
描述:
  中文: CLI 交互循环的实现模块，支持依赖注入（会话、stdout 补丁、SMCP 客户端）。
  English: Implementation module for CLI interactive loop with DI (session, stdout patch, SMCP client).
"""

from __future__ import annotations

import json
from contextlib import AbstractContextManager
from pathlib import Path
from typing import Any, Protocol

from prompt_toolkit import PromptSession
from pydantic import TypeAdapter
from rich.table import Table

from a2c_smcp.computer.cli.utils import console, parse_kv_pairs, print_mcp_config, print_status, print_tools
from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import MCPServerInput as MCPServerInputModel
from a2c_smcp.smcp import MCPServerConfig as SMCPServerConfigDict
from a2c_smcp.smcp import MCPServerInput as SMCPServerInputDict
from a2c_smcp.smcp import ToolCallReq as SMCPToolCallReq

# 定义上下文管理器类型
ContextManager = AbstractContextManager[None]


# 定义上下文管理器工厂函数的协议
class PatchStdoutCtx(Protocol):
    def __call__(self, *, raw: bool = False) -> ContextManager: ...


class _Session(Protocol):
    async def prompt_async(self, *_: str, **__: Any) -> str: ...


async def interactive_loop(
    comp: Computer,
    *,
    session_factory: type[PromptSession],
    patch_stdout_ctx: PatchStdoutCtx,
    smcp_client_cls: type[Any],
    init_client: Any | None = None,
) -> None:
    """
    中文: 交互循环的可注入实现；从 main.py 传入 PromptSession 工厂、patch_stdout 上下文与 SMCP 客户端类。
    English: DI-friendly interactive loop; main.py passes PromptSession factory, patch_stdout ctx and SMCP client class.
    """
    session = session_factory()
    smcp_client = init_client

    console.print("[bold]进入交互模式，输入 help 查看命令 / Enter interactive mode, type 'help' for commands[/bold]")
    if not console.is_terminal and not console.no_color:
        console.print(
            "[yellow]检测到控制台可能不支持 ANSI 颜色。若在 PyCharm 中运行，请在 Run/Debug 配置中启用 'Emulate terminal in "
            "output console'；或者使用 --no-color 关闭彩色输出。[/yellow]",
        )

    while True:
        try:
            with patch_stdout_ctx(raw=True):
                raw = (await session.prompt_async("a2c> ")).strip()
        except (EOFError, KeyboardInterrupt):
            console.print("\n[cyan]Bye[/cyan]")
            break

        if raw == "":
            continue

        if raw.lower() in {"help", "?"}:
            help_table = Table(
                title="可用命令 / Commands",
                header_style="bold magenta",
                show_lines=False,
                expand=False,
            )
            help_table.add_column("Command", style="bold cyan", no_wrap=True)
            help_table.add_column("Description", style="white")

            help_table.add_row("status", "查看服务器状态 / show server status")
            help_table.add_row("tools", "列出可用工具 / list tools")
            help_table.add_row("mcp", "显示当前 MCP 配置 / show current MCP config")
            help_table.add_row("server add <json|@file>", "添加或更新 MCP 配置 / add or update config")
            help_table.add_row("server rm <name>", "移除 MCP 配置 / remove config")
            help_table.add_row("start <name>|all", "启动客户端 / start client(s)")
            help_table.add_row("stop <name>|all", "停止客户端 / stop client(s)")
            help_table.add_row("inputs load <@file>", "从文件加载 inputs 定义 / load inputs")
            help_table.add_row("inputs list", "查看当前inputs的定义 / show inputs")
            # 中文: 新增当前 inputs 值的增删改查命令
            # English: Add CRUD commands for current inputs values
            help_table.add_row("inputs value list", "列出当前 inputs 的缓存值 / list current cached input values")
            help_table.add_row("inputs value get <id>", "获取指定 id 的值 / get cached value by id")
            help_table.add_row(
                "inputs value set <id> [<json|text>]", "设置指定 id 的值（省略值则用 default）/ set cached value (omit to use default)"
            )
            help_table.add_row("inputs value rm <id>", "删除指定 id 的值 / remove cached value by id")
            help_table.add_row("inputs value clear [<id>]", "清空全部或指定 id 的缓存 / clear all or one cached value")
            help_table.add_row("tc <json|@file>", "使用与 Socket.IO 一致的 JSON 结构调试工具 / debug tool with Socket.IO-compatible JSON")
            help_table.add_row(
                "desktop [size] [window_uri]",
                "获取当前桌面窗口组合（可选限制条数与指定URI） / get current desktop (optional size and URI)",
            )
            help_table.add_row("history [n]", "显示最近的工具调用历史（默认最多10条）/ show recent tool call history (default up to 10)")
            help_table.add_row(
                "socket connect [<url>]",
                "连接 Socket.IO（省略 URL 将进入引导式输入） / connect to Socket.IO (guided if URL omitted)",
            )
            help_table.add_row("socket join <office_id> <computer_name>", "加入房间 / join office")
            help_table.add_row("socket leave", "离开房间 / leave office")
            help_table.add_row("notify update", "触发配置更新通知 / emit config updated notification")
            help_table.add_row("render <json|@file>", "测试渲染（占位符解析） / test rendering (placeholders)")
            help_table.add_row("quit | exit", "退出 / quit")

            console.print(help_table)
            console.print("[dim]提示: 输入命令后按回车执行；输入 'help' 或 '?' 重新查看命令列表。[/dim]")
            continue

        parts = raw.split()
        cmd = parts[0].lower()

        try:
            if cmd in {"quit", "exit"}:
                break

            elif cmd == "status":
                print_status(comp)

            elif cmd == "tools":
                tools = await comp.aget_available_tools()
                print_tools(tools)

            elif cmd == "mcp":
                servers: dict[str, dict] = {}
                for cfg in comp.mcp_servers:
                    servers[cfg.name] = json.loads(json.dumps(cfg.model_dump(mode="json")))
                inputs = [json.loads(json.dumps(i.model_dump(mode="json"))) for i in comp.inputs]
                print_mcp_config({"servers": servers, "inputs": inputs})

            elif cmd == "desktop":
                # 中文: 解析可选参数：size 与 window_uri（顺序不固定，数字视为 size，其它作为 URI）
                # English: Parse optional args: size and window_uri (order-agnostic; digits -> size, else -> URI)
                size: int | None = None
                window_uri: str | None = None
                for arg in parts[1:]:
                    if size is None and arg.isdigit():
                        try:
                            size = int(arg)
                        except Exception:
                            size = None
                    elif window_uri is None:
                        window_uri = arg

                try:
                    desktops = await comp.get_desktop(size=size, window_uri=window_uri)
                    # 直接以 JSON 输出，便于上层消费 / print as JSON for easy consumption
                    console.print_json(data=desktops)
                except Exception as e:
                    console.print(f"[red]获取桌面失败 / Failed to get desktop: {e}[/red]")

            elif cmd == "server" and len(parts) >= 2:
                sub = parts[1].lower()
                payload = raw.split(" ", 2)[2] if len(parts) >= 3 else ""
                if sub == "add":
                    if payload.startswith("@"):
                        data = json.loads(Path(payload[1:]).read_text(encoding="utf-8"))
                    else:
                        data = json.loads(payload)
                    validated: dict[str, Any] = TypeAdapter(SMCPServerConfigDict).validate_python(data)
                    try:
                        await comp.aadd_or_aupdate_server(validated, session=session)
                        console.print("[green]✅ 服务器配置已添加/更新并正在启动 / Server config added/updated and starting[/green]")
                        if smcp_client:
                            await smcp_client.emit_update_config()
                    except Exception as e:
                        console.print(f"[red]❌ 添加/更新服务器失败 / Failed to add/update server: {e}[/red]")
                elif sub in {"rm", "remove"}:
                    if len(parts) < 3:
                        console.print("[yellow]用法: server rm <name>[/yellow]")
                    else:
                        await comp.aremove_server(parts[2])
                        console.print("[green]已移除配置 / Removed[/green]")
                        if smcp_client:
                            await smcp_client.emit_update_config()
                else:
                    console.print("[yellow]未知的 server 子命令 / Unknown subcommand[/yellow]")

            elif cmd == "start" and len(parts) >= 2:
                target = parts[1]
                if not comp.mcp_manager:
                    console.print("[yellow]Manager 未初始化[/yellow]")
                else:
                    try:
                        if target == "all":
                            await comp.mcp_manager.astart_all()
                            console.print("[green]✅ 所有服务器启动完成 / All servers started[/green]")
                        else:
                            await comp.mcp_manager.astart_client(target)
                            console.print(f"[green]✅ 服务器 '{target}' 启动完成 / Server '{target}' started[/green]")
                    except Exception as e:
                        console.print(f"[red]❌ 启动服务器失败 / Failed to start server: {e}[/red]")

            elif cmd == "stop" and len(parts) >= 2:
                target = parts[1]
                if not comp.mcp_manager:
                    console.print("[yellow]Manager 未初始化[/yellow]")
                else:
                    try:
                        if target == "all":
                            await comp.mcp_manager.astop_all()
                            console.print("[green]✅ 所有服务器停止完成 / All servers stopped[/green]")
                        else:
                            await comp.mcp_manager.astop_client(target)
                            console.print(f"[green]✅ 服务器 '{target}' 停止完成 / Server '{target}' stopped[/green]")
                    except Exception as e:
                        console.print(f"[red]❌ 停止服务器失败 / Failed to stop server: {e}[/red]")

            elif cmd == "inputs" and len(parts) >= 2:
                sub = parts[1].lower()
                if sub == "load":
                    if len(parts) < 3 or not parts[2].startswith("@"):
                        console.print("[yellow]用法: inputs load @file.json[/yellow]")
                    else:
                        data = json.loads(Path(parts[2][1:]).read_text(encoding="utf-8"))
                        raw_items = TypeAdapter(list[SMCPServerInputDict]).validate_python(data)
                        models: set[MCPServerInputModel] = {TypeAdapter(MCPServerInputModel).validate_python(item) for item in raw_items}
                        comp.update_inputs(models)
                        console.print("[green]Inputs 已更新 / Inputs updated[/green]")
                        if smcp_client:
                            await smcp_client.emit_update_config()
                elif sub == "add":
                    if len(parts) < 3:
                        console.print("[yellow]用法: inputs add <json|@file.json>[/yellow]")
                    else:
                        payload = raw.split(" ", 2)[2]
                        if payload.startswith("@"):  # 文件里可为单个或数组
                            data = json.loads(Path(payload[1:]).read_text(encoding="utf-8"))
                        else:
                            data = json.loads(payload)
                        if isinstance(data, list):
                            items = TypeAdapter(list[SMCPServerInputDict]).validate_python(data)
                            for item in items:
                                comp.add_or_update_input(TypeAdapter(MCPServerInputModel).validate_python(item))
                        else:
                            item = TypeAdapter(SMCPServerInputDict).validate_python(data)
                            comp.add_or_update_input(TypeAdapter(MCPServerInputModel).validate_python(item))
                        console.print("[green]Input(s) 已添加/更新 / Added/Updated[/green]")
                        if smcp_client:
                            await smcp_client.emit_update_config()
                elif sub in {"update"}:
                    if len(parts) < 3:
                        console.print("[yellow]用法: inputs update <json|@file.json>[/yellow]")
                    else:
                        payload = raw.split(" ", 2)[2]
                        if payload.startswith("@"):  # 文件里可为单个或数组
                            data = json.loads(Path(payload[1:]).read_text(encoding="utf-8"))
                        else:
                            data = json.loads(payload)
                        if isinstance(data, list):
                            items = TypeAdapter(list[SMCPServerInputDict]).validate_python(data)
                            for item in items:
                                comp.add_or_update_input(TypeAdapter(SMCPServerInputDict).validate_python(item))
                        else:
                            item = TypeAdapter(SMCPServerInputDict).validate_python(data)
                            comp.add_or_update_input(item)
                        console.print("[green]Input(s) 已添加/更新 / Added/Updated[/green]")
                        if smcp_client:
                            await smcp_client.emit_update_config()
                elif sub in {"rm", "remove"}:
                    if len(parts) < 3:
                        console.print("[yellow]用法: inputs rm <id>[/yellow]")
                    else:
                        ok = comp.remove_input(parts[2])
                        if ok:
                            console.print("[green]已移除 / Removed[/green]")
                            if smcp_client:
                                await smcp_client.emit_update_config()
                        else:
                            console.print("[yellow]不存在的 id / Not found[/yellow]")
                elif sub == "get":
                    if len(parts) < 3:
                        console.print("[yellow]用法: inputs get <id>[/yellow]")
                    else:
                        input_get_items = comp.get_input(parts[2])
                        if input_get_items is None:
                            console.print("[yellow]不存在的 id / Not found[/yellow]")
                        else:
                            console.print_json(data=input_get_items.model_dump(mode="json"))
                elif sub == "list":
                    input_items = [i.model_dump(mode="json") for i in comp.inputs]
                    console.print_json(data=input_items)
                elif sub == "value":
                    if len(parts) < 3:
                        console.print("[yellow]用法: inputs value <list|get|set|rm|clear> ...[/yellow]")
                    else:
                        vsub = parts[2].lower()
                        if vsub == "list":
                            values = comp.list_input_values()
                            console.print_json(data=values or {})
                        elif vsub == "get":
                            if len(parts) < 4:
                                console.print("[yellow]用法: inputs value get <id>[/yellow]")
                            else:
                                val = comp.get_input_value(parts[3])
                                if val is None:
                                    console.print("[yellow]未找到或尚未解析 / Not found or not resolved yet[/yellow]")
                                else:
                                    try:
                                        console.print_json(data=val)
                                    except Exception:
                                        console.print(repr(val))
                        elif vsub == "set":
                            if len(parts) < 4:
                                console.print("[yellow]用法: inputs value set <id> [<json|text>][/yellow]")
                                console.print("[dim]提示: 如果不提供值，将使用 default 值 / Hint: omit value to use default[/dim]")
                            else:
                                target_id = parts[3]
                                # 中文: 如果只有 4 个部分（inputs value set <id>），则尝试使用 default 值
                                # English: If only 4 parts (inputs value set <id>), try to use default value
                                if len(parts) == 4:
                                    # 获取 input 定义 / Get input definition
                                    input_def = comp.get_input(target_id)
                                    if input_def is None:
                                        console.print("[yellow]不存在的 id / Not found[/yellow]")
                                        continue
                                    from a2c_smcp.computer.mcp_clients.model import MCPServerCommandInput

                                    if isinstance(input_def, MCPServerCommandInput):
                                        console.print(
                                            f"[yellow]Input '{target_id}' 是 command 类型，不支持 default 值 / is command type, "
                                            f"no default support[/yellow]"
                                        )
                                        console.print("[dim]用法: inputs value set <id> <json|text>[/dim]")
                                        continue
                                    if input_def.default is None:
                                        console.print(f"[yellow]Input '{target_id}' 没有 default 值 / has no default value[/yellow]")
                                        console.print("[dim]用法: inputs value set <id> <json|text>[/dim]")
                                        continue
                                    data = input_def.default
                                    console.print(f"[dim]使用 default 值 / Using default value: {repr(data)}[/dim]")
                                else:
                                    payload = raw.split(" ", 4)[4]
                                    try:
                                        data = json.loads(payload)
                                    except Exception:
                                        data = payload
                                ok = comp.set_input_value(target_id, data)
                                if ok:
                                    console.print("[green]已设置 / Set[/green]")
                                else:
                                    console.print("[yellow]不存在的 id / Not found[/yellow]")
                        elif vsub in {"rm", "remove"}:
                            if len(parts) < 4:
                                console.print("[yellow]用法: inputs value rm <id>[/yellow]")
                            else:
                                ok = comp.remove_input_value(parts[3])
                                console.print("[green]已删除 / Removed[/green]" if ok else "[yellow]无此缓存 / No such cache[/yellow]")
                        elif vsub == "clear":
                            target_id = parts[3] if len(parts) >= 4 else None
                            comp.clear_input_values(target_id)
                            console.print("[green]缓存已清理 / Cache cleared[/green]")
                        else:
                            console.print("[yellow]未知的 inputs value 子命令 / Unknown subcommand[/yellow]")
                else:
                    console.print("[yellow]未知的 inputs 子命令 / Unknown subcommand[/yellow]")

            elif cmd == "socket" and len(parts) >= 2:
                sub = parts[1].lower()
                if sub == "connect":
                    if smcp_client and getattr(smcp_client, "connected", False):
                        console.print("[yellow]已连接 / Already connected[/yellow]")
                    else:
                        url_val: str | None = parts[2] if len(parts) >= 3 else None
                        if not url_val:
                            with patch_stdout_ctx(raw=True):
                                url_val = (await session.prompt_async("URL: ")).strip()
                        if not url_val:
                            console.print("[yellow]URL 不能为空 / URL required[/yellow]")
                            continue

                        if len(parts) < 3:
                            with patch_stdout_ctx(raw=True):
                                auth_str = (await session.prompt_async("Auth (key:value, 可留空): ")).strip()
                            with patch_stdout_ctx(raw=True):
                                headers_str = (await session.prompt_async("Headers (key:value, 可留空): ")).strip()
                        else:
                            auth_str = ""
                            headers_str = ""

                        try:
                            auth = parse_kv_pairs(auth_str)
                            headers = parse_kv_pairs(headers_str)
                        except Exception as e:
                            console.print(f"[red]参数解析失败 / Parse error: {e}[/red]")
                            continue

                        smcp_client = smcp_client_cls(computer=comp)
                        await smcp_client.connect(url_val, auth=auth, headers=headers)
                        console.print("[green]已连接 / Connected[/green]")
                elif sub == "join":
                    if not smcp_client or not getattr(smcp_client, "connected", False):
                        console.print("[yellow]请先连接 / Connect first[/yellow]")
                    elif len(parts) < 4:
                        console.print("[yellow]用法: socket join <office_id> <computer_name>[/yellow]")
                    else:
                        # 如果指定了computer_name 会动态地修改运行时computer的name
                        comp.name = parts[3]
                        try:
                            await smcp_client.join_office(parts[2])
                            console.print("[green]已加入房间 / Joined office[/green]")
                        except RuntimeError as e:
                            console.print(f"[red]{e}[/red]")
                        except Exception as e:
                            console.print(f"[red]加入房间失败 / Failed to join office: {e}[/red]")
                elif sub == "leave":
                    if not smcp_client or not getattr(smcp_client, "connected", False):
                        console.print("[yellow]未连接 / Not connected[/yellow]")
                    elif not getattr(smcp_client, "office_id", None):
                        console.print("[yellow]未加入房间 / Not in any office[/yellow]")
                    else:
                        await smcp_client.leave_office(smcp_client.office_id)
                        console.print("[green]已离开房间 / Left office[/green]")
                else:
                    console.print("[yellow]未知的 socket 子命令 / Unknown subcommand[/yellow]")

            elif cmd == "notify" and len(parts) >= 2:
                sub = parts[1].lower()
                if sub == "update":
                    if not smcp_client:
                        console.print("[yellow]未连接 Socket.IO，已跳过 / Not connected, skip[/yellow]")
                    else:
                        await smcp_client.emit_update_config()
                        console.print("[green]已触发配置更新通知 / Update notification emitted[/green]")
                else:
                    console.print("[yellow]未知的 notify 子命令 / Unknown subcommand[/yellow]")

            elif cmd == "render":
                payload = raw.split(" ", 1)[1] if len(parts) >= 2 else ""
                if payload.startswith("@"):
                    data = json.loads(Path(payload[1:]).read_text(encoding="utf-8"))
                else:
                    data = json.loads(payload)
                rendered = await comp._config_render.arender(
                    data,
                    lambda x: comp._input_resolver.aresolve_by_id(x, session=session),
                )
                console.print_json(data=rendered)

            elif cmd == "tc":
                # 中文: 工具调用调试命令，参数需为与 Socket.IO 请求一致的 JSON（参见 a2c_smcp/smcp.py 的 ToolCallReq）
                # English: Tool call debug command. Argument must be a JSON matching Socket.IO request (see ToolCallReq in a2c_smcp/smcp.py)
                if len(parts) < 2:
                    console.print("[yellow]用法: tc <json|@file.json>[/yellow]")
                    continue
                payload = raw.split(" ", 1)[1]
                try:
                    if payload.startswith("@"):
                        data = json.loads(Path(payload[1:]).read_text(encoding="utf-8"))
                    else:
                        data = json.loads(payload)

                    # 中文: 使用 TypedDict 校验与规范化请求结构
                    # English: Validate and normalize request using TypedDict
                    req = TypeAdapter(SMCPToolCallReq).validate_python(data)

                    # 前置检查：需要已有活跃的 MCP 管理器
                    # Pre-check: require active MCP manager
                    if not comp.mcp_manager:
                        console.print(
                            "[yellow]MCP 管理器未初始化。请先添加并启动服务器 (server add/start) / MCP manager not initialized."
                            " Add and start a server first.[/yellow]",
                        )
                        continue

                    # 从请求中提取字段并调用
                    # Extract fields and execute
                    req_id: str = req["req_id"]
                    tool_name: str = req["tool_name"]
                    parameters: dict = req.get("params", {}) or {}
                    # ToolCallReq.timeout 定义为 int（秒）。转为 float 传入底层以兼容。
                    # ToolCallReq.timeout defined as int (seconds). Convert to float.
                    timeout_val = req.get("timeout")
                    timeout: float | None = float(timeout_val) if isinstance(timeout_val, (int, float)) else None

                    result = await comp.aexecute_tool(req_id, tool_name, parameters, timeout)

                    # 结果输出：优先以 JSON 打印
                    # Output result: prefer JSON
                    try:
                        if hasattr(result, "model_dump"):
                            console.print_json(data=result.model_dump(mode="json"))
                        else:
                            # 尝试通用序列化
                            console.print_json(data=json.loads(json.dumps(result, default=lambda o: getattr(o, "__dict__", str(o)))))
                    except Exception:
                        console.print(repr(result))
                except Exception as e:
                    console.print(f"[red]❌ 工具调用失败 / Tool call failed: {e}[/red]")

            elif cmd == "history":
                # 中文: 显示最近的调用历史。可选参数 n 指定返回条数，默认显示全部（最多10条）。
                # English: Show recent call history. Optional n limits number of returned entries (default all, up to 10).
                try:
                    n: int | None = None
                    if len(parts) >= 2:
                        try:
                            n = int(parts[1])
                        except Exception:
                            n = None
                    history = await comp.aget_tool_call_history()
                    tc_items = list(history)[-n:] if n is not None and n > 0 else list(history)
                    console.print_json(data=tc_items)
                except Exception as e:  # pragma: no cover
                    console.print(f"[red]❌ 读取历史失败 / Failed to read history: {e}[/red]")

            else:
                console.print("[yellow]未知命令 / Unknown command[/yellow]")
        except Exception as e:  # pragma: no cover
            console.print(f"[red]执行失败 / Failed: {e}[/red]")
