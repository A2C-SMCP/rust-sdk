# -*- coding: utf-8 -*-
"""
文件名: test_interactive_more.py
作者: JQQ
创建日期: 2025/9/29
最后修改日期: 2025/9/29
版权: 2023 JQQ. All rights reserved.
依赖: pytest
描述:
  中文: 覆盖 `interactive_impl.py` 剩余分支：tools、server @file、socket 已连接/未加入、inputs 变体与 inputs value JSON 文本两类。
  English: Cover remaining branches in `interactive_impl.py`: tools, server @file, socket already-connected/not-joined,
    inputs variants, and inputs value JSON vs text.
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
    """中文: 将脚本化命令注入交互循环。English: Feed scripted inputs to interactive loop."""

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


class _Client:
    """中文: 覆盖 socket 分支需要的最小客户端桩对象。English: Minimal client stub for socket branches."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:  # noqa: D401
        self.connected = False
        self.office_id: str | None = None
        self.updated = 0
        _Client.last = self  # type: ignore[attr-defined]

    async def connect(self, url: str, auth: dict | None = None, headers: dict | None = None) -> None:  # noqa: D401
        self.connected = True
        self._url = url
        self._auth = auth
        self._headers = headers

    async def join_office(self, office_id: str, computer_name: str) -> None:  # noqa: D401
        assert self.connected
        self.office_id = office_id
        self._comp = computer_name

    async def leave_office(self, office_id: str) -> None:  # noqa: D401
        assert self.connected
        self.office_id = None

    async def emit_update_config(self) -> None:  # noqa: D401
        self.updated += 1


@pytest.mark.asyncio
async def test_tools_and_inputs_update_single_and_list(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """
    - tools: 覆盖 aget_available_tools 分支
    - inputs add @file (数组) 与 update (单条)
    - inputs get 不存在与存在
    - inputs value set: JSON 与 纯文本 两类
    """
    monkeypatch.setattr(cli_main, "SMCPComputerClient", _Client)

    # 通过文件提供 inputs 数组
    inputs_file = tmp_path / "inputs.json"
    inputs_file.write_text(
        json.dumps(
            [
                {"id": "A", "type": "promptString", "description": "d1", "default": "x"},
                {"id": "B", "type": "pickString", "description": "d2", "options": ["1", "2"], "default": "1"},
            ],
        ),
        encoding="utf-8",
    )

    # 服务器配置文件（用于 server add @file 覆盖）
    server_file = tmp_path / "server.json"
    server_file.write_text(
        json.dumps(
            {
                "name": "s1",
                "type": "stdio",
                "disabled": True,
                "forbidden_tools": [],
                "tool_meta": {},
                "server_parameters": {
                    "command": "echo",
                    "args": [],
                    "env": None,
                    "cwd": None,
                    "encoding": "utf-8",
                    "encoding_error_handler": "strict",
                },
            },
        ),
        encoding="utf-8",
    )

    # 提前连接一次以覆盖 "已连接" 分支
    pre_connect = [
        "socket connect http://localhost:9999",
        "tools",
        # inputs add @file (数组)
        f"inputs add @{inputs_file}",
        # get 不存在
        "inputs get NOT_EXIST",
        # update 单条（覆盖 update 的非数组路径）
        'inputs update {"id":"A","type":"promptString","description":"dx","default":"y"}',
        # value set JSON
        'inputs value set A {"k":1}',
        # value set 文本
        "inputs value set A ptext",
        # server add @file 与通知
        f"server add @{server_file}",
        # socket 已连接提示
        "socket connect http://localhost:9999",
        # leave 未加入
        "socket leave",
        "exit",
    ]

    # Monkeypatch 输入与 patch_stdout
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(pre_connect))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    # 注入 tools 的桩实现
    comp = Computer(name="test_im_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    async def _fake_tools() -> list[dict[str, Any]]:
        return [{"name": "t1", "description": "desc", "return_schema": {}}]

    monkeypatch.setattr(comp, "aget_available_tools", _fake_tools)

    await _interactive_loop(comp)

    # 断言 server add 时触发了配置更新
    last: _Client = _Client.last  # type: ignore[assignment]
    assert last.updated >= 1


@pytest.mark.asyncio
async def test_server_rm_and_start_stop_single_with_errors(monkeypatch: pytest.MonkeyPatch) -> None:
    """
    - 覆盖 server rm <name> 的 happy path
    - 覆盖 start/stop <name> 的异常路径（触发 except 分支）
    """
    monkeypatch.setattr(cli_main, "SMCPComputerClient", _Client)

    commands = [
        # 初始化 manager 后再进入交互
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_im_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await comp.boot_up()

    # 准备第二段命令：rm + start/stop 单个 + 异常
    cmds2 = [
        "server rm not-exist",  # aremove_server 安静返回，不影响覆盖
        "start one",
        "stop one",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(cmds2))

    # 注入 manager 的异常行为
    class BadMgr(type(comp.mcp_manager)):  # type: ignore[misc]
        async def astart_client(self, name: str) -> None:  # type: ignore[override]
            raise RuntimeError("boom-start")

        async def astop_client(self, name: str) -> None:  # type: ignore[override]
            raise RuntimeError("boom-stop")

    comp.mcp_manager.__class__ = BadMgr  # type: ignore[attr-defined]

    await _interactive_loop(comp)
