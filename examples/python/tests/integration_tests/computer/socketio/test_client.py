# -*- coding: utf-8 -*-
# filename: test_client.py
# @Time    : 2025/8/21 13:56
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

import asyncio
import socket
from collections.abc import AsyncGenerator
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest
from mcp import StdioServerParameters
from mcp.client.session_group import SseServerParameters, StreamableHttpParameters
from mcp.types import CallToolResult, TextContent
from socketio import ASGIApp

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.manager import MCPServerManager
from a2c_smcp.computer.mcp_clients.model import SseServerConfig, StdioServerConfig, StreamableHttpServerConfig, ToolMeta
from a2c_smcp.computer.socketio.client import SMCPComputerClient
from a2c_smcp.smcp import GET_CONFIG_EVENT, GET_TOOLS_EVENT, SMCP_NAMESPACE, TOOL_CALL_EVENT
from a2c_smcp.utils.logger import logger
from tests.integration_tests.computer.socketio.mock_uv_server import UvicornTestServer
from tests.integration_tests.mock_socketio_server import MockComputerServerNamespace, create_computer_test_socketio


@pytest.fixture
def basic_server_port() -> int:
    """Find an available port for the basic server."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def computer() -> Computer:
    """创建一个模拟的Computer对象"""
    mock_computer = MagicMock(spec=Computer)
    mock_computer.mcp_manager = MagicMock(spec=MCPServerManager)
    mock_computer.mcp_manager.aexecute_tool = AsyncMock(
        return_value=CallToolResult(isError=False, content=[TextContent(text="成功执行", type="text")]),
    )
    mock_computer.aget_available_tools = AsyncMock(return_value=[])
    return mock_computer


@pytest.fixture
async def computer_server(basic_server_port: int) -> AsyncGenerator[MockComputerServerNamespace, Any]:
    """启动测试服务器"""
    sio = create_computer_test_socketio()
    sio.eio.start_service_task = False
    asgi_app = ASGIApp(sio, socketio_path="/socket.io")
    server = UvicornTestServer(asgi_app, port=basic_server_port)
    await server.up()
    yield sio.namespace_handlers[SMCP_NAMESPACE]  # 返回命名空间处理器以便测试中访问
    # 强制快速关闭，不等待连接清理 / Force fast shutdown without waiting for connection cleanup
    await server.down(force=True)


@pytest.mark.asyncio
async def test_computer_join_and_leave_office(computer, computer_server: MockComputerServerNamespace, basic_server_port: int):
    """测试加入和离开办公室"""
    computer_name = "test_computer"
    computer.name = computer_name
    client = SMCPComputerClient(computer=computer)
    logger.info(f"client sid: {client.sid}")

    # 连接服务器
    await client.connect(
        f"http://localhost:{basic_server_port}",
        headers={"mock_header": "mock_value"},
        auth={"mock_header": "mock_value"},
        socketio_path="/socket.io",
        namespaces=[SMCP_NAMESPACE],
    )

    # 验证动作状态
    await asyncio.sleep(0.5)  # 等待事件处理，交出一下控制权
    logger.info(f"client sid: {client.namespaces[SMCP_NAMESPACE]}")
    assert computer_server.client_operations_record[client.namespaces[SMCP_NAMESPACE]] == ("connect", None)

    # 加入办公室
    office_id = "test_office"
    await client.join_office(office_id)
    await asyncio.sleep(0.1)  # 等待事件处理，交出一下控制权
    assert computer_server.client_operations_record[client.namespaces[SMCP_NAMESPACE]] == ("enter_room", office_id)

    # 验证状态
    assert client.office_id == office_id

    # 离开办公室
    await client.leave_office(office_id)
    await asyncio.sleep(0.5)  # 等待事件处理，交出一下控制权
    assert computer_server.client_operations_record[client.namespaces[SMCP_NAMESPACE]] == ("leave_room", office_id)

    # 验证状态
    assert client.office_id is None

    # 断开连接
    await client.disconnect()


@pytest.mark.asyncio
async def test_computer_receives_tool_call(computer, computer_server, basic_server_port: int):
    """测试收到工具调用请求"""
    computer.name = "test_computer"
    client = SMCPComputerClient(computer=computer)
    client_connected_event = asyncio.Event()

    async def run_client():
        logger.debug("run_client")
        await client.connect(
            f"http://localhost:{basic_server_port}",
            socketio_path="/socket.io",
            headers={"mock_header": "mock_value"},
            auth={"mock_header": "mock_value"},
            namespaces=[SMCP_NAMESPACE],
        )
        await client.join_office("test_office")
        logger.debug("client connected and joined office")
        client_connected_event.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            ...
        finally:
            await client.disconnect()

    run_client_task = asyncio.create_task(run_client())

    # 等待客户端连接
    await client_connected_event.wait()

    # 模拟工具调用请求
    tool_call_req = {
        "computer": "test_computer",
        "tool_name": "test_tool",
        "params": {"param1": "value1"},
        "agent": "test_office",
        "req_id": "test_req_id",
        "timeout": 10,
    }

    await computer_server.emit(TOOL_CALL_EVENT, tool_call_req, to=client.namespaces[SMCP_NAMESPACE], namespace=SMCP_NAMESPACE)
    await asyncio.sleep(0.5)

    # 验证工具调用被正确处理
    assert computer.aexecute_tool.called
    computer.aexecute_tool.assert_called_with(req_id="test_req_id", tool_name="test_tool", parameters={"param1": "value1"}, timeout=10)

    # 取消客户端任务
    run_client_task.cancel()


@pytest.mark.asyncio
async def test_computer_sends_update_mcp_config(computer, computer_server, basic_server_port: int):
    """测试发送更新MCP配置事件"""
    computer_name = "test_computer"
    computer.name = computer_name
    client = SMCPComputerClient(computer=computer)

    await client.connect(
        f"http://localhost:{basic_server_port}",
        socketio_path="/socket.io",
        headers={"mock_header": "mock_value"},
        auth={"mock_header": "mock_value"},
        namespaces=[SMCP_NAMESPACE],
    )
    logger.info(f"[DEBUG] Computer name: {computer_name}")
    await client.join_office("test_office")

    # 发送更新MCP配置事件
    await client.update_config()

    computer_sid = client.namespaces[SMCP_NAMESPACE]
    logger.info(f"[DEBUG] Computer SID in UpdateComputerConfigReq: {computer_sid}")

    # 等待事件处理
    await asyncio.sleep(0.5)

    assert computer_server.client_operations_record[client.namespaces[SMCP_NAMESPACE]] == (
        "server_update_config",
        {"computer": computer.name},
    )

    await client.disconnect()


@pytest.mark.asyncio
async def test_computer_handles_get_tools_request(computer, computer_server, basic_server_port: int):
    """测试处理获取工具请求"""
    computer.name = "test_computer"
    client = SMCPComputerClient(computer=computer)

    await client.connect(
        f"http://localhost:{basic_server_port}",
        socketio_path="/socket.io",
        headers={"mock_header": "mock_value"},
        auth={"mock_header": "mock_value"},
        namespaces=[SMCP_NAMESPACE],
    )
    await client.join_office("test_office")

    # 模拟获取工具请求
    get_tools_req = {"computer": "test_computer", "agent": "test_office", "req_id": "test_req_id"}

    # 发送获取工具请求（模拟Agent的行为）
    await computer_server.emit(GET_TOOLS_EVENT, get_tools_req, namespace=SMCP_NAMESPACE, to=client.namespaces[SMCP_NAMESPACE])
    await asyncio.sleep(0.5)

    # 验证Computer的方法是否被调用
    assert computer.aget_available_tools.called

    await client.disconnect()


@pytest.mark.asyncio
async def test_computer_handles_tool_call_timeout(computer, computer_server, basic_server_port: int):
    """测试工具调用超时处理"""
    computer.name = "test_computer"
    # 配置模拟工具调用超时
    computer.mcp_manager.aexecute_tool = AsyncMock(side_effect=asyncio.TimeoutError)

    client = SMCPComputerClient(computer=computer)

    await client.connect(
        f"http://localhost:{basic_server_port}",
        socketio_path="/socket.io",
        headers={"mock_header": "mock_value"},
        auth={"mock_header": "mock_value"},
        namespaces=[SMCP_NAMESPACE],
    )
    await client.join_office("test_office")

    # 模拟工具调用请求（标记为应该超时）
    tool_call_req = {
        "computer": "test_computer",
        "tool_name": "test_tool",
        "params": {"param1": "value1"},
        "agent": "test_office",
        "req_id": "test_req_id",
        "timeout": 10,
    }

    # 发送工具调用请求
    try:
        await computer_server.emit(TOOL_CALL_EVENT, tool_call_req, namespace=SMCP_NAMESPACE, to=client.namespaces[SMCP_NAMESPACE])
    except Exception as e:
        logger.error(f"工具调用出错: {e}")

    # 等待处理
    await asyncio.sleep(0.1)

    # 验证返回了超时结果
    computer.aexecute_tool.assert_called()

    await client.disconnect()


@pytest.mark.asyncio
async def test_computer_handles_get_config(computer_server: MockComputerServerNamespace, basic_server_port: int):
    """测试处理获取MCP配置请求 / Handle GET_CONFIG_EVENT and validate response"""
    computer_name = "test_computer"
    # 构造具备 mcp_servers 属性的 Computer 替身 / Fake Computer with mcp_servers
    stdio_cfg = StdioServerConfig(
        name="stdio-srv",
        server_parameters=StdioServerParameters(command="bash", args=["-lc", "echo hi"], env={}),
        forbidden_tools=["ban1"],
        tool_meta={"toolA": ToolMeta(auto_apply=True)},
    )
    sse_cfg = SseServerConfig(
        name="sse-srv",
        server_parameters=SseServerParameters(url="http://localhost:18080/sse"),
        forbidden_tools=[],
        tool_meta={},
    )
    http_cfg = StreamableHttpServerConfig(
        name="http-srv",
        server_parameters=StreamableHttpParameters(url="http://localhost:18081"),
        forbidden_tools=[],
        tool_meta={},
    )

    class _FakeComputer:
        @property
        def mcp_servers(self):
            # 返回不可变元组 / immutable tuple
            return (stdio_cfg, sse_cfg, http_cfg)

        @property
        def inputs(self) -> list:
            return []

        @property
        def name(self) -> str:
            return computer_name

        # 兼容其他测试中会用到的方法（此测试用例中不会调用）/ compatibility no-op
        aget_available_tools = AsyncMock(return_value=[])
        mcp_manager = MagicMock()

    client = SMCPComputerClient(computer=_FakeComputer())

    await client.connect(
        f"http://localhost:{basic_server_port}",
        socketio_path="/socket.io",
        headers={"mock_header": "mock_value"},
        auth={"mock_header": "mock_value"},
        namespaces=[SMCP_NAMESPACE],
    )
    logger.info(f"[DEBUG] Computer name: {computer_name}")
    await client.join_office("test_office")

    # 通过服务端命名空间的 call，向客户端发起 GET_CONFIG_EVENT 并等待回调返回
    # Use server-side namespace.call to request and await client's callback response
    computer_sid = client.namespaces[SMCP_NAMESPACE]
    logger.info(f"[DEBUG] Computer SID in GetComputerConfigReq: {computer_sid}")
    req = {"computer": computer_name, "agent": "test_office"}
    resp = await computer_server.call(
        GET_CONFIG_EVENT,
        req,
        to=client.namespaces[SMCP_NAMESPACE],
        namespace=SMCP_NAMESPACE,
    )

    # 校验返回结构 / Validate response structure
    assert "servers" in resp
    servers = resp["servers"]
    assert set(servers.keys()) == {"stdio-srv", "sse-srv", "http-srv"}

    assert servers["stdio-srv"]["type"] == "stdio"
    assert servers["sse-srv"]["type"] == "sse"
    assert servers["http-srv"]["type"] == "streamable"

    assert servers["stdio-srv"]["forbidden_tools"] == ["ban1"]
    assert "toolA" in servers["stdio-srv"]["tool_meta"]

    assert isinstance(servers["stdio-srv"]["server_parameters"], dict)
    assert isinstance(servers["sse-srv"]["server_parameters"], dict)
    assert isinstance(servers["http-srv"]["server_parameters"], dict)

    await client.disconnect()
