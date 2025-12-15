# -*- coding: utf-8 -*-
# filename: test_interactive_tc.py
# @Time    : 2025/10/02 13:24
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
文件名: test_interactive_tc.py
作者: JQQ
创建日期: 2025/10/02
最后修改日期: 2025/10/02
版权: 2023 JQQ. All rights reserved.
依赖: pytest
描述:
  中文: 集成测试层面验证交互 CLI 的 `tc` 命令：使用 @file 输入，并在不依赖真实网络/真实 MCP 的前提下完成一次调用流程。
  English: Integration-level test for interactive CLI `tc` command using @file input without real network/MCP.
"""

from __future__ import annotations

import json
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest

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


class _Mgr:
    def __init__(self) -> None:  # noqa: D401
        self.called: dict | None = None

    async def avalidate_tool_call(self, tool_name: str, params: dict) -> tuple[str, str]:  # noqa: D401
        return ("s-it", tool_name)

    def get_server_config(self, name: str):  # noqa: D401
        class _Cfg:
            tool_meta = {}

        return _Cfg()

    async def acall_tool(self, server: str, tool: str, params: dict, timeout: float | None):  # noqa: D401
        self.called = {"server": server, "tool": tool, "params": params, "timeout": timeout}

        class _Res:
            def __init__(self) -> None:
                self.isError = False
                self.content = [{"type": "text", "text": "it-ok"}]

            def model_dump(self, *, mode: str = "json") -> dict:  # noqa: D401
                return {"isError": self.isError, "content": self.content}

        return _Res()


@pytest.mark.asyncio
async def test_tc_cmd_from_file_integration(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # 生成 @file JSON
    payload = {
        "robot_id": "r-it",
        "req_id": "req-it",
        "computer": "c",
        "tool_name": "tool/it",
        "params": {"x": 1},
        "timeout": 7,
    }
    f = tmp_path / "toolcall.json"
    f.write_text(json.dumps(payload), encoding="utf-8")

    commands = [f"tc @{f}", "exit"]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    comp.mcp_manager = _Mgr()

    await _interactive_loop(comp)
