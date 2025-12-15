# -*- coding: utf-8 -*-
# filename: test_namespace_async.py
# @Time    : 2025/09/30 23:20
# @Author  : A2C-SMCP
# @Software: PyCharm
"""
中文：针对 `a2c_smcp/server/namespace.py` 的异步命名空间集成测试。
English: Integration tests for async SMCPNamespace in `a2c_smcp/server/namespace.py`.

说明：
- 复用全局 fixtures：`socketio_server`, `basic_server_port`。
- 服务器端命名空间来自 `tests/integration_tests/mock_socketio_server.py` 的 `MockComputerServerNamespace`，不做修改。
- 客户端使用 socketio.AsyncClient 直接与服务端交互，验证服务端行为。
"""

import asyncio
from typing import Literal

import pytest
from mcp.types import CallToolResult, TextContent
from socketio import AsyncClient

from a2c_smcp.smcp import (
    ENTER_OFFICE_NOTIFICATION,
    GET_TOOLS_EVENT,
    JOIN_OFFICE_EVENT,
    LEAVE_OFFICE_EVENT,
    LEAVE_OFFICE_NOTIFICATION,
    SMCP_NAMESPACE,
    TOOL_CALL_EVENT,
    UPDATE_CONFIG_EVENT,
    EnterOfficeReq,
    GetToolsReq,
    UpdateMCPConfigNotification,
)


async def _join_office(client: AsyncClient, role: Literal["computer", "agent"], office_id: str, name: str) -> None:
    payload: EnterOfficeReq = {"role": role, "office_id": office_id, "name": name}
    ok, err = await client.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)
    assert ok and err is None


@pytest.mark.asyncio
async def test_enter_and_broadcast(socketio_server, basic_server_port: int):
    """
    中文：Agent 先入场，Computer 后入场，服务端应广播 ENTER_OFFICE_NOTIFICATION 给同房间的 Agent。
    English: Agent first, Computer then; server should broadcast ENTER_OFFICE_NOTIFICATION to Agent in same room.
    """
    agent = AsyncClient()
    computer = AsyncClient()

    enter_events: list[dict] = []

    @agent.on(ENTER_OFFICE_NOTIFICATION, namespace=SMCP_NAMESPACE)
    async def _on_enter(data: dict):
        enter_events.append(data)

    # 连接并让 Agent 入场
    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-async-1"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-A")

    # 连接并让 Computer 入场
    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await _join_office(computer, role="computer", office_id=office_id, name="comp-A")

    # 等待广播
    await asyncio.sleep(0.2)

    assert enter_events, "Agent 应收到 ENTER_OFFICE_NOTIFICATION"

    await agent.disconnect()
    await computer.disconnect()


@pytest.mark.asyncio
async def test_leave_and_broadcast(socketio_server, basic_server_port: int):
    """
    中文：Computer 离开办公室，服务端应广播 LEAVE_OFFICE_NOTIFICATION 给房间内其他客户端。
    English: When Computer leaves, server should broadcast LEAVE_OFFICE_NOTIFICATION to others in the room.
    """
    agent = AsyncClient()
    computer = AsyncClient()

    leave_events: list[dict] = []

    @agent.on(LEAVE_OFFICE_NOTIFICATION, namespace=SMCP_NAMESPACE)
    async def _on_leave(data: dict):
        leave_events.append(data)

    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-async-2"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-B")

    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await _join_office(computer, role="computer", office_id=office_id, name="comp-B")

    # 通过 server:leave_office 离开
    ok, err = await computer.call(
        LEAVE_OFFICE_EVENT,
        {"office_id": office_id},
        namespace=SMCP_NAMESPACE,
    )
    assert ok and err is None

    await asyncio.sleep(0.2)
    assert leave_events, "Agent 应收到 LEAVE_OFFICE_NOTIFICATION"

    await agent.disconnect()
    await computer.disconnect()


@pytest.mark.asyncio
async def test_tool_call_roundtrip(socketio_server, basic_server_port: int):
    """
    中文：Agent 发起 client:tool_call，服务端转发至目标 Computer，并将其 ACK 作为结果返回。
    English: Agent calls client:tool_call; server forwards to Computer and returns ACK result.
    """
    agent = AsyncClient()
    computer = AsyncClient()

    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-async-3"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-C")

    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await _join_office(computer, role="computer", office_id=office_id, name="comp-C")

    @computer.on(TOOL_CALL_EVENT, namespace=SMCP_NAMESPACE)
    async def _on_tool_call(data: dict):
        return CallToolResult(
            isError=False,
            content=[TextContent(type="text", text="ok from computer")],
        ).model_dump(mode="json")

    # 使用 agent 直接调用服务端事件（测试服务端转发与聚合）
    res = await agent.call(
        TOOL_CALL_EVENT,
        {
            "agent": "robot-C",
            "computer": "comp-C",
            "tool_name": "echo",
            "params": {"text": "hi"},
            "req_id": "req-001",
            "timeout": 5,
        },
        namespace=SMCP_NAMESPACE,
    )

    assert isinstance(res, dict)
    assert res.get("isError") is False
    assert any(c.get("text") == "ok from computer" for c in res.get("content", []))

    await agent.disconnect()
    await computer.disconnect()


@pytest.mark.asyncio
async def test_get_tools_success_same_office(socketio_server, basic_server_port: int):
    """
    中文：Agent 与 Computer 同房间，调用 client:get_tools，服务端通过 call 获取并返回工具列表。
    English: Agent and Computer in same room; client:get_tools returns tools list via server call.
    """
    agent = AsyncClient()
    computer = AsyncClient()

    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-async-4"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-D")

    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await _join_office(computer, role="computer", office_id=office_id, name="comp-D")

    tools_ready = asyncio.Event()

    @computer.on(GET_TOOLS_EVENT, namespace=SMCP_NAMESPACE)
    async def _on_get_tools(data: GetToolsReq):
        tools_ready.set()
        return {
            "tools": [
                {
                    "name": "echo",
                    "description": "echo text",
                    "params_schema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    "return_schema": None,
                },
            ],
            "req_id": data["req_id"],
        }

    res = await agent.call(
        GET_TOOLS_EVENT,
        {
            "computer": "comp-D",
            "agent": "robot-D",
            "req_id": "req-002",
        },
        namespace=SMCP_NAMESPACE,
    )

    await asyncio.wait_for(tools_ready.wait(), timeout=3)

    assert isinstance(res, dict)
    assert res.get("tools") and res["tools"][0]["name"] == "echo"

    await agent.disconnect()
    await computer.disconnect()


@pytest.mark.asyncio
async def test_update_config_broadcast(socketio_server, basic_server_port: int):
    """
    中文：Computer 触发 server:update_config，服务端向同房间广播 UPDATE_CONFIG_NOTIFICATION。
    English: Computer emits server:update_config; server broadcasts UPDATE_CONFIG_NOTIFICATION.
    """
    agent = AsyncClient()
    computer = AsyncClient()

    update_events: list[UpdateMCPConfigNotification] = []

    @agent.on("notify:update_config", namespace=SMCP_NAMESPACE)
    async def _on_update(data: UpdateMCPConfigNotification) -> None:
        update_events.append(data)

    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-async-5"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-E")

    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await _join_office(computer, role="computer", office_id=office_id, name="comp-E")

    # 由 Computer 触发 server:update_config
    await computer.emit(
        UPDATE_CONFIG_EVENT,
        {"computer": computer.get_sid(SMCP_NAMESPACE)},
        namespace=SMCP_NAMESPACE,
    )

    await asyncio.sleep(0.2)
    assert update_events and update_events[0]["computer"] == computer.get_sid(SMCP_NAMESPACE)

    await agent.disconnect()
    await computer.disconnect()


@pytest.mark.asyncio
async def test_list_room_success(socketio_server, basic_server_port: int):
    """
    中文：测试 Agent 成功列出房间内所有会话信息
    English: Test Agent successfully lists all sessions in a room
    """
    from a2c_smcp.smcp import LIST_ROOM_EVENT, ListRoomReq

    agent = AsyncClient()
    computer1 = AsyncClient()
    computer2 = AsyncClient()

    # 连接所有客户端 / Connect all clients
    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await computer1.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    await computer2.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )

    # 让所有客户端加入同一房间 / All clients join the same room
    office_id = "office-list-room-1"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-list")
    await _join_office(computer1, role="computer", office_id=office_id, name="comp-list-1")
    await _join_office(computer2, role="computer", office_id=office_id, name="comp-list-2")

    # 等待所有客户端加入 / Wait for all clients to join
    await asyncio.sleep(0.2)

    # Agent 调用 list_room 事件 / Agent calls list_room event
    list_req: ListRoomReq = {
        "agent": agent.sid,
        "req_id": "list_req_1",
        "office_id": office_id,
    }
    result = await agent.call(LIST_ROOM_EVENT, list_req, namespace=SMCP_NAMESPACE)

    # 验证结果 / Verify result
    assert result is not None
    assert result["req_id"] == "list_req_1"
    assert "sessions" in result
    assert len(result["sessions"]) == 3  # 1 agent + 2 computers

    # 验证会话信息 / Verify session info
    sessions = result["sessions"]
    roles = [s["role"] for s in sessions]
    assert roles.count("agent") == 1
    assert roles.count("computer") == 2
    assert all(s["office_id"] == office_id for s in sessions)

    # 断开连接 / Disconnect
    await agent.disconnect()
    await computer1.disconnect()
    await computer2.disconnect()


@pytest.mark.asyncio
async def test_list_room_empty_office(socketio_server, basic_server_port: int):
    """
    中文：测试 Agent 查询只有自己的房间
    English: Test Agent queries a room with only itself
    """
    from a2c_smcp.smcp import LIST_ROOM_EVENT, ListRoomReq

    agent = AsyncClient()

    await agent.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )

    # Agent 加入 office_empty
    # Agent joins office_empty
    office_id = "office_empty"
    await _join_office(agent, role="agent", office_id=office_id, name="robot-alone")

    await asyncio.sleep(0.2)

    # Agent 查询自己所在的房间（只有自己）
    # Agent queries its own room (only itself)
    list_req: ListRoomReq = {
        "agent": agent.sid,
        "req_id": "list_req_empty",
        "office_id": office_id,
    }
    result = await agent.call(LIST_ROOM_EVENT, list_req, namespace=SMCP_NAMESPACE)

    # 验证结果：应该只有 1 个会话（Agent 自己）
    # Verify result: should only have 1 session (Agent itself)
    assert result is not None
    assert result["req_id"] == "list_req_empty"
    assert "sessions" in result
    assert len(result["sessions"]) == 1
    assert result["sessions"][0]["role"] == "agent"
    assert result["sessions"][0]["office_id"] == office_id

    await agent.disconnect()


@pytest.mark.asyncio
async def test_computer_duplicate_name_rejected(socketio_server, basic_server_port: int):
    """
    中文：测试Computer重名检查：当房间内已存在同名Computer时，第二个Computer加入应失败
    English: Test Computer duplicate name check: second Computer with same name should fail to join
    """
    computer1 = AsyncClient()
    computer2 = AsyncClient()

    # 连接第一个 Computer
    # Connect first Computer
    await computer1.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-dup-test"
    computer_name = "duplicate-comp"

    # 第一个 Computer 成功加入
    # First Computer joins successfully
    await _join_office(computer1, role="computer", office_id=office_id, name=computer_name)

    # 连接第二个 Computer（同名）
    # Connect second Computer (same name)
    await computer2.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )

    # 第二个 Computer 尝试加入同一房间，应该失败
    # Second Computer tries to join same room, should fail
    payload: EnterOfficeReq = {"role": "computer", "office_id": office_id, "name": computer_name}
    ok, err = await computer2.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)

    # 验证失败
    # Verify failure
    assert not ok, "第二个同名Computer应该加入失败 / Second Computer with same name should fail to join"
    assert err is not None, "应该返回错误信息 / Should return error message"
    assert "already exists" in err, f"错误信息应包含'already exists'，实际: {err} / Error should contain 'already exists'"

    await computer1.disconnect()
    await computer2.disconnect()


@pytest.mark.asyncio
async def test_computer_different_name_allowed(socketio_server, basic_server_port: int):
    """
    中文：测试不同名Computer可以加入：房间内已有Computer，但名字不同，应该成功
    English: Test different name Computer can join: room has Computer but different name, should succeed
    """
    computer1 = AsyncClient()
    computer2 = AsyncClient()

    # 连接第一个 Computer
    # Connect first Computer
    await computer1.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )
    office_id = "office-diff-name-test"

    # 第一个 Computer 加入
    # First Computer joins
    await _join_office(computer1, role="computer", office_id=office_id, name="comp-1")

    # 连接第二个 Computer（不同名）
    # Connect second Computer (different name)
    await computer2.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )

    # 第二个 Computer 加入同一房间，应该成功
    # Second Computer joins same room, should succeed
    payload: EnterOfficeReq = {"role": "computer", "office_id": office_id, "name": "comp-2"}
    ok, err = await computer2.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)

    # 验证成功
    # Verify success
    assert ok, f"不同名Computer应该加入成功 / Different name Computer should succeed, error: {err}"
    assert err is None, "不应该有错误信息 / Should not have error message"

    await computer1.disconnect()
    await computer2.disconnect()


@pytest.mark.asyncio
async def test_computer_switch_room_with_same_name_allowed(socketio_server, basic_server_port: int):
    """
    中文：测试Computer切换房间：同一个Computer从一个房间切换到另一个房间应该成功
    English: Test Computer switching rooms: same Computer switching from one room to another should succeed
    """
    computer = AsyncClient()

    # 连接 Computer
    # Connect Computer
    await computer.connect(
        f"http://localhost:{basic_server_port}",
        namespaces=[SMCP_NAMESPACE],
        socketio_path="/socket.io",
    )

    computer_name = "switching-comp"

    # 加入第一个房间
    # Join first room
    await _join_office(computer, role="computer", office_id="office-room-1", name=computer_name)

    # 切换到第二个房间（同名Computer）
    # Switch to second room (same name Computer)
    payload: EnterOfficeReq = {"role": "computer", "office_id": "office-room-2", "name": computer_name}
    ok, err = await computer.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)

    # 验证成功
    # Verify success
    assert ok, f"Computer切换房间应该成功 / Computer switching rooms should succeed, error: {err}"
    assert err is None, "不应该有错误信息 / Should not have error message"

    await computer.disconnect()
