"""
文件名: utils.py
作者: JQQ
创建日期: 2025/9/25
最后修改日期: 2025/9/25
版权: 2023 JQQ. All rights reserved.
依赖: rich
描述:
  中文: CLI 层通用工具：键值解析与表格打印，统一 Console 管理。
  English: Common CLI utilities: key-value parsing and table printers with unified Console management.
"""

from __future__ import annotations

import importlib
import json
from typing import Any

from rich.table import Table

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.utils import console as console_util
from a2c_smcp.smcp import SMCPTool

# 使用全局 Console（引用模块属性，便于后续动态切换）
# Use a global Console (module attribute reference for dynamic switching)
console = console_util.console


def resolve_import_target(target: str) -> Any:
    """
    中文:
      解析命令行传入的导入目标字符串，返回对应对象（函数/类/可调用等）。

    English:
      Resolve an import target string from CLI into the referenced object (function/class/callable).

    允许的导入路径格式 Allowed formats:
      1) module.submodule:attr
         - 使用冒号分隔模块与属性；attr 可继续包含 "." 以访问多级属性。
      2) module.submodule.attr
         - 不含冒号时，视为最后一个点号分隔模块与属性。

    相对路径规则 Relative path rules:
      - 不支持以 "." 开头的相对导入（例如 ".mymod:factory"）。
      - 传入的模块路径按照 Python 的导入系统进行解析，起始于当前工作目录的可导入包环境。
        换言之，相对路径应转换为可导入的包名（确保含有 __init__.py），并从运行 a2c-computer 的工作目录可被 sys.path 找到。

    例如 Examples:
      - "my_pkg.my_mod:build_computer"
      - "my_pkg.my_mod.MyComputerSubclass"
      - "pkg.sub.mod:factories.computer_factory"

    Raises:
      ValueError: 当字符串没有包含有效的模块与属性分隔时，或以相对导入开头时。
      ModuleNotFoundError/AttributeError: 导入失败时抛出。
    """
    module_name = None
    attr_path = None
    if ":" in target:
        module_name, _, attr_path = target.partition(":")
    else:
        # 用最后一个点号拆分模块与属性
        if "." not in target:
            raise ValueError(f"无效的导入目标: {target!r}，需要形如 'pkg.mod:attr' 或 'pkg.mod.attr'")
        module_name, _, attr_path = target.rpartition(".")

    if not module_name or not attr_path or module_name.startswith("."):
        raise ValueError(
            f"无效的导入目标: {target!r}，不支持相对导入，必须提供完整模块路径",
        )

    module = importlib.import_module(module_name)
    obj: Any = module
    for part in attr_path.split("."):
        obj = getattr(obj, part)
    return obj


def parse_kv_pairs(text: str | None) -> dict[str, Any] | None:
    """
    中文: 将形如 "k1:v1,k2:v2" 的字符串解析为 dict；容错处理空格。
    English: Parse a string like "k1:v1,k2:v2" into a dict; tolerant to spaces.

    Args:
        text: 原始输入字符串；None 或空字符串时返回 None。

    Returns:
        dict 或 None / dict or None
    """
    if text is None:
        return None
    s = text.strip()
    if s == "":
        return None
    # 优先尝试 JSON 反序列化：支持直接传入合法的 JSON 对象字符串
    # Try JSON deserialization first: support passing a valid JSON object string directly
    try:
        parsed = json.loads(s)
    except Exception:
        pass
    else:
        if isinstance(parsed, dict):
            return parsed
        # 合法 JSON 但不是对象（如数组/字符串/数字），给出明确错误
        raise ValueError('JSON 字符串必须是对象类型（例如 {"k":"v"}）')
    result: dict[str, Any] = {}
    for seg in s.split(","):
        seg = seg.strip()
        if seg == "":
            continue
        if ":" not in seg:
            raise ValueError(f"无效的键值对: {seg}，应为 key:value 形式")
        k, v = seg.split(":", 1)
        k = k.strip()
        v = v.strip()
        if not k:
            raise ValueError(f"无效的键名: '{seg}'")
        result[k] = v
    return result if result else None


def print_status(comp: Computer) -> None:
    """
    中文: 打印系统状态，包括 MCP 服务器状态和 SocketIO 连接状态。
    English: Print system status including MCP servers and SocketIO connection.
    """
    # 中文: 打印 SocketIO 连接状态 / English: Print SocketIO connection status
    client = comp.socketio_client
    if client:
        # 中文: 已连接，展示详细信息 / English: Connected, show details
        conn_table = Table(title="SocketIO 连接状态 / SocketIO Connection Status", show_header=True)
        conn_table.add_column("属性 / Property", style="cyan", no_wrap=True)
        conn_table.add_column("值 / Value", style="green")

        # 连接状态
        is_connected = client.connected
        conn_table.add_row(
            "连接状态 / Connected",
            "[green]✓ 已连接 / Connected[/green]" if is_connected else "[red]✗ 未连接 / Disconnected[/red]",
        )

        # 连接 URL
        # 中文: 从 EngineIO 层获取连接 URL / English: Get connection URL from EngineIO layer
        eio = getattr(client, "eio", None)
        if eio:
            base_url = getattr(eio, "base_url", "N/A")
        else:
            base_url = "N/A"
        conn_table.add_row("Server URL", str(base_url))

        # 客户端 ID (EngineIO SID)
        # 中文: 客户端总 ID，区别于各 Namespace 的 SID / English: Client ID (EngineIO level), different from namespace SIDs
        client_sid = getattr(eio, "sid", "N/A") if eio else "N/A"
        conn_table.add_row("Client ID", str(client_sid))

        # Office ID (房间)
        office_id = getattr(client, "office_id", None)
        conn_table.add_row(
            "Office ID (房间)",
            str(office_id) if office_id else "[dim]未加入 / Not joined[/dim]",
        )

        console.print(conn_table)
        console.print()  # 空行分隔

        # 中文: 打印所有已连接的 Namespace 信息 / English: Print all connected namespaces info
        if client.namespaces:
            ns_table = Table(title="Namespace 连接信息 / Namespace Connections", show_header=True)
            ns_table.add_column("Namespace", style="cyan", no_wrap=True)
            ns_table.add_column("SID", style="yellow")

            for namespace, sid in client.namespaces.items():
                ns_table.add_row(namespace, str(sid))

            console.print(ns_table)
            console.print()  # 空行分隔
        else:
            console.print("[dim]暂无 Namespace 连接 / No namespace connected[/dim]")
            console.print()
    else:
        # 中文: 未连接 / English: Not connected
        console.print("[yellow]SocketIO: 目前尚未连接 / Not connected yet[/yellow]")
        console.print()

    # 中文: 打印 MCP 服务器状态 / English: Print MCP servers status
    if not comp.mcp_manager:
        console.print("[yellow]MCP Manager 未初始化 / MCP Manager not initialized[/yellow]")
        return

    rows = comp.mcp_manager.get_server_status()
    table = Table(title="MCP 服务器状态 / MCP Servers Status")
    table.add_column("Name", style="cyan")
    table.add_column("Active", style="magenta")
    table.add_column("State", style="yellow")
    for name, active, state in rows:
        table.add_row(name, "[green]✓[/green]" if active else "[red]✗[/red]", state)
    console.print(table)


def print_tools(tools: list[SMCPTool]) -> None:
    """
    中文: 打印工具列表。
    English: Print tools list.
    """
    table = Table(title="工具列表 / Tools")
    table.add_column("Name")
    table.add_column("Description")
    table.add_column("Json Return")
    for t in tools:
        table.add_row(t.get("name", ""), (t.get("description") or "")[:80], "Yes" if t.get("return_schema") else "No")
    console.print(table)


def print_mcp_config(config: dict[str, Any]) -> None:
    """
    中文: 打印当前 MCP 配置（servers 与 inputs）。
    English: Print current MCP config (servers and inputs).
    """
    servers = config.get("servers") or {}
    inputs = config.get("inputs") or []
    console.print("[bold]Servers:[/bold]")
    s_table = Table()
    s_table.add_column("Name")
    s_table.add_column("Type")
    s_table.add_column("Disabled")
    for name, cfg in servers.items():
        s_table.add_row(name, cfg.get("type", ""), "Yes" if cfg.get("disabled") else "No")
    console.print(s_table)

    console.print("[bold]Inputs:[/bold]")
    i_table = Table()
    i_table.add_column("ID")
    i_table.add_column("Type")
    i_table.add_column("Description")
    for i in inputs:
        i_table.add_row(i.get("id", ""), i.get("type", ""), (i.get("description") or "")[:60])
    console.print(i_table)
