# -*- coding: utf-8 -*-
# filename: test_base.py
# @Time    : 2025/9/30 23:05
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文：Agent 基础客户端（BaseAgentClient）的集成测试。
English: Integration tests for BaseAgentClient.
"""

import pytest
from mcp.types import CallToolResult, TextContent

from a2c_smcp.agent.auth import DefaultAgentAuthProvider
from a2c_smcp.agent.base import BaseAgentSyncClient
from a2c_smcp.agent.types import AgentEventHandler
from a2c_smcp.smcp import EnterOfficeNotification, GetToolsRet, LeaveOfficeNotification, SMCPTool, UpdateMCPConfigNotification


class MockAgentClient(BaseAgentSyncClient):
    """
    中文：用于测试的 Mock Agent 客户端实现。
    English: Mock Agent client implementation for testing.
    """

    def __init__(self, auth_provider, event_handler=None):
        super().__init__(auth_provider, event_handler)
        self.emitted_events = []
        self.called_events = []

    def emit(self, event, data=None, namespace=None, callback=None):
        self.emitted_events.append((event, data, namespace, callback))

    def call(self, event, data=None, namespace=None, timeout=60):
        self.called_events.append((event, data, namespace, timeout))
        return {"success": True}

    def register_event_handlers(self):
        # Mock 实现，不需要实际注册
        pass


class MockEventHandler(AgentEventHandler):
    """Mock 事件处理器"""

    def __init__(self):
        self.enter_events = []
        self.leave_events = []
        self.update_events = []
        self.tools_received = []
        # 记录传入的client实例 / Record passed client instances
        self.enter_clients = []
        self.leave_clients = []
        self.update_clients = []
        self.tools_clients = []

    def on_computer_enter_office(self, data, sio):
        self.enter_events.append(data)
        self.enter_clients.append(sio)

    def on_computer_leave_office(self, data, sio):
        self.leave_events.append(data)
        self.leave_clients.append(sio)

    def on_computer_update_config(self, data, sio):
        self.update_events.append(data)
        self.update_clients.append(sio)

    def on_tools_received(self, computer, tools, sio):
        self.tools_received.append((computer, tools))
        self.tools_clients.append(sio)


def test_base_agent_client_initialization():
    """
    中文：验证基础 Agent 客户端的初始化。
    English: Verify BaseAgentClient initialization.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    handler = MockEventHandler()

    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    assert client.auth_provider == auth
    assert client.event_handler == handler


def test_validate_emit_event_blocks_notify_events():
    """
    中文：验证 validate_emit_event 阻止 notify:* 事件。
    English: Verify validate_emit_event blocks notify:* events.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    client = MockAgentClient(auth_provider=auth)

    with pytest.raises(ValueError, match="AgentClient不允许使用notify"):
        client.validate_emit_event("notify:test_event")

    with pytest.raises(ValueError, match="AgentClient不允许使用notify"):
        client.validate_emit_event("notify:enter_office")


def test_validate_emit_event_blocks_agent_events():
    """
    中文：验证 validate_emit_event 阻止 agent:* 事件。
    English: Verify validate_emit_event blocks agent:* events.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    client = MockAgentClient(auth_provider=auth)

    with pytest.raises(ValueError, match="AgentClient不允许发起agent"):
        client.validate_emit_event("agent:test_action")

    with pytest.raises(ValueError, match="AgentClient不允许发起agent"):
        client.validate_emit_event("agent:do_something")


def test_validate_emit_event_allows_valid_events():
    """
    中文：验证 validate_emit_event 允许有效事件。
    English: Verify validate_emit_event allows valid events.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    client = MockAgentClient(auth_provider=auth)

    # 这些事件应该被允许
    valid_events = [
        "server:join_office",
        "client:tool_call",
        "client:get_tools",
        "server:update_config",
        "custom_event",
    ]

    for event in valid_events:
        # 不应该抛出异常
        client.validate_emit_event(event)


def test_create_tool_call_request():
    """
    中文：验证工具调用请求的创建。
    English: Verify tool call request creation.
    """
    agent_id = "test-agent-123"
    office_id = "test-office-456"
    auth = DefaultAgentAuthProvider(agent_id=agent_id, office_id=office_id)
    client = MockAgentClient(auth_provider=auth)

    computer = "test-computer"
    tool_name = "test_tool"
    params = {"param1": "value1", "param2": 42}
    timeout = 30

    req = client.create_tool_call_request(computer, tool_name, params, timeout)

    # 验证返回的是字典且包含所有必需字段
    assert isinstance(req, dict)
    assert req["computer"] == computer
    assert req["tool_name"] == tool_name
    assert req["params"] == params
    assert req["agent"] == agent_id
    assert req["timeout"] == timeout
    assert "req_id" in req
    assert len(req["req_id"]) == 32  # UUID hex 长度


def test_create_get_tools_request():
    """
    中文：验证获取工具请求的创建。
    English: Verify get tools request creation.
    """
    agent_id = "test-agent-tools"
    office_id = "test-office-tools"
    auth = DefaultAgentAuthProvider(agent_id=agent_id, office_id=office_id)
    client = MockAgentClient(auth_provider=auth)

    computer = "test-computer-tools"

    req = client.create_get_tools_request(computer)

    # 验证返回的是字典且包含所有必需字段
    assert isinstance(req, dict)
    assert req["computer"] == computer
    assert req["agent"] == agent_id
    assert "req_id" in req
    assert len(req["req_id"]) == 32  # UUID hex 长度


def test_handle_tool_call_timeout():
    """
    中文：验证工具调用超时处理。
    English: Verify tool call timeout handling.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    client = MockAgentClient(auth_provider=auth)

    req_id = "test-req-id-123"
    result = client.handle_tool_call_timeout(req_id)

    assert isinstance(result, CallToolResult)
    assert result.isError is True
    assert len(result.content) == 1
    assert isinstance(result.content[0], TextContent)
    assert "工具调用超时" in result.content[0].text
    assert req_id in result.content[0].text


def test_validate_office_data_valid():
    """
    中文：验证有效办公室数据的验证。
    English: Verify validation of valid office data.
    """
    office_id = "test-office-valid"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    client = MockAgentClient(auth_provider=auth)

    # 测试进入办公室通知
    enter_data: EnterOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-123",
        "agent": None,
    }

    computer_id = client.validate_office_data(enter_data)
    assert computer_id == "test-computer-123"

    # 测试离开办公室通知
    leave_data: LeaveOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-456",
        "agent": None,
    }

    computer_id = client.validate_office_data(leave_data)
    assert computer_id == "test-computer-456"


def test_validate_office_data_invalid_office_id():
    """
    中文：验证无效办公室 ID 的验证失败。
    English: Verify validation fails for invalid office ID.
    """
    office_id = "correct-office"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    client = MockAgentClient(auth_provider=auth)

    invalid_data: EnterOfficeNotification = {
        "office_id": "wrong-office",  # 错误的办公室 ID
        "computer": "test-computer",
        "agent": None,
    }

    with pytest.raises(AssertionError, match="无效的办公室ID"):
        client.validate_office_data(invalid_data)


def test_validate_office_data_missing_computer():
    """
    中文：验证缺少计算机 ID 的验证失败。
    English: Verify validation fails for missing computer ID.
    """
    office_id = "test-office"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    client = MockAgentClient(auth_provider=auth)

    invalid_data: EnterOfficeNotification = {
        "office_id": office_id,
        "computer": None,  # 缺少计算机 ID
        "agent": None,
    }

    with pytest.raises(AssertionError, match="无效的计算机ID"):
        client.validate_office_data(invalid_data)


def test_handle_computer_enter_office():
    """
    中文：验证处理计算机进入办公室事件。
    English: Verify handling computer enter office event.
    """
    office_id = "test-office-enter"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    enter_data: EnterOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-enter",
        "agent": None,
    }

    client.handle_computer_enter_office(enter_data)

    # 验证事件处理器被调用
    assert len(handler.enter_events) == 1
    assert handler.enter_events[0] == enter_data


def test_handle_computer_leave_office():
    """
    中文：验证处理计算机离开办公室事件。
    English: Verify handling computer leave office event.
    """
    office_id = "test-office-leave"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    leave_data: LeaveOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-leave",
        "agent": None,
    }

    client.handle_computer_leave_office(leave_data)

    # 验证事件处理器被调用
    assert len(handler.leave_events) == 1
    assert handler.leave_events[0] == leave_data


def test_handle_computer_update_config():
    """
    中文：验证处理计算机更新配置事件。
    English: Verify handling computer update config event.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    update_data: UpdateMCPConfigNotification = {
        "computer": "test-computer-update",
    }

    client.handle_computer_update_config(update_data)

    # 验证事件处理器被调用
    assert len(handler.update_events) == 1
    assert handler.update_events[0] == update_data


def test_process_tools_response():
    """
    中文：验证处理工具响应。
    English: Verify processing tools response.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    tools = [
        SMCPTool(
            name="tool1",
            description="Test tool 1",
            params_schema={"type": "object"},
            return_schema=None,
        ),
        SMCPTool(
            name="tool2",
            description="Test tool 2",
            params_schema={"type": "string"},
            return_schema=None,
        ),
    ]

    response: GetToolsRet = {
        "tools": tools,
        "req_id": "test-req-id",
    }

    computer = "test-computer-tools"
    client.process_tools_response(response, computer)

    # 验证事件处理器被调用
    assert len(handler.tools_received) == 1
    assert handler.tools_received[0][0] == computer
    assert handler.tools_received[0][1] == tools


def test_process_tools_response_empty():
    """
    中文：验证处理空工具响应。
    English: Verify processing empty tools response.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    response: GetToolsRet = {
        "tools": [],  # 空工具列表
        "req_id": "test-req-id",
    }

    computer = "test-computer-empty"
    client.process_tools_response(response, computer)

    # 空工具列表不应该触发事件处理器
    assert len(handler.tools_received) == 0


def test_base_agent_client_without_event_handler():
    """
    中文：验证没有事件处理器的基础客户端。
    English: Verify base client without event handler.
    """
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id="test-office")
    client = MockAgentClient(auth_provider=auth, event_handler=None)

    # 这些操作不应该抛出异常，即使没有事件处理器
    enter_data: EnterOfficeNotification = {
        "office_id": "test-office",
        "computer": "test-computer",
        "agent": None,
    }

    client.handle_computer_enter_office(enter_data)

    update_data: UpdateMCPConfigNotification = {
        "computer": "test-computer",
    }

    client.handle_computer_update_config(update_data)

    # 验证没有异常抛出
    assert client.event_handler is None


def test_sio_param_passed_to_handlers():
    """
    中文：验证sio参数被正确传入所有事件处理器。
    English: Verify sio param is correctly passed to all event handlers.
    """
    office_id = "test-office-sio"
    auth = DefaultAgentAuthProvider(agent_id="test-agent", office_id=office_id)
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    # 测试enter_office / Test enter_office
    enter_data: EnterOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-1",
        "agent": None,
    }
    client.handle_computer_enter_office(enter_data)

    assert len(handler.enter_clients) == 1
    assert handler.enter_clients[0] is client
    assert isinstance(handler.enter_clients[0], MockAgentClient)

    # 测试leave_office / Test leave_office
    leave_data: LeaveOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer-2",
        "agent": None,
    }
    client.handle_computer_leave_office(leave_data)

    assert len(handler.leave_clients) == 1
    assert handler.leave_clients[0] is client

    # 测试update_config / Test update_config
    update_data: UpdateMCPConfigNotification = {
        "computer": "test-computer-3",
    }
    client.handle_computer_update_config(update_data)

    assert len(handler.update_clients) == 1
    assert handler.update_clients[0] is client

    # 测试tools_received / Test tools_received
    tools = [
        SMCPTool(
            name="tool1",
            description="Test tool 1",
            params_schema={"type": "object"},
            return_schema=None,
        ),
    ]
    response: GetToolsRet = {
        "tools": tools,
        "req_id": "test-req-id",
    }
    client.process_tools_response(response, "test-computer-4")

    assert len(handler.tools_clients) == 1
    assert handler.tools_clients[0] is client


def test_sio_param_client_properties_accessible():
    """
    中文：验证通过sio参数可以访问client的属性和方法。
    English: Verify client properties and methods are accessible via sio param.
    """
    office_id = "test-office-props"
    agent_id = "test-agent-props"
    auth = DefaultAgentAuthProvider(agent_id=agent_id, office_id=office_id)
    handler = MockEventHandler()
    client = MockAgentClient(auth_provider=auth, event_handler=handler)

    enter_data: EnterOfficeNotification = {
        "office_id": office_id,
        "computer": "test-computer",
        "agent": None,
    }
    client.handle_computer_enter_office(enter_data)

    # 验证可以通过sio访问client属性 / Verify can access client properties via sio
    passed_client = handler.enter_clients[0]

    # 验证auth_provider可访问 / Verify auth_provider is accessible
    assert hasattr(passed_client, "auth_provider")
    assert passed_client.auth_provider is not None
    agent_config = passed_client.auth_provider.get_agent_config()
    assert agent_config["agent"] == agent_id
    assert agent_config["office_id"] == office_id

    # 验证event_handler可访问 / Verify event_handler is accessible
    assert hasattr(passed_client, "event_handler")
    assert passed_client.event_handler is handler

    # 验证方法可调用 / Verify methods are callable
    assert hasattr(passed_client, "validate_emit_event")
    assert callable(passed_client.validate_emit_event)
