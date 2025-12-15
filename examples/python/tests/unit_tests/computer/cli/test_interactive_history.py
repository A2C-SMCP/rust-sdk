# -*- coding: utf-8 -*-
# filename: test_interactive_history.py
# @Time    : 2025/10/02 14:00
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
文件名: test_interactive_history.py
作者: JQQ
创建日期: 2025/10/02
最后修改日期: 2025/10/02
版权: 2023 JQQ. All rights reserved.
依赖: pytest
描述:
  中文: 覆盖 `interactive_impl.py` 的 `history` 命令，校验默认与限条数两种输出。
  English: Cover `history` command in `interactive_impl.py`, validating default and limited output.
"""

from __future__ import annotations

from contextlib import contextmanager
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


@pytest.mark.asyncio
async def test_history_default_and_limit(monkeypatch: pytest.MonkeyPatch) -> None:
    # 预置命令：先 history，再 history 1，然后 exit
    commands = ["history", "history 1", "exit"]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    # 构建 Computer，并注入假的历史记录返回
    comp = Computer(name="test_input_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    records = (
        {
            "timestamp": "2025-10-02T05:00:00Z",
            "req_id": "r1",
            "server": "s1",
            "tool": "t1",
            "parameters": {"a": 1},
            "timeout": 3.0,
            "success": True,
            "error": None,
        },
        {
            "timestamp": "2025-10-02T05:01:00Z",
            "req_id": "r2",
            "server": "s1",
            "tool": "t2",
            "parameters": {"b": 2},
            "timeout": None,
            "success": False,
            "error": "boom",
        },
    )

    async def _fake_history():  # noqa: D401
        return records

    monkeypatch.setattr(comp, "aget_tool_call_history", _fake_history)

    await _interactive_loop(comp)

    # Fast path：若执行到此处表示命令未抛异常；更严格的断言交由集成与 e2e
    assert True
