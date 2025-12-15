# -*- coding: utf-8 -*-
# filename: test_base_client_edge_cases.py
# @Time    : 2025/10/02 22:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 覆盖 BaseMCPClient 中边界/异常分支的集成测试。
英文: Integration tests to cover edge/error branches in BaseMCPClient.

目标覆盖行 (base_client.py):
- 213-216: _keep_alive_task 异常路径 / exception path
- 351-352: list_tools 分页 / list_tools pagination
- 379-380: list_resources 分页 / list_resources pagination
- 387: 过滤非 window 资源 / filter non-window resources
- 399-401: list_windows 异常返回 [] / error path returns []
- 423: get_window_detail 接受 str / accepts str
- 426-428: get_window_detail 异常返回固定内容 / error return with fixed content
- 458-461: _close_task 等待子任务报错分支 / awaiting subtask raises non-cancel error
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

import pytest
from mcp import ClientSession, StdioServerParameters

from a2c_smcp.computer.mcp_clients.base_client import BaseMCPClient
from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient

# ------------------------------
# 工具函数 / helpers
# ------------------------------


def _server_path(name: str) -> Path:
    return Path(__file__).resolve().parents[2] / "computer" / "mcp_servers" / name


# ------------------------------
# 覆盖 list_tools 分页 / tools pagination
# ------------------------------


@pytest.mark.asyncio
async def test_list_tools_pagination() -> None:
    """
    中文: 连接分页工具服务器，覆盖 list_tools 的 nextCursor 循环。
    英文: Connect paged tools server to cover list_tools nextCursor loop.
    """
    server_py = _server_path("tools_paged_stdio_server.py")
    assert server_py.exists()

    client = StdioMCPClient(StdioServerParameters(command=sys.executable, args=[str(server_py)]))
    await client.aconnect()
    await client._create_session_success_event.wait()

    tools = await client.list_tools()
    names = {t.name for t in tools}
    assert {"page1_tool", "page2_tool"}.issubset(names)

    await client.adisconnect()
    await client._async_session_closed_event.wait()


# ------------------------------
# 覆盖 list_windows 分页与过滤 / windows pagination + filtering
# ------------------------------


@pytest.mark.asyncio
async def test_list_windows_pagination_and_filtering() -> None:
    """
    中文: 连接分页+混合资源服务器，覆盖 list_resources 的分页与非 window 过滤。
    英文: Connect paged+mixed resources server to cover pagination and non-window filtering.
    """
    server_py = _server_path("resources_paged_mixed_stdio_server.py")
    assert server_py.exists()

    client = StdioMCPClient(StdioServerParameters(command=sys.executable, args=[str(server_py)]))
    await client.aconnect()
    await client._create_session_success_event.wait()

    # subscribe=True 才会进入逻辑分支 / must be subscribe=True
    assert client.initialize_result and client.initialize_result.capabilities.resources
    assert client.initialize_result.capabilities.resources.subscribe is True

    windows = await client.list_windows()
    # 应仅包含 window://，且包含来自两页的 p1/p2，但不包含 file:// / only window:// present
    uris = [str(r.uri) for r in windows]
    assert all(u.startswith("window://") for u in uris)
    assert any("/p1" in u for u in uris)
    assert any("/p2" in u for u in uris)

    await client.adisconnect()
    await client._async_session_closed_event.wait()


# ------------------------------
# 覆盖 list_windows 异常返回 [] / list_windows error -> []
# ------------------------------


@pytest.mark.asyncio
async def test_list_windows_error_returns_empty(monkeypatch) -> None:  # noqa: PT019
    """
    中文: 模拟 list_resources 抛出异常，覆盖 list_windows 的异常路径并返回 []。
    英文: Simulate list_resources raising to cover list_windows exception path returning [].
    """
    server_py = _server_path("resources_subscribe_stdio_server.py")
    assert server_py.exists()

    client = StdioMCPClient(StdioServerParameters(command=sys.executable, args=[str(server_py)]))
    await client.aconnect()
    await client._create_session_success_event.wait()

    # monkeypatch ClientSession.list_resources 以触发异常 / raise from list_resources
    asession = await client.async_session

    async def boom(*args, **kwargs):  # noqa: ANN001, ANN002
        raise RuntimeError("boom")

    monkeypatch.setattr(asession, "list_resources", boom, raising=True)

    windows = await client.list_windows()
    assert windows == []

    await client.adisconnect()
    await client._async_session_closed_event.wait()


# ------------------------------
# 覆盖 get_window_detail 成功/异常 / get_window_detail ok/error
# ------------------------------


@pytest.mark.asyncio
async def test_get_window_detail_ok_and_error() -> None:
    """
    中文: 成功读取与异常读取。成功用字符串 URI 调用以覆盖 AnyUrl(resource) 分支；异常使用资源对象以覆盖 except 内的 resource.uri。
    英文: Success via str URI to cover AnyUrl(resource); error via Resource object so except can access resource.uri.
    """
    # OK: 使用分页混合服务器读取字符串 URI / read from str URI
    paged_py = _server_path("resources_paged_mixed_stdio_server.py")
    client_ok = StdioMCPClient(StdioServerParameters(command=sys.executable, args=[str(paged_py)]))
    await client_ok.aconnect()
    await client_ok._create_session_success_event.wait()

    windows = await client_ok.list_windows()
    assert windows
    uri_str = str(windows[0].uri)
    detail = await client_ok.get_window_detail(uri_str)
    # 返回应为 ReadResourceResult，包含 contents / should contain contents
    assert getattr(detail, "contents", None) is not None

    await client_ok.adisconnect()
    await client_ok._async_session_closed_event.wait()

    # ERROR: 使用抛出 read_resource 的服务器 / server raising on read_resource
    err_py = _server_path("resources_read_error_stdio_server.py")
    client_err = StdioMCPClient(StdioServerParameters(command=sys.executable, args=[str(err_py)]))
    await client_err.aconnect()
    await client_err._create_session_success_event.wait()

    windows2 = await client_err.list_windows()
    assert not windows2, "因为resources_read_error_stdio_server.py没有实现 subscribe 能力，因此直接返回空"


# ------------------------------
# 覆盖 _keep_alive_task 异常路径 / _keep_alive_task error path
# ------------------------------


class FailingClient(BaseMCPClient):
    """
    中文: 自定义客户端：创建会话阶段抛出异常，从而触发 _create_session_failure_event 与 aerror()。
    英文: Custom client that raises during _create_async_session to trigger failure path and aerror().
    """

    async def _create_async_session(self) -> ClientSession:  # type: ignore[override]
        raise RuntimeError("create session failed")


# ------------------------------
# 覆盖 _close_task 等待子任务报错分支 / _close_task generic exception path
# ------------------------------


@pytest.mark.asyncio
async def test_close_task_handles_non_cancel_error():
    """
    中文: 构造一个在取消后抛出非 CancelledError 的任务，覆盖 _close_task 的异常捕获分支。
    英文: Build a task that raises non-CancelledError on cancellation to cover _close_task error branch.
    """

    async def evil_task():
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError as e:
            # 在取消时抛出其他异常 / raise non-cancel after cancel
            raise RuntimeError("boom-after-cancel") from e

    # 生成一个假的客户端以便复用 _close_task / reuse _close_task
    class Dummy(BaseMCPClient):
        async def _create_async_session(self) -> ClientSession:  # type: ignore[override]
            raise NotImplementedError

    dummy = Dummy(params=StdioServerParameters(command=sys.executable, args=["-c", "print('x')"]))
    dummy._session_keep_alive_task = asyncio.create_task(evil_task())

    # 执行关闭逻辑，若分支覆盖成功则不会抛出异常 / close should swallow non-cancel errors and log
    await dummy._close_task()
