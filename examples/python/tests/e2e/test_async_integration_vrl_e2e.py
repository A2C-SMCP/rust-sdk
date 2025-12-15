# -*- coding: utf-8 -*-
# filename: test_async_integration_vrl_e2e.py
# @Time    : 2025/10/06 14:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: Computer-Agent-Server 三端联动的 VRL 功能端到端测试。
      验证 VRL 转换功能在完整工作流中的正确性：
      - Computer 配置 VRL 脚本
      - Agent 调用工具
      - Agent 收到的工具返回中包含 VRL 转换结果
English: Asynchronous end-to-end tests for VRL functionality in Computer-Agent-Server integration.
         Validates VRL transformation in complete workflow:
         - Computer configures VRL script
         - Agent calls tools
         - Agent receives tool results with VRL transformation
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path
from typing import Any

import pytest

from a2c_smcp.agent import AsyncSMCPAgentClient, DefaultAgentAuthProvider
from a2c_smcp.agent.types import AsyncAgentEventHandler
from a2c_smcp.computer import Computer
from a2c_smcp.computer.mcp_clients.model import A2C_VRL_TRANSFORMED, StdioServerConfig
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


def _create_mcp_config_with_vrl(name: str, script_path: str, vrl_script: str | None = None) -> dict[str, Any]:
    """
    中文: 创建带 VRL 配置的 MCP Server 配置
    English: Create MCP Server config with VRL script
    """
    project_root = Path(__file__).parent.parent.parent
    absolute_script_path = (project_root / script_path).resolve()

    config = {
        "name": name,
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": {"auto_apply": True},
        "server_parameters": {
            "command": sys.executable,
            "args": [str(absolute_script_path)],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }

    # 添加 VRL 配置 / Add VRL config
    if vrl_script:
        config["vrl"] = vrl_script

    return config


@pytest.mark.asyncio
async def test_async_integration_agent_receives_vrl_transformed_result(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 Computer 配置了 VRL 脚本
      - 验证 Agent 调用工具后收到的结果包含 VRL 转换数据
      - 验证 VRL 转换结果存储在 meta[a2c_vrl_transformed] 中
      - 验证转换后的数据结构符合 VRL 脚本定义
    English:
      - Verify Computer is configured with VRL script
      - Verify Agent receives VRL transformed result after tool call
      - Verify VRL transformation is stored in meta[a2c_vrl_transformed]
      - Verify transformed data structure matches VRL script definition
    """
    agent_id = "async-vrl-integration-office-1"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 配置 VRL 脚本：添加转换标记和额外字段
    # Configure VRL script: add transformation marker and extra fields
    vrl_script = """
.vrl_transformed = true
.transformation_timestamp = now()
.status = "processed"
.original_tool_name = .content[0].text
"""

    # 创建带 VRL 配置的 MCP Server
    # Create MCP Server config with VRL
    mcp_config = _create_mcp_config_with_vrl(
        "e2e-vrl-integration-server-1",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
        vrl_script=vrl_script,
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例
    # Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端
    # Create Agent client
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

        # 1. Agent 先连接并加入办公室
        # Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        ok, err = await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-vrl-1", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        assert ok is True, f"Agent join office failed: {err}"
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server
        # Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播
        # Wait for event propagation
        await asyncio.sleep(1.0)

        # 4. 验证 Agent 收到工具列表
        # Verify Agent received tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_name, tools = event_handler.tools_received_events[0]
        assert computer_name == "test", "回调工具列表时，使用的是computer name"

        # 5. Agent 调用工具
        # Agent calls tool
        result = await agent_client.emit_tool_call(
            computer=computer_name,
            tool_name="mark_a",
            params={},
            timeout=15,
        )

        # 6. 验证工具调用成功
        # Verify tool call succeeded
        assert result.isError is False, f"Tool call failed: {result}"
        assert len(result.content) >= 1
        assert "ok:mark_a" in result.content[0].text

        # 7. 验证 VRL 转换结果存在于 meta 中
        # Verify VRL transformation result exists in meta
        # 注意：由于Pydantic模型的序列化问题，meta可能在model_extra中
        # Note: Due to Pydantic model serialization, meta might be in model_extra
        result_dict = result.model_dump()
        print(f"[E2E VRL] Full result dict keys: {result_dict.keys()}")
        print(f"[E2E VRL] Result meta field: {result.meta}")

        # 尝试从多个位置获取meta数据
        # Try to get meta data from multiple locations
        meta_data = result.meta or result_dict.get("meta") or {}

        assert meta_data, f"Tool result meta is empty. Result dict: {result_dict}"
        assert A2C_VRL_TRANSFORMED in meta_data, (
            f"VRL transformation key '{A2C_VRL_TRANSFORMED}' not found in meta. Available keys: {list(meta_data.keys())}"
        )

        # 8. 解析 VRL 转换结果
        # Parse VRL transformation result
        vrl_result_json = meta_data[A2C_VRL_TRANSFORMED]
        assert isinstance(vrl_result_json, str), f"VRL result should be JSON string, got {type(vrl_result_json)}"

        vrl_result = json.loads(vrl_result_json)
        print(f"[E2E VRL] VRL transformation result: {vrl_result}")

        # 9. 验证 VRL 转换后的字段
        # Verify VRL transformed fields
        assert "vrl_transformed" in vrl_result, f"'vrl_transformed' field not found in VRL result: {vrl_result}"
        assert vrl_result["vrl_transformed"] is True, f"'vrl_transformed' should be True, got {vrl_result['vrl_transformed']}"

        assert "status" in vrl_result, f"'status' field not found in VRL result: {vrl_result}"
        assert vrl_result["status"] == "processed", f"'status' should be 'processed', got {vrl_result['status']}"

        assert "transformation_timestamp" in vrl_result, f"'transformation_timestamp' field not found in VRL result: {vrl_result}"

        assert "original_tool_name" in vrl_result, f"'original_tool_name' field not found in VRL result: {vrl_result}"
        assert "ok:mark_a" in vrl_result["original_tool_name"], (
            f"'original_tool_name' should contain 'ok:mark_a', got {vrl_result['original_tool_name']}"
        )

        print(f"[E2E VRL] Test completed successfully in {time.time() - start_time:.2f}s")

    finally:
        # 清理资源
        # Cleanup resources
        cleanup_start = time.time()

        await agent_client.disconnect()
        print(f"[E2E VRL] Agent disconnect took {time.time() - cleanup_start:.2f}s")

        await computer.shutdown()
        print(f"[E2E VRL] Computer shutdown took {time.time() - cleanup_start:.2f}s")


@pytest.mark.asyncio
async def test_async_integration_vrl_field_mapping_and_extraction(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 VRL 可以提取和重组工具返回的数据结构
      - 验证 Agent 可以获取经过 VRL 处理后的结构化数据
      - 验证复杂的 VRL 转换逻辑（字段提取、嵌套对象等）
    English:
      - Verify VRL can extract and reorganize tool return data structure
      - Verify Agent can receive VRL-processed structured data
      - Verify complex VRL transformation logic (field extraction, nested objects, etc.)
    """
    agent_id = "async-vrl-integration-office-2"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 配置复杂的 VRL 脚本：提取内容并重组为新结构
    # Configure complex VRL script: extract content and reorganize into new structure
    vrl_script = """
.result = {
    "success": true,
    "data": {
        "tool_output": .content[0].text,
        "content_type": .content[0].type
    },
    "metadata": {
        "is_error": .isError,
        "processed_at": now(),
        "processor": "vrl-e2e-test"
    }
}
.summary = "Tool executed successfully"
"""

    # 创建带 VRL 配置的 MCP Server
    # Create MCP Server config with VRL
    mcp_config = _create_mcp_config_with_vrl(
        "e2e-vrl-integration-server-2",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
        vrl_script=vrl_script,
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例
    # Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端
    # Create Agent client
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
        # 1. Agent 先连接并加入办公室
        # Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-vrl-2", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server
        # Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播
        # Wait for event propagation
        await asyncio.sleep(1.0)

        # 4. 验证 Agent 收到工具列表
        # Verify Agent received tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_name, tools = event_handler.tools_received_events[0]
        assert computer_name == "test", "回调工具列表时使用的是computer name"

        # 5. Agent 调用工具
        # Agent calls tool
        result = await agent_client.emit_tool_call(
            computer=computer_name,
            tool_name="mark_a",
            params={},
            timeout=15,
        )

        # 6. 验证工具调用成功
        # Verify tool call succeeded
        assert result.isError is False, f"Tool call failed: {result}"

        # 7. 验证 VRL 转换结果
        # Verify VRL transformation result
        result_dict = result.model_dump()
        meta_data = result.meta or result_dict.get("meta") or {}

        assert meta_data, f"Tool result meta is empty. Result dict: {result_dict}"
        assert A2C_VRL_TRANSFORMED in meta_data

        vrl_result = json.loads(meta_data[A2C_VRL_TRANSFORMED])
        print(f"[E2E VRL] Complex VRL transformation result: {json.dumps(vrl_result, indent=2)}")

        # 8. 验证重组后的数据结构
        # Verify reorganized data structure
        assert "result" in vrl_result, f"'result' field not found in VRL result: {vrl_result}"
        assert "success" in vrl_result["result"], "'result.success' not found"
        assert vrl_result["result"]["success"] is True

        assert "data" in vrl_result["result"], "'result.data' not found"
        assert "tool_output" in vrl_result["result"]["data"], "'result.data.tool_output' not found"
        assert "ok:mark_a" in vrl_result["result"]["data"]["tool_output"]

        assert "metadata" in vrl_result["result"], "'result.metadata' not found"
        assert "is_error" in vrl_result["result"]["metadata"], "'result.metadata.is_error' not found"
        assert vrl_result["result"]["metadata"]["is_error"] is False

        assert "summary" in vrl_result, f"'summary' field not found in VRL result: {vrl_result}"
        assert "successfully" in vrl_result["summary"].lower()

        print("[E2E VRL] Complex VRL transformation test completed successfully")

    finally:
        # 清理资源
        # Cleanup resources
        await agent_client.disconnect()
        await computer.shutdown()


@pytest.mark.asyncio
async def test_async_integration_vrl_preserves_original_result(
    async_integration_socketio_server,
    async_integration_server_port: int,
    tmp_path: Path,
):
    """
    中文:
      - 验证 VRL 转换不影响原始工具返回内容
      - 验证原始 content 字段保持不变
      - 验证 VRL 转换结果仅存储在 meta 中
    English:
      - Verify VRL transformation doesn't affect original tool return content
      - Verify original content field remains unchanged
      - Verify VRL transformation result is only stored in meta
    """
    agent_id = "async-vrl-integration-office-3"
    office_id = agent_id
    server_url = f"http://127.0.0.1:{async_integration_server_port}"

    # 配置 VRL 脚本
    # Configure VRL script
    vrl_script = '.vrl_marker = "this_is_vrl_transformed_data"\n.extra_info = {"key": "value"}'

    # 创建带 VRL 配置的 MCP Server
    # Create MCP Server config with VRL
    mcp_config = _create_mcp_config_with_vrl(
        "e2e-vrl-integration-server-3",
        "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
        vrl_script=vrl_script,
    )
    stdio_config = StdioServerConfig(**mcp_config)

    # 创建 Computer 实例
    # Create Computer instance
    computer = Computer(
        name="test",
        mcp_servers={stdio_config},
        auto_connect=True,
    )
    computer_client = SMCPComputerClient(computer=computer)

    # 创建 Agent 客户端
    # Create Agent client
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
        # 1. Agent 先连接并加入办公室
        # Agent connects and joins office first
        await agent_client.connect_to_server(server_url)
        await asyncio.sleep(0.2)

        await agent_client.call(
            JOIN_OFFICE_EVENT,
            {"role": "agent", "name": "test-agent-vrl-3", "office_id": office_id},
            namespace=SMCP_NAMESPACE,
            timeout=5,
        )
        await asyncio.sleep(0.3)

        # 2. Computer 启动并连接到 Server
        # Computer boots up and connects to Server
        await computer.boot_up()
        await computer_client.connect(
            server_url,
            socketio_path="/socket.io",
            namespaces=[SMCP_NAMESPACE],
            transports=["polling"],
        )
        await computer_client.join_office(office_id)

        # 3. 等待事件传播
        # Wait for event propagation
        await asyncio.sleep(1.0)

        # 4. 验证 Agent 收到工具列表
        # Verify Agent received tool list
        assert await _wait_until(lambda: len(event_handler.tools_received_events) >= 1, timeout=5)
        computer_name, tools = event_handler.tools_received_events[0]
        assert computer_name == "test", "回调获取工具列表时使用的是 computer name"

        # 5. Agent 调用工具
        # Agent calls tool
        result = await agent_client.emit_tool_call(
            computer=computer_name,
            tool_name="mark_a",
            params={},
            timeout=15,
        )

        # 6. 验证工具调用成功
        # Verify tool call succeeded
        assert result.isError is False, f"Tool call failed: {result}"

        # 7. 验证原始内容保持不变
        # Verify original content remains unchanged
        assert len(result.content) >= 1, "Tool result content is empty"
        assert "ok:mark_a" in result.content[0].text, f"Original content should contain 'ok:mark_a', got {result.content[0].text}"

        # 8. 验证 VRL 转换结果存在于 meta 中，但不影响 content
        # Verify VRL transformation exists in meta but doesn't affect content
        result_dict = result.model_dump()
        meta_data = result.meta or result_dict.get("meta") or {}

        assert meta_data, f"Tool result meta is empty. Result dict: {result_dict}"
        assert A2C_VRL_TRANSFORMED in meta_data

        vrl_result = json.loads(meta_data[A2C_VRL_TRANSFORMED])
        print(f"[E2E VRL] VRL result in meta: {vrl_result}")

        # 9. 验证 VRL 转换的字段
        # Verify VRL transformed fields
        assert "vrl_marker" in vrl_result
        assert vrl_result["vrl_marker"] == "this_is_vrl_transformed_data"
        assert "extra_info" in vrl_result
        assert vrl_result["extra_info"]["key"] == "value"

        # 10. 确认原始 content 中不包含 VRL 转换的字段
        # Confirm original content doesn't contain VRL transformed fields
        content_text = result.content[0].text
        assert "vrl_marker" not in content_text, "VRL transformed field should not appear in original content"
        assert "this_is_vrl_transformed_data" not in content_text, "VRL transformed value should not appear in original content"

        print("[E2E VRL] VRL preservation test completed successfully")

    finally:
        # 清理资源
        # Cleanup resources
        await agent_client.disconnect()
        await computer.shutdown()
