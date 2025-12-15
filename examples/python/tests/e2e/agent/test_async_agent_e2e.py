# -*- coding: utf-8 -*-
# filename: test_async_agent_e2e.py
# @Time    : 2025/10/05 15:47
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 异步 Agent 模块的端到端测试：基于真实 HTTP 服务与真实 AsyncSMCPAgentClient，验证核心功能流。
English: End-to-end tests for the Async Agent module: real HTTP service and real AsyncSMCPAgentClient validating core functionality flows.
"""

from __future__ import annotations

import asyncio

import pytest

from a2c_smcp.agent import AsyncSMCPAgentClient, DefaultAgentAuthProvider
from a2c_smcp.smcp import (
    JOIN_OFFICE_EVENT,
    LEAVE_OFFICE_EVENT,
    SMCP_NAMESPACE,
    EnterOfficeNotification,
    LeaveOfficeNotification,
    SMCPTool,
    UpdateMCPConfigNotification,
)

pytestmark = pytest.mark.e2e


async def _wait_until(cond, timeout: float = 2.0, step: float = 0.01) -> bool:
    """
    中文: 简易异步等待辅助函数，直到条件满足或超时。
    English: Simple async wait helper until condition met or timeout.
    """
    end = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < end:
        if cond():
            return True
        await asyncio.sleep(step)
    return cond()


class MockAsyncEventHandler:
    """
    中文: 测试用的异步事件处理器，记录所有事件
    English: Test async event handler that records all events
    """

    def __init__(self):
        self.enter_office_events: list[EnterOfficeNotification] = []
        self.leave_office_events: list[LeaveOfficeNotification] = []
        self.update_config_events: list[UpdateMCPConfigNotification] = []
        self.tools_received_events: list[tuple[str, list[SMCPTool]]] = []

    async def on_computer_enter_office(self, data: EnterOfficeNotification, sio: AsyncSMCPAgentClient) -> None:
        self.enter_office_events.append(data)

    async def on_computer_leave_office(self, data: LeaveOfficeNotification, sio: AsyncSMCPAgentClient) -> None:
        self.leave_office_events.append(data)

    async def on_computer_update_config(self, data: UpdateMCPConfigNotification, sio: AsyncSMCPAgentClient) -> None:
        self.update_config_events.append(data)

    async def on_tools_received(self, computer: str, tools: list[SMCPTool], sio: AsyncSMCPAgentClient) -> None:
        self.tools_received_events.append((computer, tools))


@pytest.mark.asyncio
async def test_async_agent_connect_and_join_office(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证异步 Agent 客户端连接到服务器
      - 验证异步 Agent 加入办公室
      - 验证 Computer 先加入后 Agent 能收到通知
    English:
      - Verify async Agent client connects to server
      - Verify async Agent joins office
      - Verify Agent receives notification when Computer joins first
    """
    # 创建认证提供者 / Create auth provider
    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-1",
        office_id="office-async-1",
    )

    # 创建事件处理器 / Create event handler
    event_handler = MockAsyncEventHandler()

    # 创建异步 Agent 客户端 / Create async Agent client
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 连接到服务器 / Connect to server
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")

        # 等待连接稳定 / Wait for connection to stabilize
        await asyncio.sleep(0.1)

        # Agent 加入办公室 / Agent joins office
        ok, err = await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-1", "office_id": "office-async-1"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        assert ok is True
        assert err is None

        # Computer 加入办公室 / Computer joins office
        ok, err = await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "test-computer-async-1", "office_id": "office-async-1"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        assert ok is True
        assert err is None

        # 等待事件处理 / Wait for event processing
        assert await _wait_until(lambda: len(event_handler.enter_office_events) >= 1, timeout=3)

        # 验证收到的事件 / Verify received events
        assert len(event_handler.enter_office_events) >= 1
        enter_event = event_handler.enter_office_events[0]
        assert enter_event["office_id"] == "office-async-1"
        assert "computer" in enter_event

        # 验证自动获取工具列表 / Verify automatic tools fetching
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=3)
        computer_id, tools = event_handler.tools_received_events[0]
        assert len(tools) == 2  # echo and add
        assert tools[0]["name"] == "echo"
        assert tools[1]["name"] == "add"

    finally:
        # 清理连接 / Cleanup connection
        await agent_client.disconnect()


@pytest.mark.asyncio
async def test_async_agent_tool_call(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证异步 Agent 调用 Computer 工具
      - 验证工具调用返回正确结果
    English:
      - Verify async Agent calls Computer tools
      - Verify tool call returns correct results
    """
    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-2",
        office_id="office-async-2",
    )

    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=None,
    )

    try:
        # 连接并加入办公室 / Connect and join office
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")
        await asyncio.sleep(0.1)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-2", "office_id": "office-async-2"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # Computer 加入办公室 / Computer joins office
        await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "test-computer-async-2", "office_id": "office-async-2"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # 等待连接稳定 / Wait for connection to stabilize
        await asyncio.sleep(0.2)

        # 调用 echo 工具 / Call echo tool
        result = await agent_client.emit_tool_call(
            computer="test-computer-async-2",
            tool_name="echo",
            params={"message": "Hello, Async World!"},
            timeout=5,
        )

        # 验证结果 / Verify result
        assert result.isError is False
        assert len(result.content) == 1
        assert "Echo: Hello, Async World!" in result.content[0].text

        # 调用 add 工具 / Call add tool
        result = await agent_client.emit_tool_call(
            computer="test-computer-async-2",
            tool_name="add",
            params={"a": 15, "b": 25},
            timeout=5,
        )

        # 验证结果 / Verify result
        assert result.isError is False
        assert len(result.content) == 1
        assert "Result: 40" in result.content[0].text

    finally:
        await agent_client.disconnect()


@pytest.mark.asyncio
async def test_async_agent_get_tools_and_desktop(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证异步 Agent 获取 Computer 工具列表
      - 验证异步 Agent 获取 Computer 桌面信息
    English:
      - Verify async Agent gets Computer tools list
      - Verify async Agent gets Computer desktop info
    """
    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-3",
        office_id="office-async-3",
    )

    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=None,
    )

    try:
        # 连接并加入办公室 / Connect and join office
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")
        await asyncio.sleep(0.1)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-3", "office_id": "office-async-3"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # Computer 加入办公室 / Computer joins office
        await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "test-computer-async-3", "office_id": "office-async-3"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        await asyncio.sleep(0.2)

        # 获取工具列表 / Get tools list
        tools_response = await agent_client.get_tools_from_computer("test-computer-async-3", timeout=5)

        # 验证工具列表 / Verify tools list
        assert tools_response["req_id"] is not None
        assert len(tools_response["tools"]) == 2
        assert tools_response["tools"][0]["name"] == "echo"
        assert tools_response["tools"][1]["name"] == "add"

        # 获取桌面信息 / Get desktop info
        desktop_response = await agent_client.get_desktop_from_computer("test-computer-async-3", timeout=5)

        # 验证桌面信息 / Verify desktop info
        assert desktop_response["req_id"] is not None
        assert len(desktop_response["desktops"]) == 2
        assert "window://1" in desktop_response["desktops"]
        assert "window://2" in desktop_response["desktops"]

    finally:
        await agent_client.disconnect()


@pytest.mark.asyncio
async def test_async_agent_computer_leave_notification(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证 Computer 离开办公室时异步 Agent 收到通知
    English:
      - Verify async Agent receives notification when Computer leaves office
    """
    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-4",
        office_id="office-async-4",
    )

    event_handler = MockAsyncEventHandler()

    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 连接并加入办公室 / Connect and join office
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")
        await asyncio.sleep(0.1)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-4", "office_id": "office-async-4"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # Computer 加入办公室 / Computer joins office
        await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "test-computer-async-4", "office_id": "office-async-4"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # 等待加入事件和工具获取完成 / Wait for join event and tools fetching
        assert await _wait_until(lambda: len(event_handler.enter_office_events) >= 1, timeout=3)
        # 等待工具列表自动获取完成 / Wait for automatic tools fetching to complete
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=3)

        # Computer 离开办公室 / Computer leaves office
        await async_mock_computer_client.call(
            LEAVE_OFFICE_EVENT,
            {"office_id": "office-async-4"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # 等待离开事件 / Wait for leave event
        assert await _wait_until(lambda: len(event_handler.leave_office_events) >= 1, timeout=3)

        # 验证离开事件 / Verify leave event
        leave_event = event_handler.leave_office_events[0]
        assert leave_event["office_id"] == "office-async-4"
        assert "computer" in leave_event

    finally:
        await agent_client.disconnect()


@pytest.mark.asyncio
async def test_async_agent_multiple_tool_calls(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证异步 Agent 可以连续调用多个工具
      - 验证并发工具调用的正确性
    English:
      - Verify async Agent can call multiple tools consecutively
      - Verify correctness of concurrent tool calls
    """
    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-5",
        office_id="office-async-5",
    )

    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=None,
    )

    try:
        # 连接并加入办公室 / Connect and join office
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")
        await asyncio.sleep(0.1)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-5", "office_id": "office-async-5"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # Computer 加入办公室 / Computer joins office
        await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "test-computer-async-5", "office_id": "office-async-5"},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        await asyncio.sleep(0.2)

        # 连续调用多个工具 / Call multiple tools consecutively
        results = []
        for i in range(3):
            result = await agent_client.emit_tool_call(
                computer="test-computer-async-5",
                tool_name="add",
                params={"a": i, "b": i + 1},
                timeout=5,
            )
            results.append(result)

        # 验证所有调用都成功 / Verify all calls succeeded
        assert len(results) == 3
        for i, result in enumerate(results):
            assert result.isError is False
            expected = i + (i + 1)
            assert f"Result: {expected}" in result.content[0].text

    finally:
        await agent_client.disconnect()


@pytest.mark.asyncio
async def test_async_agent_list_room(async_socketio_server, async_server_port: int, async_mock_computer_client):
    """
    中文:
      - 验证异步 Agent 调用 LIST_ROOM 事件
      - 验证返回房间内所有会话信息（Agent 和 Computer）
      - 验证会话信息包含正确的 sid、name、role、office_id
    English:
      - Verify async Agent calls LIST_ROOM event
      - Verify returns all session info in the room (Agent and Computer)
      - Verify session info contains correct sid, name, role, office_id
    """
    from a2c_smcp.smcp import LIST_ROOM_EVENT

    auth_provider = DefaultAgentAuthProvider(
        agent_id="test-agent-async-list-room",
        office_id="office-list-room-e2e",
    )

    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=None,
    )

    try:
        # 连接并加入办公室 / Connect and join office
        await agent_client.connect_to_server(f"http://127.0.0.1:{async_server_port}")
        await asyncio.sleep(0.1)

        office_id = "office-list-room-e2e"
        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "agent-list-test", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # Computer 加入办公室 / Computer joins office
        await async_mock_computer_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "name": "computer-list-test", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # 等待连接稳定 / Wait for connection to stabilize
        await asyncio.sleep(0.2)

        # 获取 Agent 和 Computer 的 SID / Get Agent and Computer SID
        agent_sid = agent_client.get_sid(namespace=SMCP_NAMESPACE)
        computer_sid = async_mock_computer_client.get_sid(namespace=SMCP_NAMESPACE)

        # 调用 LIST_ROOM 事件 / Call LIST_ROOM event
        result = await agent_client.call(
            LIST_ROOM_EVENT,
            {
                "agent": agent_sid,
                "req_id": "list_room_req_e2e",
                "office_id": office_id,
            },
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )

        # 验证返回结果 / Verify result
        assert result is not None
        assert "sessions" in result
        assert "req_id" in result
        assert result["req_id"] == "list_room_req_e2e"

        # 验证会话列表 / Verify session list
        sessions = result["sessions"]
        assert len(sessions) == 2  # 1 Agent + 1 Computer

        # 提取角色列表 / Extract role list
        roles = [s["role"] for s in sessions]
        assert "agent" in roles
        assert "computer" in roles

        # 验证 Agent 会话信息 / Verify Agent session info
        agent_session = next(s for s in sessions if s["role"] == "agent")
        assert agent_session["sid"] == agent_sid
        assert agent_session["name"] == "agent-list-test"
        assert agent_session["office_id"] == office_id

        # 验证 Computer 会话信息 / Verify Computer session info
        computer_session = next(s for s in sessions if s["role"] == "computer")
        assert computer_session["sid"] == computer_sid
        assert computer_session["office_id"] == office_id

    finally:
        await agent_client.disconnect()
