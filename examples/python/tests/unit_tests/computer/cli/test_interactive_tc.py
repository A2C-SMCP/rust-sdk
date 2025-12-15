# -*- coding: utf-8 -*-
# filename: test_interactive_tc.py
# @Time    : 2025/10/02 13:22
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
  中文: 覆盖 `interactive_impl.py` 新增的 `tc` 命令，校验 JSON 与 @file 两种输入、无 manager 保护分支、以及参数传递。
  English: Cover newly added `tc` command in `interactive_impl.py`: JSON and @file inputs, guard when no manager, and
    parameter forwarding.
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


class _FakeMgr:
    """中文: 覆盖最小化的 Manager 能力。English: Minimal MCP manager stub."""

    def __init__(self) -> None:  # noqa: D401
        self._validated: tuple[str, dict] | None = None
        self._called: tuple[str, str, dict, float | None] | None = None
        # 记录 get_tool_meta 调用 / record get_tool_meta calls
        self._get_meta_calls: list[tuple[str, str]] = []

    async def avalidate_tool_call(self, tool_name: str, params: dict) -> tuple[str, str]:  # noqa: D401
        # 回传 (server_name, tool_name)
        self._validated = (tool_name, params)
        return ("s1", tool_name)

    def get_server_config(self, name: str):  # noqa: D401
        class _Cfg:
            tool_meta = {}

        return _Cfg()

    # 新增: 兼容 Manager.get_tool_meta 新接口 / new API for merged tool meta
    def get_tool_meta(self, server_name: str, tool_name: str):  # noqa: D401
        # 记录调用并返回 None，表示无强制 auto_apply，由上层 confirm_callback 决定
        # Record invocation and return None so confirm_callback path is used
        self._get_meta_calls.append((server_name, tool_name))
        return None

    async def acall_tool(self, server: str, tool: str, params: dict, timeout: float | None):  # noqa: D401
        self._called = (server, tool, params, timeout)
        # 返回符合 CallToolResult 结构的简化对象

        class _Res:
            def __init__(self) -> None:
                self.isError = False
                self.content = [{"type": "text", "text": "ok"}]

            def model_dump(self, *, mode: str = "json") -> dict:  # noqa: D401
                return {"isError": self.isError, "content": self.content}

        return _Res()


@pytest.mark.asyncio
async def test_tc_json_calls_execute(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """
    中文: 通过 JSON 直接调用 tc，断言 comp.aexecute_tool 被正确调用并输出 JSON 结果。
    English: Use JSON to trigger tc and assert it forwards to comp.aexecute_tool and prints JSON result.
    """

    # 安装伪 session 与 stdout
    cmd = (
        'tc {"agent":"r","req_id":"r01","computer":"c","tool_name":"tool/x","params":{"a":1},"timeout":3}',
        "exit",
    )
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(list(cmd)))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    # 构建 Computer 与 Manager 桩
    comp = Computer(
        name="test_it_c",
        inputs=set(),
        mcp_servers=set(),
        auto_connect=False,
        auto_reconnect=False,
        # 中文: 允许调用通过，避免二次确认分支阻断执行
        # English: Approve calls to bypass confirm gate so acall_tool is hit
        confirm_callback=lambda req_id, server, tool, params: True,
    )
    mgr = _FakeMgr()
    comp.mcp_manager = mgr

    # 运行交互循环（将调用 tc -> aexecute_tool -> mgr.acall_tool）
    await _interactive_loop(comp)

    # 断言 manager 被调用，timeout 转换为 float
    assert mgr._called is not None
    server, tool, params, timeout = mgr._called
    assert server == "s1"
    assert tool == "tool/x"
    assert params == {"a": 1}
    assert isinstance(timeout, float) and timeout == 3.0
    # 新增断言：应调用 get_tool_meta 以决定 auto_apply 行为
    assert mgr._get_meta_calls == [("s1", "tool/x")]


@pytest.mark.asyncio
async def test_tc_from_file_and_no_manager_guard(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """
    - @file 路径触发 tc
    - 当 comp.mcp_manager 为 None 时，不应抛异常（走提示分支）
    """

    data = {
        "robot_id": "r",
        "req_id": "r02",
        "computer": "c",
        "tool_name": "tool/y",
        "params": {"b": 2},
        "timeout": 5,
    }
    f = tmp_path / "toolcall.json"
    f.write_text(json.dumps(data), encoding="utf-8")

    cmds = [f"tc @{f}", "exit"]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(cmds))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_it_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    # 不设置 manager，用于覆盖提示分支

    await _interactive_loop(comp)

    # 仅验证运行完成且未异常即可
    assert True
