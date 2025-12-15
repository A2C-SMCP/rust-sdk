# -*- coding: utf-8 -*-
# filename: test_base_client.py
# @Time    : 2025/8/18 19:26
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
import asyncio
from asyncio import Queue
from contextlib import AsyncExitStack
from unittest.mock import AsyncMock, MagicMock

import pytest
from mcp import ClientSession
from mcp.types import Implementation, InitializeResult, ListToolsResult, ServerCapabilities
from pydantic import BaseModel
from transitions.core import MachineError

from a2c_smcp.computer.mcp_clients.base_client import STATES, BaseMCPClient


# 创建测试用的具体客户端实现
class MockMCPClient(BaseMCPClient):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._async_session = None
        self.prepare_called = False
        self.can_called = False
        self.before_called = False
        self.after_called = False
        self.on_enter_called = False
        self.disabled = False
        self.forbidden_tools = []
        self.tool_meta = {}
        self.aexit_stack = AsyncMock(spec=AsyncExitStack)

    async def _create_async_session(self) -> ClientSession:
        return MagicMock(spec=ClientSession)

    async def aprepare_connect(self, event):
        await super().aprepare_connect(event)
        self.prepare_called = True

    async def acan_connect(self, event):
        await super().acan_connect(event)
        self.can_called = True
        return True

    async def abefore_connect(self, event):
        await super().abefore_connect(event)
        self.before_called = True

    async def on_enter_connected(self, event):
        await super().on_enter_connected(event)

    async def aafter_connect(self, event):
        await super().aafter_connect(event)
        self.after_called = True

    async def on_enter_disconnected(self, event):
        self.on_enter_called = True
        await super().on_enter_disconnected(event)


# ------------------------------
# InitializeResult 相关的辅助类 / Helpers for InitializeResult related tests
# ------------------------------


class DummyParams(BaseModel):
    """
    中文: 伪造的参数模型，用于构造客户端实例。
    英文: Dummy parameter model used to construct client instance.
    """

    value: int = 1


class FakeClientSession:
    """
    中文: 伪造的 ClientSession，仅实现 initialize() 用于返回固定的 InitializeResult。
    英文: Fake ClientSession implementing initialize() to return a fixed InitializeResult.
    """

    def __init__(self) -> None:
        self.initialized = False

    async def initialize(self) -> InitializeResult:
        self.initialized = True
        return InitializeResult(
            protocolVersion="2025-06-18",
            capabilities=ServerCapabilities(),
            serverInfo=Implementation(name="fake", version="1.0.0"),
            instructions="test",
        )


class InitializeResultTestClient(BaseMCPClient):
    """
    中文: 用于测试的 BaseMCPClient 子类，返回 FakeClientSession，避免与现有 TestMCPClient 命名冲突。
    英文: Test subclass of BaseMCPClient returning a FakeClientSession, avoiding name conflict with TestMCPClient.
    """

    async def _create_async_session(self):
        # 这里不做额外上下文，仅返回伪造 session / No extra context, just return fake session
        return FakeClientSession()


# pytest fixture 创建测试客户端
@pytest.fixture
async def client():
    """创建一个测试用的 MCP 客户端实例"""
    state_changes = Queue()

    # 状态变化回调收集器
    async def state_change_callback(from_state, to_state):
        await state_changes.put((from_state, to_state))

    # 创建并返回客户端和状态变化队列
    client = MockMCPClient(params=MagicMock(), state_change_callback=state_change_callback)
    return client, state_changes


@pytest.fixture
async def connected_client(client):
    """返回已连接状态的客户端"""
    client, state_changes = client
    await client.aconnect()
    return client, state_changes


# 辅助函数用于从队列中获取状态变化
async def get_next_state_change(state_changes):
    """从状态变化队列中获取下一个变化，最多等待1秒"""
    return await asyncio.wait_for(state_changes.get(), timeout=1.0)


# 测试用例
@pytest.mark.asyncio
async def test_initial_state(client):
    """测试初始状态是否正确"""
    client_instance, _ = client
    assert client_instance.state == STATES.initialized


@pytest.mark.asyncio
async def test_successful_connection_flow(client):
    """测试完整的连接流程"""
    client_instance, state_changes = client

    # 触发连接
    await client_instance.aconnect()

    # 验证状态转换
    assert client_instance.state == STATES.connected

    # 验证回调函数被调用
    from_state, to_state = await get_next_state_change(state_changes)
    assert from_state == STATES.initialized.value
    assert to_state == STATES.connected.value

    # 验证各阶段回调被调用
    assert client_instance.prepare_called
    assert client_instance.can_called
    assert client_instance.before_called
    assert client_instance.after_called


@pytest.mark.asyncio
async def test_disconnection_flow(connected_client):
    """测试断开连接流程"""
    client_instance, state_changes = connected_client
    await state_changes.get()  # 清除初始连接状态变化

    # 触发断开连接
    await client_instance.adisconnect()

    # 验证状态转换
    assert client_instance.state == STATES.disconnected

    # 验证回调函数被调用
    from_state, to_state = await get_next_state_change(state_changes)
    assert from_state == STATES.connected
    assert to_state == STATES.disconnected

    # 验证断开时的资源清理
    assert client_instance._async_session is None
    assert client_instance.on_enter_called


@pytest.mark.asyncio
async def test_disconnect_from_disconnected_state(connected_client):
    """测试从断开状态断开连接"""
    client_instance, _ = connected_client
    await client_instance.adisconnect()
    assert client_instance.state == STATES.disconnected
    with pytest.raises(MachineError):
        await client_instance.adisconnect()


@pytest.mark.asyncio
async def test_error_transition(client):
    """测试错误状态转换"""
    client_instance, state_changes = client

    # 触发错误
    await client_instance.aerror()

    # 验证状态转换
    assert client_instance.state == STATES.error

    # 验证回调函数被调用
    from_state, to_state = await get_next_state_change(state_changes)
    assert from_state == STATES.initialized
    assert to_state == STATES.error


@pytest.mark.asyncio
async def test_async_session_property(client):
    """测试async_session属性"""
    client_instance, _ = client

    # 初始状态下会话为空
    assert client_instance._async_session is None

    # 访问属性触发连接
    session = await client_instance.async_session

    # 验证会话已创建
    assert session is not None

    # 验证状态转换
    assert client_instance.state == STATES.connected


@pytest.mark.asyncio
async def test_list_tools_success(connected_client):
    """测试list_tools成功场景"""
    client_instance, _ = connected_client
    # 模拟会话对象
    mock_session = client_instance._async_session
    ret_mock = MagicMock(spec=ListToolsResult)
    ret_mock.tools = []
    ret_mock.nextCursor = None
    mock_session.list_tools = AsyncMock(return_value=ret_mock)

    # 调用方法
    await client_instance.list_tools()

    # 验证正确调用
    mock_session.list_tools.assert_called()


@pytest.mark.asyncio
async def test_list_tools_error(client):
    """测试list_tools在未连接时抛出异常"""
    client_instance, _ = client
    with pytest.raises(ConnectionError, match="Not connected to server"):
        await client_instance.list_tools()


@pytest.mark.asyncio
async def test_call_tool_success(connected_client):
    """测试call_tool成功场景"""
    client_instance, _ = connected_client
    # 模拟会话对象
    mock_session = client_instance._async_session
    mock_session.call_tool = AsyncMock()

    # 调用方法
    await client_instance.call_tool("test_tool", {"param": "value"})

    # 验证正确调用
    mock_session.call_tool.assert_called_with("test_tool", {"param": "value"})


@pytest.mark.asyncio
async def test_call_tool_error(client):
    """测试call_tool在未连接时抛出异常"""
    client_instance, _ = client
    with pytest.raises(ConnectionError, match="Not connected to server"):
        await client_instance.call_tool("test_tool", {})


@pytest.mark.asyncio
async def test_failed_connection(client, mocker):
    """测试连接失败场景"""
    client_instance, state_changes = client

    # 模拟连接条件检查失败
    mocker.patch.object(MockMCPClient, "acan_connect", AsyncMock(return_value=False))

    # 触发连接
    await client_instance.aconnect()

    # 验证状态未变化
    assert client_instance.state == STATES.initialized

    # 验证没有状态变化回调
    assert state_changes.empty()


@pytest.mark.asyncio
async def test_transition_to_initialized(connected_client):
    """测试初始化状态转换"""
    client_instance, state_changes = connected_client
    await state_changes.get()  # 清除初始连接状态变化

    # 触发初始化
    await client_instance.ainitialize()

    # 验证状态转换
    assert client_instance.state == STATES.initialized

    # 验证回调函数被调用
    from_state, to_state = await get_next_state_change(state_changes)
    assert from_state == STATES.connected
    assert to_state == STATES.initialized

    # 验证资源被清理
    assert client_instance._async_session is None


@pytest.mark.asyncio
async def test_enter_error_state_releases_resources(client):
    """测试进入错误状态时释放资源"""
    client_instance, state_changes = client

    # 设置会话对象存在
    mock_session = AsyncMock()
    client_instance._async_session = mock_session

    # 触发错误状态
    await client_instance.aerror()

    # 确保进入错误状态时的清理被调用
    assert client_instance._async_session is not None
    # 目前 aerror 经过优化与迭代，不再显式调用 aclose 方法，而是根据目前的任务状态动态判断调用。
    client_instance.aexit_stack.aclose.assert_not_called()
    assert client_instance.state == STATES.error


# ------------------------------
# InitializeResult 相关测试 / Tests for InitializeResult logic
# ------------------------------


@pytest.mark.asyncio
async def test_initialize_result_set_and_cleared_on_disconnect():
    """
    中文: 连接后应设置 initialize_result；断开后应清理为 None。
    英文: After connect, initialize_result should be set; after disconnect, it should be cleared to None.
    """
    client = InitializeResultTestClient(DummyParams())

    # 连接 / connect
    await client.aconnect()

    # 等待会话创建成功 / wait for session creation
    await client._create_session_success_event.wait()

    # 验证初始化结果存在 / ensure initialize_result is present
    assert client.initialize_result is not None
    assert client.initialize_result.serverInfo.name == "fake"

    # 断开 / disconnect
    await client.adisconnect()

    # 等待会话真正关闭 / wait for actual session teardown
    await client._async_session_closed_event.wait()

    # 验证初始化结果被清理 / ensure initialize_result is cleared
    assert client.initialize_result is None


@pytest.mark.asyncio
async def test_initialize_result_cleared_on_error():
    """
    中文: 出错流程中也应清理 initialize_result。
    英文: initialize_result should be cleared in error path as well.
    """
    client = InitializeResultTestClient(DummyParams())

    # 连接 / connect
    await client.aconnect()
    await client._create_session_success_event.wait()
    assert client.initialize_result is not None

    # 触发错误 / trigger error (will cancel keep-alive and teardown session)
    await client.aerror()

    # 等待会话真正关闭 / wait for actual session teardown
    await client._async_session_closed_event.wait()

    # 应被清理 / should be cleared
    assert client.initialize_result is None
