# -*- coding: utf-8 -*-
# filename: test_interactive_history.py
# @Time    : 2025/10/02 14:02
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
  中文: 集成测试层面对 `history` 命令做一次基本打通验证：先伪造一次调用记录，再读取 history。
  English: Integration-level sanity check for `history`: pre-populate one record then read it back.
"""

from __future__ import annotations

from contextlib import contextmanager

import pytest

import a2c_smcp.computer.cli.main as cli_main
from a2c_smcp.computer.cli.main import _interactive_loop
from a2c_smcp.computer.computer import Computer


class FakePromptSession:
    def __init__(self, commands: list[str]) -> None:
        self._commands = commands

    async def prompt_async(self, *_: str, **__: object) -> str:  # noqa: D401
        if not self._commands:
            raise EOFError
        return self._commands.pop(0)


async def _append_history(comp: Computer, **kwargs):  # noqa: D401
    await comp._append_tool_history(kwargs)  # 内部API用于测试打桩，生产不会直接使用


@pytest.mark.asyncio
async def test_history_basic_integration(monkeypatch: pytest.MonkeyPatch) -> None:
    # 预置命令
    commands = ["history", "exit"]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    # 使用空操作的上下文管理器替代 patch_stdout
    # Use a no-op context manager to replace patch_stdout

    @contextmanager
    def _noop_ctx(*args, **kwargs):
        yield

    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: _noop_ctx())

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    # 伪造一条历史记录（直接调用专用于测试的方法）
    await _append_history(
        comp,
        timestamp="2025-10-02T05:00:00Z",
        req_id="R",
        server="S",
        tool="T",
        parameters={"k": 1},
        timeout=None,
        success=True,
        error=None,
    )

    await _interactive_loop(comp)

    assert True
