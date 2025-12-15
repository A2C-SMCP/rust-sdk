# -*- coding: utf-8 -*-
# filename: test_async_client.py
# @Time    : 2025/10/02 23:05
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文：AsyncSMCPAgentClient 的单元测试用例。
English: Unit tests for AsyncSMCPAgentClient.
"""

import uuid
from typing import Any
from unittest.mock import AsyncMock, patch

import pytest
from mcp.types import CallToolResult

from a2c_smcp.agent.auth import DefaultAgentAuthProvider
from a2c_smcp.agent.client import AsyncSMCPAgentClient
from a2c_smcp.smcp import (
    CANCEL_TOOL_CALL_EVENT,
    SMCP_NAMESPACE,
    EnterOfficeNotification,
    LeaveOfficeNotification,
    SMCPTool,
    UpdateMCPConfigNotification,
)


class _AsyncEH:
    """
    中文：异步事件处理器，记录回调数据。
    English: Async event handler recording callbacks.
    """

    def __init__(self) -> None:
        self.enter_office_calls: list[EnterOfficeNotification] = []
        self.leave_office_calls: list[LeaveOfficeNotification] = []
        self.update_config_calls: list[UpdateMCPConfigNotification] = []
        self.tools_received_calls: list[tuple[str, list[SMCPTool]]] = []
        # 记录传入的client实例 / Record passed client instances
        self.enter_office_clients: list[AsyncSMCPAgentClient] = []
        self.leave_office_clients: list[AsyncSMCPAgentClient] = []
        self.update_config_clients: list[AsyncSMCPAgentClient] = []
        self.tools_received_clients: list[AsyncSMCPAgentClient] = []

    async def on_computer_enter_office(self, data: EnterOfficeNotification, sio: AsyncSMCPAgentClient) -> None:
        self.enter_office_calls.append(data)
        self.enter_office_clients.append(sio)

    async def on_computer_leave_office(self, data: LeaveOfficeNotification, sio: AsyncSMCPAgentClient) -> None:
        self.leave_office_calls.append(data)
        self.leave_office_clients.append(sio)

    async def on_computer_update_config(self, data: UpdateMCPConfigNotification, sio: AsyncSMCPAgentClient) -> None:
        self.update_config_calls.append(data)
        self.update_config_clients.append(sio)

    async def on_tools_received(self, computer: str, tools: list[SMCPTool], sio: AsyncSMCPAgentClient) -> None:
        self.tools_received_calls.append((computer, tools))
        self.tools_received_clients.append(sio)


@pytest.fixture
def auth_provider() -> DefaultAgentAuthProvider:
    """
    中文：创建默认认证提供者。
    English: Create default auth provider.
    """
    return DefaultAgentAuthProvider(agent_id="test_agent", office_id="test_office", api_key="k")


@pytest.fixture
def handler() -> _AsyncEH:
    """
    中文：创建异步事件处理器。
    English: Create async event handler.
    """
    return _AsyncEH()


@pytest.fixture
def client(auth_provider: DefaultAgentAuthProvider, handler: _AsyncEH) -> AsyncSMCPAgentClient:
    """
    中文：实例化被测客户端。
    English: Instantiate client under test.
    """
    return AsyncSMCPAgentClient(auth_provider=auth_provider, event_handler=handler)


@pytest.mark.asyncio
async def test_init(client: AsyncSMCPAgentClient) -> None:
    """
    中文：校验初始化成功。
    English: Validate initialization.
    """
    assert client.auth_provider is not None
    assert client.event_handler is not None


@pytest.mark.asyncio
async def test_validate_emit_event_notify_rejected(client: AsyncSMCPAgentClient) -> None:
    """
    中文：校验 notify 事件被拒绝。
    English: Validate notify events are rejected.
    """
    with pytest.raises(ValueError, match="AgentClient不允许使用notify"):
        await client.emit("notify:x")


@pytest.mark.asyncio
async def test_validate_emit_event_valid_pass(client: AsyncSMCPAgentClient) -> None:
    """
    中文：校验合法事件通过验证并调用父类 emit。
    English: Validate valid event passes and calls parent emit.
    """
    with patch("socketio.AsyncClient.emit", new=AsyncMock()) as mock_emit:
        await client.emit("client:ok")
        mock_emit.assert_awaited_once()


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_emit_tool_call_success(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：工具调用成功返回 CallToolResult。
    English: Tool call returns CallToolResult on success.
    """
    mock_call.return_value = {"content": [{"text": "ok", "type": "text"}], "isError": False}

    res = await client.emit_tool_call("comp-1", "echo", {"text": "hi"}, timeout=5)

    assert isinstance(res, CallToolResult)
    assert not res.isError
    mock_call.assert_awaited_once()


@pytest.mark.asyncio
@patch("socketio.AsyncClient.emit", new_callable=AsyncMock)
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_emit_tool_call_timeout_sends_cancel(mock_call: AsyncMock, mock_emit: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：工具调用超时触发取消请求并返回错误结果。
    English: Tool call timeout triggers cancel and returns error result.
    """
    mock_call.side_effect = TimeoutError("Timeout")

    res = await client.emit_tool_call("comp-1", "echo", {"text": "hi"}, timeout=1)

    assert isinstance(res, CallToolResult)
    assert res.isError

    # mock捕获了self, event, data作为位置参数，namespace在kwargs中
    # mock captures self, event, data as positional args, namespace in kwargs
    args, kwargs = mock_emit.call_args
    assert args[1] == CANCEL_TOOL_CALL_EVENT  # args[0]是self，args[1]是event
    assert args[3] == SMCP_NAMESPACE


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_tools_from_computer_success(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：成功获取工具列表。
    English: Successfully get tools list.
    """
    req_id = uuid.uuid4().hex
    mock_resp = {"tools": [{"name": "t1"}], "req_id": req_id}

    with patch("uuid.uuid4") as m_uuid:
        m_uuid.return_value.hex = req_id
        mock_call.return_value = mock_resp

        ret = await client.get_tools_from_computer("comp-1")
        assert ret["req_id"] == req_id
        assert ret["tools"] and ret["tools"][0]["name"] == "t1"


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_tools_from_computer_invalid_response(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：获取工具列表时 req_id 不匹配抛出异常。
    English: Invalid tools response with mismatched req_id raises.
    """
    mock_call.return_value = {"tools": [], "req_id": "wrong"}

    with pytest.raises(ValueError, match="Invalid response"):
        await client.get_tools_from_computer("comp-1")


@pytest.mark.asyncio
async def test_process_tools_response_calls_handler(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：处理工具响应时调用事件处理器。
    English: process_tools_response should call event handler.
    """
    tools = [{"name": "t1"}]
    await client.process_tools_response({"tools": tools, "req_id": "x"}, "comp-1")

    assert handler.tools_received_calls
    comp, received = handler.tools_received_calls[0]
    assert comp == "comp-1" and received[0]["name"] == "t1"


@pytest.mark.asyncio
async def test_event_handlers_enter_leave_update(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：验证 enter/leave/update 事件分派与后续工具拉取逻辑。
    English: Validate enter/leave/update handlers and tools fetch path.
    """

    # mock get_tools_from_computer 返回工具
    # mock get_tools_from_computer to return tools
    async def _mock_get_tools(_: str, timeout: int = 20) -> dict[str, Any]:  # noqa: ARG001
        return {"tools": [{"name": "t-x"}], "req_id": "r"}

    with patch.object(client, "get_tools_from_computer", new=AsyncMock(side_effect=_mock_get_tools)):
        # enter
        enter: EnterOfficeNotification = {"office_id": "test_office", "computer": "c1"}
        await client._on_computer_enter_office(enter)
        assert handler.enter_office_calls and handler.tools_received_calls

        # leave
        leave: LeaveOfficeNotification = {"office_id": "test_office", "computer": "c1"}
        await client._on_computer_leave_office(leave)
        assert handler.leave_office_calls

        # update config 再次触发获取工具
        update: UpdateMCPConfigNotification = {"computer": "c1"}
        await client._on_computer_update_config(update)
        # 不强校验次数，仅保证路径覆盖
        assert handler.update_config_calls


@pytest.mark.asyncio
async def test_sio_param_passed_to_enter_office_handler(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：测试sio参数被正确传入enter_office处理器。
    English: Test sio param is correctly passed to enter_office handler.
    """

    async def _mock_get_tools(_: str, timeout: int = 20) -> dict[str, Any]:  # noqa: ARG001
        return {"tools": [], "req_id": "r"}

    with patch.object(client, "get_tools_from_computer", new=AsyncMock(side_effect=_mock_get_tools)):
        enter: EnterOfficeNotification = {"office_id": "test_office", "computer": "c1"}
        await client._on_computer_enter_office(enter)

        # 验证client实例被传入 / Verify client instance was passed
        assert len(handler.enter_office_clients) == 1
        passed_client = handler.enter_office_clients[0]

        # 验证传入的是同一个client实例 / Verify it's the same client instance
        assert passed_client is client
        assert isinstance(passed_client, AsyncSMCPAgentClient)

        # 验证可以访问client的属性 / Verify can access client properties
        assert hasattr(passed_client, "auth_provider")
        assert passed_client.auth_provider is not None


@pytest.mark.asyncio
async def test_sio_param_passed_to_leave_office_handler(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：测试sio参数被正确传入leave_office处理器。
    English: Test sio param is correctly passed to leave_office handler.
    """
    leave: LeaveOfficeNotification = {"office_id": "test_office", "computer": "c1"}
    await client._on_computer_leave_office(leave)

    # 验证client实例被传入 / Verify client instance was passed
    assert len(handler.leave_office_clients) == 1
    passed_client = handler.leave_office_clients[0]

    # 验证传入的是同一个client实例 / Verify it's the same client instance
    assert passed_client is client
    assert isinstance(passed_client, AsyncSMCPAgentClient)


@pytest.mark.asyncio
async def test_sio_param_passed_to_update_config_handler(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：测试sio参数被正确传入update_config处理器。
    English: Test sio param is correctly passed to update_config handler.
    """

    async def _mock_get_tools(_: str, timeout: int = 20) -> dict[str, Any]:  # noqa: ARG001
        return {"tools": [], "req_id": "r"}

    with patch.object(client, "get_tools_from_computer", new=AsyncMock(side_effect=_mock_get_tools)):
        update: UpdateMCPConfigNotification = {"computer": "c1"}
        await client._on_computer_update_config(update)

        # 验证client实例被传入 / Verify client instance was passed
        assert len(handler.update_config_clients) == 1
        passed_client = handler.update_config_clients[0]

        # 验证传入的是同一个client实例 / Verify it's the same client instance
        assert passed_client is client
        assert isinstance(passed_client, AsyncSMCPAgentClient)


@pytest.mark.asyncio
async def test_sio_param_passed_to_tools_received_handler(client: AsyncSMCPAgentClient, handler: _AsyncEH) -> None:
    """
    中文：测试sio参数被正确传入tools_received处理器。
    English: Test sio param is correctly passed to tools_received handler.
    """
    tools = [{"name": "test_tool"}]
    await client.process_tools_response({"tools": tools, "req_id": "x"}, "comp-1")

    # 验证client实例被传入 / Verify client instance was passed
    assert len(handler.tools_received_clients) == 1
    passed_client = handler.tools_received_clients[0]

    # 验证传入的是同一个client实例 / Verify it's the same client instance
    assert passed_client is client
    assert isinstance(passed_client, AsyncSMCPAgentClient)


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_computers_in_office_success(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：成功获取房间内的Computer列表。
    English: Successfully get computers list in office.
    """
    office_id = "test_office"
    req_id = f"list_computers_test_agent_{office_id}"

    # 模拟响应包含多个会话，包括computer和agent角色
    # Mock response with multiple sessions including computer and agent roles
    mock_resp = {
        "req_id": req_id,
        "sessions": [
            {"sid": "comp1", "role": "computer", "computer_id": "computer-1"},
            {"sid": "comp2", "role": "computer", "computer_id": "computer-2"},
            {"sid": "agent1", "role": "agent", "agent_id": "agent-1"},
        ],
    }
    mock_call.return_value = mock_resp

    computers = await client.get_computers_in_office(office_id)

    # 验证只返回computer角色的会话 / Verify only computer role sessions are returned
    assert len(computers) == 2
    assert all(c["role"] == "computer" for c in computers)
    assert computers[0]["computer_id"] == "computer-1"
    assert computers[1]["computer_id"] == "computer-2"

    # 验证调用参数 / Verify call arguments
    mock_call.assert_awaited_once()


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_computers_in_office_empty(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：房间内没有Computer时返回空列表。
    English: Return empty list when no computers in office.
    """
    office_id = "test_office"
    req_id = f"list_computers_test_agent_{office_id}"

    # 模拟响应只有agent角色，没有computer
    # Mock response with only agent role, no computer
    mock_resp = {
        "req_id": req_id,
        "sessions": [
            {"sid": "agent1", "role": "agent", "agent_id": "agent-1"},
        ],
    }
    mock_call.return_value = mock_resp

    computers = await client.get_computers_in_office(office_id)

    # 验证返回空列表 / Verify empty list is returned
    assert len(computers) == 0


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_computers_in_office_invalid_response(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：响应req_id不匹配时抛出异常。
    English: Raise exception when response req_id mismatches.
    """
    office_id = "test_office"

    # 模拟响应的req_id不匹配
    # Mock response with mismatched req_id
    mock_resp = {
        "req_id": "wrong_req_id",
        "sessions": [],
    }
    mock_call.return_value = mock_resp

    with pytest.raises(ValueError, match="Invalid response with mismatched req_id"):
        await client.get_computers_in_office(office_id)


@pytest.mark.asyncio
@patch("socketio.AsyncClient.call", new_callable=AsyncMock)
async def test_get_computers_in_office_timeout(mock_call: AsyncMock, client: AsyncSMCPAgentClient) -> None:
    """
    中文：请求超时时抛出异常。
    English: Raise exception on timeout.
    """
    office_id = "test_office"
    mock_call.side_effect = TimeoutError("Request timeout")

    with pytest.raises(TimeoutError):
        await client.get_computers_in_office(office_id, timeout=1)
