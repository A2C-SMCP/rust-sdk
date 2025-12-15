# -*- coding: utf-8 -*-
# filename: test_manager_windows.py
# @Time    : 2025/10/02 19:02
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 集成测试 MCPServerManager.list_windows，使用 resources_stdio_server 与 resources_subscribe_stdio_server。
英文: Integration tests for MCPServerManager.list_windows using resources_stdio_server and resources_subscribe_stdio_server.
"""

from __future__ import annotations

import sys
from pathlib import Path

import anyio
import pytest
from mcp import StdioServerParameters, types

from a2c_smcp.computer.mcp_clients.manager import MCPServerManager
from a2c_smcp.computer.mcp_clients.model import StdioServerConfig


@pytest.mark.anyio
async def test_manager_list_windows_aggregates_only_subscribe_server() -> None:
    """
    中文: Manager 应仅聚合开启 resources.subscribe 的服务的窗口；非订阅服务应返回空。
    英文: Manager should aggregate windows only from servers with resources.subscribe enabled; non-subscribe returns empty.
    """
    base = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers"
    sub_py = base / "resources_subscribe_stdio_server.py"
    nosub_py = base / "resources_stdio_server.py"
    assert sub_py.exists() and nosub_py.exists()

    sub_params = StdioServerParameters(command=sys.executable, args=[str(sub_py)])
    nosub_params = StdioServerParameters(command=sys.executable, args=[str(nosub_py)])

    manager = MCPServerManager(auto_connect=False)
    sub_cfg = StdioServerConfig(name="srv_sub", server_parameters=sub_params)
    nosub_cfg = StdioServerConfig(name="srv_nosub", server_parameters=nosub_params)

    await manager.ainitialize([sub_cfg, nosub_cfg])
    await manager.astart_all()

    try:
        results = await manager.list_windows()
        # 只应来自订阅服务 / Only from subscribe server
        assert all(srv == "srv_sub" for srv, _ in results)
        assert len(results) >= 1

        # 验证排序（dashboard priority=90 在 main priority=60 之前）
        uris = [str(res.uri) for _, res in results]
        assert any("/dashboard" in u for u in uris)
        assert any("/main" in u for u in uris)
        if len(uris) >= 2:
            assert "/dashboard" in uris[0]
            assert "/main" in uris[1]
    finally:
        await manager.astop_all()


@pytest.mark.anyio
async def test_manager_list_windows_triggers_resource_updated_notification() -> None:
    """
    中文: Manager 在调用 list_windows 时，客户端会订阅窗口资源；订阅版服务器会立刻发送 ResourceUpdated 通知，
          因此注入的 message_handler 应该接收到该通知。
    英文: When Manager.list_windows triggers subscriptions, the subscribe-capable server immediately sends
          ResourceUpdated notifications; the injected message_handler should receive them.
    """
    base = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers"
    sub_py = base / "resources_subscribe_stdio_server.py"
    assert sub_py.exists()

    sub_params = StdioServerParameters(command=sys.executable, args=[str(sub_py)])

    received: list[types.ResourceUpdatedNotification] = []

    async def message_handler(message):
        # 仅记录资源更新通知 / record only ResourceUpdatedNotification
        if isinstance(message, types.ServerNotification) and isinstance(
            message.root,
            types.ResourceUpdatedNotification,
        ):
            received.append(message.root)

    manager = MCPServerManager(auto_connect=False, message_handler=message_handler)
    sub_cfg = StdioServerConfig(name="srv_sub", server_parameters=sub_params)

    await manager.ainitialize([sub_cfg])
    await manager.astart_all()

    try:
        # 触发订阅 / trigger subscriptions
        results = await manager.list_windows()
        assert results, "should have windows to subscribe"
        listed_uris = {str(res.uri) for _, res in results}

        # 等待通知到达（最多2秒）/ wait up to 2s for notifications
        for _ in range(20):
            if received:
                break
            await anyio.sleep(0.1)

        assert received, "expected at least one ResourceUpdatedNotification"
        # 校验通知中的 URI 合理（属于已订阅的窗口之一）
        assert any(str(n.params.uri) in listed_uris for n in received)
    finally:
        await manager.astop_all()


@pytest.mark.anyio
async def test_manager_list_windows_filter_by_uri() -> None:
    """
    中文: Manager.list_windows(window_uri=...) 仅返回 URI 完全匹配的窗口与其 server 名称。
    英文: Manager.list_windows(window_uri=...) returns only the exact matched window with its server name.
    """
    base = Path(__file__).resolve().parents[2] / "computer" / "mcp_servers"
    sub_py = base / "resources_subscribe_stdio_server.py"
    assert sub_py.exists()

    sub_params = StdioServerParameters(command=sys.executable, args=[str(sub_py)])

    manager = MCPServerManager(auto_connect=False)
    sub_cfg = StdioServerConfig(name="srv_sub", server_parameters=sub_params)

    await manager.ainitialize([sub_cfg])
    await manager.astart_all()

    try:
        # 先获取全部，找到一个 URI
        results_all = await manager.list_windows()
        assert results_all, "should have at least one window from subscribe server"
        target_uri = str(results_all[0][1].uri)

        # 过滤后仅返回匹配项且 server 名称为 srv_sub
        results_filtered = await manager.list_windows(window_uri=target_uri)
        assert len(results_filtered) == 1
        srv_name, res = results_filtered[0]
        assert srv_name == "srv_sub"
        assert str(res.uri) == target_uri
    finally:
        await manager.astop_all()
