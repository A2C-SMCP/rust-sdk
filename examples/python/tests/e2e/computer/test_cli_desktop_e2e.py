# -*- coding: utf-8 -*-
# filename: test_cli_desktop_e2e.py
# @Time    : 2025/10/02 20:22
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: e2e 测试：多 MCP Server 窗口 + 桌面顺序与工具调用顺序相关。
英文: E2E test: multiple MCP Servers windows + desktop order affected by recent tool calls.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


def _server_cfg_for_script(name: str, script_rel_path: str) -> dict[str, Any]:
    return {
        "name": name,
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": {
            "auto_apply": True,
        },
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [script_rel_path],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }


@pytest.mark.e2e
def test_desktop_order_respects_recent_tool_calls(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    场景：
    1) 启动两个支持 window:// 的 stdio MCP Servers：A 与 B。
       - A: tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py
       - B: tests/integration_tests/computer/mcp_servers/resources_subscribe_b_stdio_server.py（含工具 mark_b）
    2) 通过 tc 调用 B 的 mark_b 工具，形成“最近调用服务=B”。
    3) 执行 desktop 命令，期望窗口顺序：优先 B（按 priority 降序），然后 A（遇到 fullscreen 只取第一个）。
    """
    child = cli_proc

    # 1) 添加两个服务器配置（相对路径，子进程 cwd 已是项目根）
    cfg_a = _server_cfg_for_script(
        "e2e-desktop-A",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    cfg_b = _server_cfg_for_script(
        "e2e-desktop-B",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_b_stdio_server.py",
    )

    child.sendline(f"server add {json.dumps(cfg_a, ensure_ascii=False)}")
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)
    child.sendline(f"server add {json.dumps(cfg_b, ensure_ascii=False)}")
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)

    # 显式启动
    child.sendline("start all")
    expect_prompt_stable(child, quiet=0.8, max_wait=20.0)

    # 等待 desktop 输出包含两侧窗口（A 与 B），避免后续顺序断言受初始化时序影响
    def _read_desktop_list() -> list[str] | None:
        child.sendline("desktop")
        output0 = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
        out0 = strip_ansi(output0)
        end0 = out0.rfind("]")
        if end0 == -1:
            return None
        start0 = -1
        i0 = end0
        while i0 >= 0:
            if out0[i0] == "[":
                j0 = i0 + 1
                while j0 < len(out0) and out0[j0].isspace():
                    j0 += 1
                if j0 < len(out0) and out0[j0] == '"':
                    start0 = i0
                    break
            i0 -= 1
        if start0 == -1 or start0 >= end0:
            return None
        try:
            return json.loads(out0[start0 : end0 + 1])
        except Exception:
            return None

    desktop_list = None
    for _ in range(10):
        desktop_list = _read_desktop_list()
        if (
            desktop_list
            and any("example.desktop.subscribe.b" in u for u in desktop_list)
            and any("example.desktop.subscribe.a" in u for u in desktop_list)
        ):
            break
    assert desktop_list is not None and any("example.desktop.subscribe.b" in u for u in desktop_list), (
        f"B windows not present after startup: {desktop_list}"
    )
    assert any("example.desktop.subscribe.a" in u for u in desktop_list), f"A windows not present: {desktop_list}"

    # 2) 通过 tc 调用 B 的工具 mark_b，形成最近调用记录
    tool_req = {
        "agent": "r-e2e",
        "req_id": "req-e2e",
        "computer": "client",
        "tool_name": "mark_b",  # 由 B server 提供
        "params": {},
        "timeout": 5,
    }
    tf = tmp_path / "toolcall_mark_b.json"
    tf.write_text(json.dumps(tool_req, ensure_ascii=False), encoding="utf-8")
    child.sendline(f"tc @{tf}")
    expect_prompt_stable(child, quiet=0.6, max_wait=12.0)

    # 3) 执行 desktop 并解析输出 JSON
    # 再次获取 desktop 列表用于顺序断言
    desktop_list = _read_desktop_list() or []

    # 断言：
    # - B 的 windows 在前（board priority=85，再是 main priority=50）
    # - A 的 fullscreen dashboard 随后（fullscreen 只取第一个）
    assert any("example.desktop.subscribe.b" in u for u in desktop_list), f"no B windows: {desktop_list}"
    assert any("example.desktop.subscribe.a" in u for u in desktop_list), f"no A windows: {desktop_list}"

    # 找到 B 的两个窗口位置
    b_board_idx = next((i for i, u in enumerate(desktop_list) if "/board" in u), None)
    b_main_idx = next((i for i, u in enumerate(desktop_list) if "/main" in u and "subscribe.b" in u), None)
    a_dash_idx = next((i for i, u in enumerate(desktop_list) if "/dashboard" in u), None)

    assert b_board_idx is not None and b_main_idx is not None and a_dash_idx is not None, desktop_list
    assert b_board_idx < b_main_idx < a_dash_idx, f"order mismatch: {desktop_list}"

    # 4) 通过 tc 调用 A 的工具 mark_a，再次获取 desktop 列表并断言顺序翻转：
    #    - A 的 fullscreen /dashboard 在最前（fullscreen 只取第一个）
    #    - 随后是 B 的 board 与 main（85 与 50）
    tool_req_a = {
        "agent": "r-e2e",
        "req_id": "req-e2e-2",
        "computer": "client",
        "tool_name": "mark_a",  # 由 A server 提供
        "params": {},
        "timeout": 5,
    }
    tf_a = tmp_path / "toolcall_mark_a.json"
    tf_a.write_text(json.dumps(tool_req_a, ensure_ascii=False), encoding="utf-8")
    child.sendline(f"tc @{tf_a}")
    expect_prompt_stable(child, quiet=0.6, max_wait=12.0)

    desktop_list = _read_desktop_list() or []
    assert any("example.desktop.subscribe.b" in u for u in desktop_list), f"no B windows after A: {desktop_list}"
    assert any("example.desktop.subscribe.a" in u for u in desktop_list), f"no A windows after A: {desktop_list}"

    # A 的 fullscreen dashboard 应位于最前
    a_dash_idx = next((i for i, u in enumerate(desktop_list) if "/dashboard" in u), None)
    b_board_idx = next((i for i, u in enumerate(desktop_list) if "/board" in u), None)
    b_main_idx = next((i for i, u in enumerate(desktop_list) if "/main" in u and "subscribe.b" in u), None)
    assert a_dash_idx is not None and b_board_idx is not None and b_main_idx is not None, desktop_list
    assert a_dash_idx < b_board_idx < b_main_idx, f"order mismatch after A: {desktop_list}"

    # 5) 再次调用 B 的工具 mark_b，验证顺序再次回到 B 优先
    tool_req_b2 = {
        "agent": "r-e2e",
        "req_id": "req-e2e-3",
        "computer": "client",
        "tool_name": "mark_b",
        "params": {},
        "timeout": 5,
    }
    tf_b2 = tmp_path / "toolcall_mark_b_2.json"
    tf_b2.write_text(json.dumps(tool_req_b2, ensure_ascii=False), encoding="utf-8")
    child.sendline(f"tc @{tf_b2}")
    expect_prompt_stable(child, quiet=0.6, max_wait=12.0)

    desktop_list = _read_desktop_list() or []
    assert any("example.desktop.subscribe.b" in u for u in desktop_list), f"no B windows after B2: {desktop_list}"
    assert any("example.desktop.subscribe.a" in u for u in desktop_list), f"no A windows after B2: {desktop_list}"

    b_board_idx = next((i for i, u in enumerate(desktop_list) if "/board" in u), None)
    b_main_idx = next((i for i, u in enumerate(desktop_list) if "/main" in u and "subscribe.b" in u), None)
    a_dash_idx = next((i for i, u in enumerate(desktop_list) if "/dashboard" in u), None)
    assert b_board_idx is not None and b_main_idx is not None and a_dash_idx is not None, desktop_list
    assert b_board_idx < b_main_idx < a_dash_idx, f"order mismatch after B2: {desktop_list}"
