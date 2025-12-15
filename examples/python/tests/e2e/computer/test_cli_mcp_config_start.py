"""
文件名: test_cli_mcp_config_start.py
作者: JQQ
创建日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: 使用固定的配置文件 tests/e2e/computer/configs/server_direct_execution.json 来添加并启动 stdio MCP Server，然后校验 status 与 tools。
  English: Use fixed config file to add/start stdio MCP Server and assert status/tools.
"""

from __future__ import annotations

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


def _assert_contains_tools(child: pexpect.spawn, tool_name: str, retries: int = 10, delay: float = 1.0) -> None:
    """检查工具列表中是否包含指定的工具名称 / Check if tools list contains the specified tool name"""
    # 获取详细输出用于调试 / Final attempt with detailed output for debugging
    child.sendline("tools")
    out = expect_prompt_stable(child, quiet=0.4, max_wait=12.0)
    clean_out = strip_ansi(out)
    assert tool_name in clean_out, f"tools 未包含 {tool_name}. 尝试次数: {retries}. 清理后输出:\n{clean_out}\n原始输出:\n{out}"


def _assert_status_has(child: pexpect.spawn, server_name: str, retries: int = 10, delay: float = 1.0) -> None:
    """检查服务器状态中是否包含指定的服务器名称 / Check if server status contains the specified server name"""
    # 获取详细输出用于调试 / Final attempt with detailed output for debugging
    child.sendline("status")
    out = expect_prompt_stable(child, quiet=0.4, max_wait=12.0)
    clean_out = strip_ansi(out)
    assert server_name in clean_out, f"status 未出现 {server_name}. 尝试次数: {retries}. 清理后输出:\n{clean_out}\n原始输出:\n{out}"


@pytest.mark.e2e
def test_start_via_known_config_file(cli_proc: pexpect.spawn) -> None:
    """
    使用固定的配置文件路径添加并启动 direct_execution 服务器，然后验证状态与工具：
    - server add @tests/e2e/computer/configs/server_direct_execution.json
    - start all
    - status 包含 e2e-test
    - tools 包含 hello
    """
    child = cli_proc

    # 添加服务器配置 / Add server configuration
    child.sendline("server add @tests/e2e/computer/configs/server_direct_execution.json")
    # 等待稳定提示符，确保 add 的输出完全结束 / wait for stable prompt to ensure add output finished
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)

    # 验证服务器状态和工具可用性 / Verify server status and tool availability
    _assert_status_has(child, "e2e-test", retries=12, delay=1.0)
    _assert_contains_tools(child, "hello", retries=12, delay=1.0)
