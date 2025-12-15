# -*- coding: utf-8 -*-
# filename: test_base_client_windows.py
# @Time    : 2025/10/02 17:05
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 集成测试 BaseMCPClient.list_windows，针对新提供的 Resources 服务器（无订阅/有订阅）。
英文: Integration tests for BaseMCPClient.list_windows with new Resources servers (no subscribe/with subscribe).
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from mcp import StdioServerParameters

from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient


@pytest.mark.asyncio
async def test_list_windows_without_subscribe_returns_empty() -> None:
    """
    中文: 针对仅 Resources 且不支持订阅的服务器，list_windows 应返回空列表。
    英文: For Resources-only server without subscribe, list_windows should return an empty list.
    """
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "resources_stdio_server.py"
    assert server_py.exists(), f"server script not found: {server_py}"

    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])
    client = StdioMCPClient(params)

    await client.aconnect()
    await client._create_session_success_event.wait()

    # capabilities 检查 / check capabilities
    assert client.initialize_result is not None
    assert client.initialize_result.capabilities.resources is not None
    assert client.initialize_result.capabilities.resources.subscribe is False

    windows = await client.list_windows()
    assert isinstance(windows, list)
    assert windows == [], "Without subscribe capability, list_windows should return []."

    await client.adisconnect()
    await client._async_session_closed_event.wait()


@pytest.mark.asyncio
async def test_list_windows_with_subscribe_returns_sorted_and_subscribed() -> None:
    """
    中文: 针对支持订阅的服务器，list_windows 应返回 window:// 资源，并按 priority 降序排序。
    英文: For server with subscribe enabled, list_windows should return window:// resources sorted by priority desc.
    """
    server_py = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / "resources_subscribe_stdio_server.py"
    assert server_py.exists(), f"server script not found: {server_py}"

    params = StdioServerParameters(command=sys.executable, args=[str(server_py)])
    client = StdioMCPClient(params)

    await client.aconnect()
    await client._create_session_success_event.wait()

    # capabilities 检查 / check capabilities
    assert client.initialize_result is not None
    assert client.initialize_result.capabilities.resources is not None
    assert client.initialize_result.capabilities.resources.subscribe is True

    windows = await client.list_windows()
    assert isinstance(windows, list)
    assert len(windows) >= 1

    # 验证 window:// 协议与排序（dashboard priority=90 在前，main priority=60 在后）
    uris = [str(r.uri) for r in windows]
    assert all(u.startswith("window://") for u in uris)

    # 订阅版服务器中，我们预置了 dashboard(90, fullscreen) 与 main(60)
    # 在 BaseMCPClient 内部按优先级降序排序，因此期望 dashboard 在前
    assert any("/dashboard" in u for u in uris), "dashboard window expected"
    assert any("/main" in u for u in uris), "main window expected"

    if len(uris) >= 2:
        # 前两个至少应满足优先级排序
        # 对于我们预置的资源，期望顺序：dashboard -> main
        assert "/dashboard" in uris[0]
        assert "/main" in uris[1]

    await client.adisconnect()
    await client._async_session_closed_event.wait()
