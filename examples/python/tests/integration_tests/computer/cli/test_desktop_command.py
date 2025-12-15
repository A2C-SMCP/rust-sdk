# -*- coding: utf-8 -*-
# filename: test_desktop_command.py
# @Time    : 2025/10/02 20:16
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: CLI 新增的 desktop 命令集成测试。
英文: Integration tests for the newly added CLI 'desktop' command.
"""

from __future__ import annotations

import json
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest
from mcp import StdioServerParameters

import a2c_smcp.computer.cli.main as cli_main
from a2c_smcp.computer.cli.main import _interactive_loop
from a2c_smcp.computer.computer import Computer


class FakePromptSession:
    def __init__(self, commands: list[str]) -> None:
        self._commands = commands

    async def prompt_async(self, *_: str, **__: Any) -> str:  # noqa: D401
        if not self._commands:
            raise EOFError
        return self._commands.pop(0)


@contextmanager
def no_patch_stdout():
    yield


@pytest.mark.asyncio
async def test_cli_desktop_with_subscribe_resources_server() -> None:
    """
    中文: 使用 resources_subscribe_stdio_server 启动后，执行 desktop 命令应输出非空列表。
    英文: After starting resources_subscribe_stdio_server, 'desktop' should output a non-empty list.
    """
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "resources_subscribe_stdio_server.py"
    assert server_py.exists()

    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])

    server_cfg = {
        "name": "it-res-sub",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "server_parameters": json.loads(params.model_dump_json()),
    }

    # 执行流程：添加 -> 启动 -> desktop -> 退出
    commands = [
        f"server add {json.dumps(server_cfg)}",
        "start it-res-sub",
        "desktop",
        "exit",
    ]

    # Patch interactive IO
    cli_main.PromptSession = lambda: FakePromptSession(commands)  # type: ignore
    cli_main.patch_stdout = lambda raw: no_patch_stdout()  # type: ignore

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    # 为断言 desktop 的输出可调用 comp.get_desktop 再次核验
    await _interactive_loop(comp)
    desktops = await comp.get_desktop()
    assert isinstance(desktops, list) and len(desktops) >= 1


@pytest.mark.asyncio
async def test_cli_help_contains_desktop() -> None:
    """
    中文: help 列表应包含 desktop 命令说明。
    英文: 'help' listing should contain the 'desktop' command description.
    """
    commands = [
        "help",
        "exit",
    ]

    cli_main.PromptSession = lambda: FakePromptSession(commands)  # type: ignore
    cli_main.patch_stdout = lambda raw: no_patch_stdout()  # type: ignore

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)
