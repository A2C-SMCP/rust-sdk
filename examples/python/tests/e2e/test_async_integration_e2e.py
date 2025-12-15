# -*- coding: utf-8 -*-
# filename: test_async_integration_e2e.py
# @Time    : 2025/10/05 16:20
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: Computer-Agent-Server 三者联动的异步端到端测试。
      验证完整的工作流：Computer 加入 -> Agent 获取工具列表 -> Agent 调用工具 -> 验证桌面同步。
English: Asynchronous end-to-end tests for Computer-Agent-Server integration.
         Validates complete workflow: Computer joins -> Agent gets tools -> Agent calls tools -> Verify desktop sync.
"""

from __future__ import annotations

import asyncio
import sys
import time
from pathlib import Path
from typing import Any

import pytest

from a2c_smcp.agent import AsyncSMCPAgentClient, DefaultAgentAuthProvider
from a2c_smcp.agent.types import AsyncAgentEventHandler
from a2c_smcp.computer import Computer
from a2c_smcp.computer.mcp_clients.model import StdioServerConfig
from a2c_smcp.computer.socketio.client import SMCPComputerClient
from a2c_smcp.smcp import (
    JOIN_OFFICE_EVENT,
    SMCP_NAMESPACE,
    EnterOfficeNotification,
    LeaveOfficeNotification,
    SMCPTool,
    UpdateMCPConfigNotification,
)

pytestmark = pytest.mark.e2e


async def _wait_until(cond, timeout: float = 3.0, step: float = 0.01) -> bool:
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


class MockAsyncEventHandler(AsyncAgentEventHandler):
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


def _create_mcp_config(name: str, script_path: str) -> dict[str, Any]:
    """
    中文: 创建 MCP Server 配置
    English: Create MCP Server config
    """
    # 将相对路径转换为绝对路径，确保在不同工作目录下都能正确找到文件
    # Convert relative path to absolute path to ensure file can be found in different working directories
    project_root = Path(__file__).parent.parent.parent  # 从 tests/e2e/ 回到项目根目录 / Go back to project root from tests/e2e/
    absolute_script_path = (project_root / script_path).resolve()

    return {
        "name": name,
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": {"auto_apply": True},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [str(absolute_script_path)],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }


@pytest.mark.asyncio
async def test_async_integration_computer_agent_server_basic_flow(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 Computer 连接到 Server 并加入办公室
      - 验证 Agent 连接到 Server 并加入同一办公室
      - 验证 Agent 收到 Computer 加入通知
      - 验证 Agent 自动获取 Computer 的工具列表
      - 验证工具列表包含预期的工具
    English:
      - Verify Computer connects to Server and joins office
      - Verify Agent connects to Server and joins same office
      - Verify Agent receives Computer join notification
      - Verify Agent automatically fetches Computer's tool list
      - Verify tool list contains expected tools
    """
    # 在 SMCP 协议中，agent_id 和 office_id 必须保持一致 / In SMCP protocol, agent_id and office_id must be consistent
    agent_id = "async-integration-office-1"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 创建 MCP Server 配置 / Create MCP Server config
    mcp_config = _create_mcp_config(
        "e2e-async-integration-server",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端 / Create Agent client
    auth_provider = DefaultAgentAuthProvider(
        agent_id=agent_id,
        office_id=office_id,
    )
    event_handler = MockAsyncEventHandler()
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        start_time = time.time()

        # 1. Agent 先连接并加入办公室 / Agent connects and joins office first
        t1 = time.time()
        await agent_client.connect_to_server(server_url)
        print(f"[E2E] Agent connect took {time.time() - t1:.2f}s")
        await asyncio.sleep(0.2)

        t2 = time.time()
        ok, err = await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-1", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        print(f"[E2E] Agent join office took {time.time() - t2:.2f}s")
        assert ok is True, f"Agent join office failed: {err}"
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server / Computer boots up and connects to Server
        t3 = time.time()
        await computer.boot_up()
        print(f"[E2E] Computer boot_up took {time.time() - t3:.2f}s")

        t4 = time.time()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        print(f"[E2E] Computer connect took {time.time() - t4:.2f}s")

        t5 = time.time()
        await computer_client.join_office(office_id)
        print(f"[E2E] Computer join office took {time.time() - t5:.2f}s")

        # 3. 等待事件传播 / Wait for event propagation
        await asyncio.sleep(1.0)
        print(f"[E2E] Setup phase took {time.time() - start_time:.2f}s")

        # 4. 验证 Agent 收到 Computer 加入通知 / Verify Agent received Computer join notification
        assert await _wait_until(lambda: len(event_handler.enter_office_events) >= 1, timeout=5), (
            "Agent did not receive Computer enter office notification"
        )

        enter_event = event_handler.enter_office_events[0]
        assert enter_event["office_id"] == office_id
        assert "computer" in enter_event

        # 5. 验证 Agent 自动获取了工具列表 / Verify Agent automatically fetched tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5), "Agent did not receive tools list"

        computer_sid, tools = event_handler.tools_received_events[0]
        assert len(tools) >= 1, f"Expected at least 1 tool, got {len(tools)}"

        # 验证工具列表包含 mark_a 工具 / Verify tool list contains mark_a tool
        tool_names = [t["name"] for t in tools]
        assert "mark_a" in tool_names, f"Expected 'mark_a' in tools, got {tool_names}"

        print(f"[E2E] Test assertions completed in {time.time() - start_time:.2f}s")

    finally:
        # 清理资源 / Cleanup resources
        cleanup_start = time.time()

        t_agent = time.time()
        await agent_client.disconnect()
        print(f"[E2E] Agent disconnect took {time.time() - t_agent:.2f}s")

        # 显式调用 Computer.shutdown() 清理 MCP Server 进程
        # 注意：跳过 computer_client.disconnect()，因为它会等待30秒超时
        # Explicitly call Computer.shutdown() to cleanup MCP Server processes
        # Note: Skip computer_client.disconnect() as it waits for 30s timeout
        t_shutdown = time.time()
        await computer.shutdown()
        print(f"[E2E] Computer shutdown took {time.time() - t_shutdown:.2f}s")

        print(f"[E2E] Cleanup phase took {time.time() - cleanup_start:.2f}s")


@pytest.mark.asyncio
async def test_async_integration_agent_call_computer_tool(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 Agent 可以调用 Computer 上的工具
      - 验证工具调用返回正确结果
      - 验证工具调用后可以获取桌面信息
    English:
      - Verify Agent can call tools on Computer
      - Verify tool call returns correct results
      - Verify desktop info can be retrieved after tool call
    """
    # 在 SMCP 协议中，agent_id 和 office_id 必须保持一致 / In SMCP protocol, agent_id and office_id must be consistent
    agent_id = "async-integration-office-2"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 创建 MCP Server 配置 / Create MCP Server config
    mcp_config = _create_mcp_config(
        "e2e-async-integration-server-2",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端 / Create Agent client
    auth_provider = DefaultAgentAuthProvider(
        agent_id=agent_id,
        office_id=office_id,
    )
    event_handler = MockAsyncEventHandler()
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 1. Agent 先连接并加入办公室 / Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-2", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server / Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播 / Wait for event propagation
        await asyncio.sleep(1.0)

        # 等待 Agent 收到工具列表 / Wait for Agent to receive tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_sid, tools = event_handler.tools_received_events[0]

        # 2. Agent 调用 mark_a 工具 / Agent calls mark_a tool
        result = await agent_client.emit_tool_call(
            computer=computer_sid,
            tool_name="mark_a",
            params={},
            timeout=15,  # 增加超时时间 / Increase timeout
        )

        # 3. 验证工具调用结果 / Verify tool call result
        assert result.isError is False, f"Tool call failed: {result}"
        assert len(result.content) >= 1
        # 工具应该返回 "ok:mark_a" / Tool should return "ok:mark_a"
        assert "ok:mark_a" in result.content[0].text

        # 4. 获取桌面信息 / Get desktop info
        desktop_response = await agent_client.get_desktop_from_computer(computer_sid, timeout=10)
        assert "desktops" in desktop_response
        desktops = desktop_response["desktops"]

        # 验证桌面包含 window:// 资源 / Verify desktop contains window:// resources
        assert len(desktops) >= 1, f"Expected at least 1 desktop window, got {len(desktops)}"
        assert any("window://" in d for d in desktops), f"Expected window:// in desktops, got {desktops}"

    finally:
        # 清理资源 / Cleanup resources
        cleanup_start = time.time()
        print("[E2E Test2] Starting cleanup...")

        # 先断开 Agent，避免在 Computer 断开时收到错误通知
        # Disconnect Agent first to avoid error notifications when Computer disconnects
        t1 = time.time()
        await agent_client.disconnect()
        print(f"[E2E Test2] Agent disconnect took {time.time() - t1:.2f}s")
        await asyncio.sleep(0.2)  # 等待断开完成 / Wait for disconnect to complete

        # 然后断开 Computer
        # Then disconnect Computer
        if computer_client.connected:
            t2 = time.time()
            await computer_client.leave_office(office_id)
            print(f"[E2E Test2] Computer leave_office took {time.time() - t2:.2f}s")

        # 显式调用 Computer.shutdown() 清理 MCP Server 进程
        # 注意：不调用 computer_client.disconnect()，因为它会等待30秒超时
        # Explicitly call Computer.shutdown() to cleanup MCP Server processes
        # Note: Skip computer_client.disconnect() as it waits for 30s timeout
        t3 = time.time()
        await computer.shutdown()
        print(f"[E2E Test2] Computer shutdown took {time.time() - t3:.2f}s")

        print(f"[E2E Test2] Total cleanup took {time.time() - cleanup_start:.2f}s")


@pytest.mark.asyncio
async def test_async_integration_computer_leave_notification(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 Computer 离开办公室时 Agent 收到通知
      - 验证通知包含正确的办公室 ID 和 Computer 信息
    English:
      - Verify Agent receives notification when Computer leaves office
      - Verify notification contains correct office ID and Computer info
    """
    # 在 SMCP 协议中，agent_id 和 office_id 必须保持一致 / In SMCP protocol, agent_id and office_id must be consistent
    agent_id = "async-integration-office-3"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 创建 MCP Server 配置 / Create MCP Server config
    mcp_config = _create_mcp_config(
        "e2e-async-integration-server-3",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端 / Create Agent client
    auth_provider = DefaultAgentAuthProvider(
        agent_id=agent_id,
        office_id=office_id,
    )
    event_handler = MockAsyncEventHandler()
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 1. Agent 先连接并加入办公室 / Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-3", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server / Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播 / Wait for event propagation
        await asyncio.sleep(1.0)

        # 等待 Agent 收到加入通知并完成工具获取 / Wait for Agent to receive join notification and complete tool fetching
        assert await _wait_until(lambda: len(event_handler.enter_office_events) >= 1, timeout=5)
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5), (
            "Agent did not receive tools from Computer"
        )

        # 3. Computer 离开办公室 / Computer leaves office
        await computer_client.leave_office(office_id)
        t = time.time()
        await computer_client.disconnect()
        print(f"[E2E Test2] Computer Client disconnect took {time.time() - t:.2f}s")

        # 3. 验证 Agent 收到离开通知 / Verify Agent received leave notification
        assert await _wait_until(lambda: len(event_handler.leave_office_events) >= 1, timeout=5), (
            "Agent did not receive Computer leave office notification"
        )

        leave_event = event_handler.leave_office_events[0]
        assert leave_event["office_id"] == office_id
        assert "computer" in leave_event
    except Exception as e:
        print(f"测试未通过 : {e}")
        pytest.fail(f"Error: {e}")
    finally:
        # 清理资源 / Cleanup resources
        await agent_client.disconnect()

        # 显式调用 Computer.shutdown() 清理 MCP Server 进程
        # 注意：跳过 computer_client.disconnect()，因为它会等待30秒超时
        # Explicitly call Computer.shutdown() to cleanup MCP Server processes
        # Note: Skip computer_client.disconnect() as it waits for 30s timeout
        await computer.shutdown()


@pytest.mark.asyncio
async def test_async_integration_multiple_tool_calls(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 Agent 可以连续调用多个工具
      - 验证每次调用都能正确返回结果
      - 验证并发调用的正确性
    English:
      - Verify Agent can call multiple tools consecutively
      - Verify each call returns correct results
      - Verify correctness of concurrent calls
    """
    # 在 SMCP 协议中，agent_id 和 office_id 必须保持一致 / In SMCP protocol, agent_id and office_id must be consistent
    agent_id = "async-integration-office-4"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 创建 MCP Server 配置 / Create MCP Server config
    mcp_config = _create_mcp_config(
        "e2e-async-integration-server-4",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端 / Create Agent client
    auth_provider = DefaultAgentAuthProvider(
        agent_id=agent_id,
        office_id=office_id,
    )
    event_handler = MockAsyncEventHandler()
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 1. Agent 先连接并加入办公室 / Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-4", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server / Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播 / Wait for event propagation
        await asyncio.sleep(1.0)

        # 等待 Agent 收到工具列表 / Wait for Agent to receive tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_sid, tools = event_handler.tools_received_events[0]

        # 2. 连续调用工具多次 / Call tool multiple times
        results = []
        for _ in range(3):
            result = await agent_client.emit_tool_call(
                computer=computer_sid,
                tool_name="mark_a",
                params={},
                timeout=15,  # 增加超时时间 / Increase timeout
            )
            results.append(result)
            await asyncio.sleep(0.3)  # 增加延迟避免过快调用 / Increase delay to avoid rapid calls

        # 3. 验证所有调用都成功 / Verify all calls succeeded
        assert len(results) == 3
        for i, result in enumerate(results):
            assert result.isError is False, f"Tool call {i} failed: {result}"
            assert len(result.content) >= 1
            assert "ok:mark_a" in result.content[0].text

        # 4. 测试并发调用 / Test concurrent calls
        concurrent_tasks = [
            agent_client.emit_tool_call(
                computer=computer_sid,
                tool_name="mark_a",
                params={},
                timeout=15,  # 增加超时时间 / Increase timeout
            )
            for _ in range(3)
        ]
        concurrent_results = await asyncio.gather(*concurrent_tasks)

        # 验证并发调用都成功 / Verify concurrent calls succeeded
        assert len(concurrent_results) == 3
        for i, result in enumerate(concurrent_results):
            assert result.isError is False, f"Concurrent tool call {i} failed: {result}"
            assert len(result.content) >= 1
            assert "ok:mark_a" in result.content[0].text
    except Exception as e:
        print(f"测试未通过 : {e}")
        pytest.fail(f"Error: {e}")
    finally:
        # 清理资源 / Cleanup resources
        # 先断开 Agent / Disconnect Agent first
        await agent_client.disconnect()
        await asyncio.sleep(0.2)

        # 然后断开 Computer / Then disconnect Computer
        if computer_client.connected:
            await computer_client.leave_office(office_id)
            await asyncio.sleep(0.1)
            await computer_client.disconnect()
        await asyncio.sleep(0.2)


@pytest.mark.asyncio
async def test_async_integration_desktop_sync_after_tool_call(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证调用工具后桌面信息保持同步
      - 验证 Agent 可以获取最新的桌面状态
    English:
      - Verify desktop info stays synced after tool calls
      - Verify Agent can retrieve latest desktop state
    """
    # 在 SMCP 协议中，agent_id 和 office_id 必须保持一致 / In SMCP protocol, agent_id and office_id must be consistent
    agent_id = "async-integration-office-5"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 创建 MCP Server 配置 / Create MCP Server config
    mcp_config = _create_mcp_config(
        "e2e-async-integration-server-5",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端 / Create Agent client
    auth_provider = DefaultAgentAuthProvider(
        agent_id=agent_id,
        office_id=office_id,
    )
    event_handler = MockAsyncEventHandler()
    agent_client = AsyncSMCPAgentClient(
        auth_provider=auth_provider,
        event_handler=event_handler,
    )

    try:
        # 1. Agent 先连接并加入办公室 / Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-async-5", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server / Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播 / Wait for event propagation
        await asyncio.sleep(1.0)

        # 等待 Agent 收到工具列表 / Wait for Agent to receive tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_sid, tools = event_handler.tools_received_events[0]

        # 2. 获取初始桌面状态 / Get initial desktop state
        desktop_before = await agent_client.get_desktop_from_computer(computer_sid, timeout=10)
        assert "desktops" in desktop_before

        # 3. 调用工具 / Call tool
        result = await agent_client.emit_tool_call(
            computer=computer_sid,
            tool_name="mark_a",
            params={},
            timeout=15,  # 增加超时时间 / Increase timeout
        )
        assert result.isError is False

        # 4. 再次获取桌面状态 / Get desktop state again
        await asyncio.sleep(0.5)  # 增加等待时间 / Increase wait time
        desktop_after = await agent_client.get_desktop_from_computer(computer_sid, timeout=10)
        assert "desktops" in desktop_after
        desktops_after = desktop_after["desktops"]

        # 5. 验证桌面信息一致性 / Verify desktop info consistency
        # 桌面窗口应该保持一致（因为我们的测试 MCP Server 返回固定的窗口列表）
        # Desktop windows should be consistent (our test MCP Server returns fixed window list)
        assert len(desktops_after) >= 1
        assert any("window://" in d for d in desktops_after)

    finally:
        # 清理资源 / Cleanup resources
        # 先断开 Agent / Disconnect Agent first
        await agent_client.disconnect()
        await asyncio.sleep(0.2)

        # 然后断开 Computer / Then disconnect Computer
        if computer_client.connected:
            await computer_client.leave_office(office_id)
            await asyncio.sleep(0.1)
            await computer_client.disconnect()
        await asyncio.sleep(0.2)
