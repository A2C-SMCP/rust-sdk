# -*- coding: utf-8 -*-
# filename: test_server_e2e.py
# @Time    : 2025/10/05 14:12
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: Server 模块的端到端测试：基于真实 HTTP 服务与真实 socketio.Client，验证核心事件流。
English: End-to-end tests for the Server module: real HTTP service and real socketio.Client validating core event flows.
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from a2c_smcp.smcp import (
    CANCEL_TOOL_CALL_EVENT,
    CANCEL_TOOL_CALL_NOTIFICATION,
    ENTER_OFFICE_NOTIFICATION,
    GET_DESKTOP_EVENT,
    GET_TOOLS_EVENT,
    JOIN_OFFICE_EVENT,
    LEAVE_OFFICE_EVENT,
    LEAVE_OFFICE_NOTIFICATION,
    SMCP_NAMESPACE,
    TOOL_CALL_EVENT,
    UPDATE_CONFIG_EVENT,
    UPDATE_CONFIG_NOTIFICATION,
    UPDATE_DESKTOP_EVENT,
    UPDATE_DESKTOP_NOTIFICATION,
    UPDATE_TOOL_LIST_EVENT,
    UPDATE_TOOL_LIST_NOTIFICATION,
)

pytestmark = pytest.mark.e2e


def _wait_until(cond, timeout: float = 2.0, step: float = 0.01) -> bool:
    """
    中文: 简易等待辅助函数，直到条件满足或超时。
    English: Simple wait helper until condition met or timeout.
    """
    end = time.time() + timeout
    while time.time() < end:
        if cond():
            return True
        time.sleep(step)
    return cond()


def _join_office(client, role: str, name: str, office_id: str) -> None:
    """中文: 加入房间 / English: Join office"""
    ok, err = client.call(
        JOIN_OFFICE_EVENT,
        {"role": role, "name": name, "office_id": office_id},
        namespace=SMCP_NAMESPACE,
        timeout=5,
    )
    if not (ok and err is None):
        raise RuntimeError(f"加入房间失败 / Failed to join office: ok={ok}, err={err}")


def _leave_office(client, office_id: str) -> None:
    """中文: 离开房间 / English: Leave office"""
    ok, err = client.call(
        LEAVE_OFFICE_EVENT,
        {"office_id": office_id},
        namespace=SMCP_NAMESPACE,
        timeout=5,
    )
    if not (ok and err is None):
        raise RuntimeError(f"离开房间失败 / Failed to leave office: ok={ok}, err={err}")


def test_server_end_to_end_flow(agent_client, computer_client):
    """
    中文:
      - 验证加入房间后的通知广播
      - 验证 Agent 获取 Computer 工具与桌面
      - 验证 Computer 更新通知广播
      - 验证 Agent 发起工具调用以及取消调用的通知
      - 验证离开房间的通知广播
    English:
      - Verify notifications on join
      - Verify Agent fetches Computer tools and desktop
      - Verify Computer update notifications
      - Verify Agent tool call and cancel notifications
      - Verify notifications on leave
    """

    room = "office-1"
    comp_name = "cmp-1"
    agent_name = "age-1"

    # 准备通知捕获容器 / Prepare notification captures
    agent_events: dict[str, list[Any]] = {
        ENTER_OFFICE_NOTIFICATION: [],
        UPDATE_CONFIG_NOTIFICATION: [],
        UPDATE_TOOL_LIST_NOTIFICATION: [],
        UPDATE_DESKTOP_NOTIFICATION: [],
        CANCEL_TOOL_CALL_NOTIFICATION: [],
        LEAVE_OFFICE_NOTIFICATION: [],
    }

    computer_events: dict[str, list[Any]] = {
        ENTER_OFFICE_NOTIFICATION: [],
        UPDATE_CONFIG_NOTIFICATION: [],
        UPDATE_TOOL_LIST_NOTIFICATION: [],
        UPDATE_DESKTOP_NOTIFICATION: [],
        CANCEL_TOOL_CALL_NOTIFICATION: [],
        LEAVE_OFFICE_NOTIFICATION: [],
    }

    # 在 Computer 客户端上实现请求处理：get_tools / get_desktop / tool_call
    # Implement request handlers on computer side (MUST register BEFORE joining)
    def _on_get_tools(data):
        # 返回最简工具列表 / minimal tool list
        return {
            "tools": [
                {
                    "name": "echo",
                    "description": "echo input",
                    "params_schema": {"type": "object"},
                    "return_schema": {"type": "object"},
                },
            ],
            "req_id": data.get("req_id", "r1"),
        }

    def _on_get_desktop(data):
        return {"desktops": ["win://1"], "req_id": data.get("req_id", "r2")}

    def _on_tool_call(data):
        # 回显实现 / echo implementation
        return {"ok": True, "echo": data}

    computer_client.on(GET_TOOLS_EVENT, _on_get_tools, namespace=SMCP_NAMESPACE)
    computer_client.on(GET_DESKTOP_EVENT, _on_get_desktop, namespace=SMCP_NAMESPACE)
    computer_client.on(TOOL_CALL_EVENT, _on_tool_call, namespace=SMCP_NAMESPACE)

    # 注册通知监听 / subscribe notifications
    for evt, store in agent_events.items():

        def make_handler(s, e=evt):
            def handler(data):
                s.append(data)

            return handler

        agent_client.on(evt, make_handler(store), namespace=SMCP_NAMESPACE)

    for evt, store in computer_events.items():

        def make_handler(s, e=evt):
            def handler(data):
                s.append(data)

            return handler

        computer_client.on(evt, make_handler(store), namespace=SMCP_NAMESPACE)

    # 等待确保事件处理器注册完成 / Wait to ensure event handlers are registered
    time.sleep(0.1)

    # 计算机先加入，Agent 后加入，Computer 应收到 Agent 加入的通知 / computer joins first, then agent joins
    _join_office(computer_client, role="computer", name=comp_name, office_id=room)

    # 等待一下以确保 session 建立 / small wait to ensure session saved
    time.sleep(0.1)

    _join_office(agent_client, role="agent", name=agent_name, office_id=room)

    # Computer 应该收到 Agent 加入的通知 / Computer should receive agent join notification
    assert _wait_until(lambda: len(computer_events[ENTER_OFFICE_NOTIFICATION]) >= 1)
    assert computer_events[ENTER_OFFICE_NOTIFICATION][0]["office_id"] == room
    assert "agent" in computer_events[ENTER_OFFICE_NOTIFICATION][0]

    # Agent 拉取工具列表 / Agent get tools
    agent_ret_tools = agent_client.call(
        GET_TOOLS_EVENT,
        {"computer": comp_name, "robot_id": agent_name, "req_id": "req-tools"},
        namespace=SMCP_NAMESPACE,
        timeout=2,
    )
    assert isinstance(agent_ret_tools, dict)
    assert agent_ret_tools.get("tools") and agent_ret_tools.get("req_id") == "req-tools"

    # Agent 拉取桌面 / Agent get desktop
    agent_ret_desktop = agent_client.call(
        GET_DESKTOP_EVENT,
        {"computer": comp_name, "robot_id": agent_name, "req_id": "req-desk"},
        namespace=SMCP_NAMESPACE,
        timeout=2,
    )
    assert agent_ret_desktop.get("desktops") == ["win://1"]
    assert agent_ret_desktop.get("req_id") == "req-desk"

    # Computer 广播配置更新、工具列表更新、桌面更新 / updates broadcasting
    computer_client.emit(UPDATE_CONFIG_EVENT, {"computer": comp_name}, namespace=SMCP_NAMESPACE)
    computer_client.emit(UPDATE_TOOL_LIST_EVENT, {"computer": comp_name}, namespace=SMCP_NAMESPACE)
    computer_client.emit(UPDATE_DESKTOP_EVENT, {"computer": comp_name}, namespace=SMCP_NAMESPACE)

    assert _wait_until(lambda: len(agent_events[UPDATE_CONFIG_NOTIFICATION]) >= 1)
    assert _wait_until(lambda: len(agent_events[UPDATE_TOOL_LIST_NOTIFICATION]) >= 1)
    assert _wait_until(lambda: len(agent_events[UPDATE_DESKTOP_NOTIFICATION]) >= 1)

    # Agent 发起工具调用 / Agent tool call
    tool_call_ret = agent_client.call(
        TOOL_CALL_EVENT,
        {
            "computer": comp_name,
            "tool_name": "echo",
            "params": {"x": 1},
            "timeout": 2,
            "agent": agent_name,
            "req_id": "req-tc",
        },
        namespace=SMCP_NAMESPACE,
        timeout=3,
    )
    # 验证返回结果 / Verify return result
    assert tool_call_ret.get("ok") is True
    assert tool_call_ret.get("echo", {}).get("tool_name") == "echo"

    # Agent 取消工具调用（广播通知给 Computer） / cancel tool call
    agent_client.emit(
        CANCEL_TOOL_CALL_EVENT,
        {"agent": agent_name, "req_id": "req-tc"},
        namespace=SMCP_NAMESPACE,
    )
    assert _wait_until(lambda: len(computer_events[CANCEL_TOOL_CALL_NOTIFICATION]) >= 1)

    # 离开办公室 / leave
    # Computer 先离开，Agent 应该收到通知 / Computer leaves first, Agent should receive notification
    _leave_office(computer_client, office_id=room)
    assert _wait_until(lambda: len(agent_events[LEAVE_OFFICE_NOTIFICATION]) >= 1)
    assert agent_events[LEAVE_OFFICE_NOTIFICATION][0]["office_id"] == room
    assert "computer" in agent_events[LEAVE_OFFICE_NOTIFICATION][0]

    # Agent 后离开，此时房间里已经没有其他人了 / Agent leaves last, no one else in the room
    _leave_office(agent_client, office_id=room)
