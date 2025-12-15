# -*- coding: utf-8 -*-
# 文件名: test_inputs_value_cli.py
# 作者: JQQ
# 创建日期: 2025/9/24
# 最后修改日期: 2025/9/24
# 版权: 2023 JQQ. All rights reserved.
# 依赖: pytest
# 描述:
#   中文: 覆盖 CLI 中 `inputs value` 子命令的基本增删改查流程。
#   English: Cover basic CRUD flow for `inputs value` subcommands in CLI.

from __future__ import annotations

from contextlib import contextmanager
from typing import Any

import pytest

import a2c_smcp.computer.cli.main as cli_main
from a2c_smcp.computer.cli.main import _interactive_loop
from a2c_smcp.computer.computer import Computer


class FakePromptSession:
    """中文: 将脚本化命令注入交互循环。
    English: Feed scripted inputs to the interactive loop.
    """

    def __init__(self, commands: list[str]) -> None:
        self._commands = commands

    async def prompt_async(self, *_: str, **__: Any) -> str:  # noqa: D401
        if not self._commands:
            raise EOFError
        return self._commands.pop(0)


@contextmanager
def no_patch_stdout():
    """No-op context manager to replace patch_stdout() in tests."""
    yield


@pytest.mark.asyncio
async def test_inputs_value_crud_commands(monkeypatch: pytest.MonkeyPatch) -> None:
    commands = [
        # 初始查看应为空 / list should be empty initially
        "inputs value list",
        # 先添加定义 / add a definition first
        'inputs add {"id":"K","type":"promptString","description":"d"}',
        # 设置、读取与列表 / set, get and list
        'inputs value set K "abc"',
        "inputs value get K",
        "inputs value list",
        # 删除指定键 / remove specific key
        "inputs value rm K",
        "inputs value list",
        # 清空（当为空时也应可调用）/ clear even if empty
        "inputs value clear",
        # 退出 / exit
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_input_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)
