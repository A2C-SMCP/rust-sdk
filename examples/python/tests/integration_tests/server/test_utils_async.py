# -*- coding: utf-8 -*-
# filename: test_utils_async.py
# @Time    : 2025/09/30 23:35
# @Author  : A2C-SMCP
"""
中文：针对 `a2c_smcp/server/utils.py` 的异步工具函数集成测试。
English: Integration tests for async utilities in `a2c_smcp/server/utils.py`.

覆盖点：
- aget_computers_in_office
- aget_all_sessions_in_office
"""

import asyncio

import pytest
from socketio import AsyncClient

from a2c_smcp.server import aget_all_sessions_in_office, aget_computers_in_office
from a2c_smcp.smcp import JOIN_OFFICE_EVENT, SMCP_NAMESPACE


async def _join_office(client: AsyncClient, role: str, office_id: str, name: str) -> None:
    ok, err = await client.call(
        JOIN_OFFICE_EVENT,
        {"role": role, "office_id": office_id, "name": name},
        namespace=SMCP_NAMESPACE,
    )
    assert ok and err is None


@pytest.mark.asyncio
async def test_aget_computers_and_sessions(socketio_server, basic_server_port: int):
    """
    场景：Agent 与 2 个 Computer 加入同一房间，验证工具函数返回。
    """
    # socketio_server fixture 返回的是命名空间，可从中取 server（AsyncServer）
    ns = socketio_server
    sio = ns.server

    agent = AsyncClient()
    comp1 = AsyncClient()
    comp2 = AsyncClient()

    office_id = "office-utils-1"

    # 连接并入场
    await agent.connect(f"http://localhost:{basic_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    await _join_office(agent, role="agent", office_id=office_id, name="robot-U1")

    await comp1.connect(f"http://localhost:{basic_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    await _join_office(comp1, role="computer", office_id=office_id, name="comp-U1")

    await comp2.connect(f"http://localhost:{basic_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    await _join_office(comp2, role="computer", office_id=office_id, name="comp-U2")

    # 等待会话写入完成
    await asyncio.sleep(0.2)

    computers = await aget_computers_in_office(office_id, sio)
    sessions = await aget_all_sessions_in_office(office_id, sio)

    assert len(computers) == 2
    assert all(c["role"] == "computer" for c in computers)
    # 会话应包含3个（1 Agent + 2 Computer）
    assert len(sessions) == 3

    # 清理
    await agent.disconnect()
    await comp1.disconnect()
    await comp2.disconnect()
