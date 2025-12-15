"""
文件名: test_cli_mcp_servers.py
作者: JQQ
创建日期: 2025/9/22
最后修改日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: e2e 测试：通过配置文件与内联 JSON 添加 stdio MCP Server（direct_execution），并校验 status 与 tools。
  English: E2E tests adding stdio MCP server (direct_execution) via @file and inline JSON, then assert status/tools.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


def _assert_tools_contains(child: pexpect.spawn, tool_name: str, retries: int = 5, delay: float = 0.8) -> None:
    """轮询 tools 输出，直到包含指定工具或重试耗尽。"""
    # 最后一次失败时断言
    child.sendline("tools")
    output = expect_prompt_stable(child, quiet=max(0.3, delay), max_wait=12.0)
    output = strip_ansi(output)
    assert tool_name in output, f"tools 未包含 {tool_name}. 输出:\n{output}"


def _assert_status_has_server(child: pexpect.spawn, server_name: str, retries: int = 5, delay: float = 0.8) -> None:
    """轮询 status 输出，直到出现指定 server 名称或重试耗尽。"""
    child.sendline("status")
    output = expect_prompt_stable(child, quiet=max(0.3, delay), max_wait=12.0)
    output = strip_ansi(output)
    assert server_name in output, f"status 未出现 {server_name}. 输出:\n{output}"


@pytest.mark.e2e
def test_add_server_via_config_file(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    场景1：使用配置文件（@file）添加 direct_execution 服务器，随后检查 status 和 tools。
    - server add @file
    - start all（冗余调用，确保已启动）
    - status 中应包含 e2e-test
    - tools 中应包含 hello（来自 direct_execution 的工具）
    """
    child = cli_proc

    # 1) 写入 server 配置文件
    server_cfg = {
        "name": "e2e-test",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_direct_execution.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 2) 添加配置（@file）
    child.sendline(f"server add @{cfg_path}")
    # 中文: 等待稳定提示符，确保 add 输出结束 / English: wait for stable prompt to ensure add finished
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)
    # 后台启动有一定延迟，显式 start all 以确保启动
    child.sendline("start all")
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)

    # 3) 校验 status/tools
    _assert_status_has_server(child, "e2e-test", retries=8, delay=0.8)
    _assert_tools_contains(child, "hello", retries=10, delay=1.0)


@pytest.mark.e2e
def test_add_server_via_inline_json_and_check(cli_proc: pexpect.spawn) -> None:
    """
    场景2：正常启动后使用内联 JSON 进行 server add，然后依次使用 status 与 tools 校验。
    - server add {json}
    - start all（冗余调用，确保已启动）
    - status 中应包含 e2e-test-inline
    - tools 中应包含 hello
    """
    child = cli_proc

    # 使用当前 Python 解释器构建内联 JSON / Build inline JSON with current Python interpreter
    inline_json = (
        f'{{"name": "e2e-test-inline", "type": "stdio", "disabled": false, "forbidden_tools": [], '
        f'"tool_meta": {{}}, "server_parameters": {{"command": "{sys.executable}", "args": '
        f' ["tests/integration_tests/computer/mcp_servers/direct_execution.py"], "env": null, "cwd": null, '
        f' "encoding": "utf-8", "encoding_error_handler": "strict"}}}}'
    )

    # 1) 添加配置（inline JSON）
    child.sendline(f"server add {inline_json}")
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)
    # 显式启动
    child.sendline("start all")
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)

    # 2) 校验 status/tools
    _assert_status_has_server(child, "e2e-test-inline", retries=8, delay=0.8)
    _assert_tools_contains(child, "hello", retries=10, delay=1.0)
