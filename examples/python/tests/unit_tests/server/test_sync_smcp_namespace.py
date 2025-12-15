# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/sync_namespace.py
覆盖 enter_room/leave_room 及所有 on_* 分支
"""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from a2c_smcp.server.sync_namespace import SyncSMCPNamespace
from a2c_smcp.smcp import (
    CANCEL_TOOL_CALL_NOTIFICATION,
    ENTER_OFFICE_NOTIFICATION,
    LEAVE_OFFICE_NOTIFICATION,
)


class _DummyAuthProv:
    def authenticate(self, sio, environ, auth, headers):  # pragma: no cover - not used here
        return True


@pytest.fixture()
def ns():
    n = SyncSMCPNamespace(_DummyAuthProv())
    # 伪造 server 结构
    server = MagicMock()
    server.manager = MagicMock()
    server.manager.get_participants = MagicMock(return_value=[])
    n.server = server
    # 常用桩
    n.get_session = MagicMock(return_value={})
    n.save_session = MagicMock()
    n.emit = MagicMock()
    return n


def test_enter_room_agent_rules(ns):
    # agent 在其他房间 -> 抛错
    ns.get_session.return_value = {"role": "agent", "office_id": "A"}
    with pytest.raises(ValueError):
        ns.enter_room("sidA", "B")

    # agent 不在任何房间，房间已有 agent -> 抛错
    ns.get_session.return_value = {"role": "agent"}
    ns.server.manager.get_participants.return_value = ["sidX"]
    # 该参与者为 agent

    def _get_sess_for_participant(sid):
        if sid == "sidX":
            return {"role": "agent"}
        return {}

    ns.get_session.side_effect = [
        {"role": "agent"},  # self session
        {"role": "agent"},  # participant session
    ]
    with pytest.raises(ValueError):
        ns.enter_room("sidB", "room1")

    # agent 已在同一房间 -> 返回且不重复 emit
    ns.get_session.side_effect = None
    ns.get_session.return_value = {"role": "agent", "office_id": "room1"}
    ns.emit.reset_mock()
    ns.enter_room("sidC", "room1")
    ns.emit.assert_not_called()


def test_enter_room_computer_switch_and_duplicate(ns):
    # computer 从 roomA 切到 roomB
    ns.get_session.return_value = {"role": "computer", "office_id": "roomA"}
    ns.leave_room = MagicMock()
    ns.enter_room("csid", "roomB")
    ns.leave_room.assert_called_once_with("csid", "roomA")

    # 重复加入
    ns.get_session.return_value = {"role": "computer", "office_id": "roomB"}
    ns.emit.reset_mock()
    ns.enter_room("csid", "roomB")
    ns.emit.assert_not_called()


def test_enter_room_updates_session_and_broadcast(ns):
    # 新加入应设置 office_id 并广播 ENTER_OFFICE_NOTIFICATION
    ns = SyncSMCPNamespace(_DummyAuthProv())
    server = MagicMock()
    server.manager = MagicMock()
    server.manager.get_participants = MagicMock(return_value=[])
    ns.server = server

    sess = {"role": "computer"}
    ns.get_session = MagicMock(return_value=sess)
    ns.save_session = MagicMock()
    ns.emit = MagicMock()

    ns.enter_room("sid1", "roomZ")

    assert sess["office_id"] == "roomZ"
    ns.emit.assert_called_once()
    args, kwargs = ns.emit.call_args
    assert args[0] == ENTER_OFFICE_NOTIFICATION
    assert kwargs.get("room") == "roomZ"
    assert kwargs.get("skip_sid") == "sid1"


def test_leave_room_broadcast_and_clear_session(monkeypatch):
    ns = SyncSMCPNamespace(_DummyAuthProv())
    ns.emit = MagicMock()
    sess = {"role": "computer", "office_id": "roomX"}
    ns.get_session = MagicMock(return_value=sess)
    ns.save_session = MagicMock()

    # 避免真正调用父类 leave_room
    from a2c_smcp.server.sync_base import SyncBaseNamespace

    monkeypatch.setattr(SyncBaseNamespace, "leave_room", MagicMock())
    ns.leave_room("sidX", "roomX")

    ns.emit.assert_called_once()
    args, kwargs = ns.emit.call_args
    assert args[0] == LEAVE_OFFICE_NOTIFICATION
    assert "office_id" not in sess  # 已清理


def test_on_server_join_office_ok_and_rollback_on_error():
    ns = SyncSMCPNamespace(_DummyAuthProv())
    ns.get_session = MagicMock(return_value={})
    ns.save_session = MagicMock()
    # ensure server exists for enter_room path
    server = MagicMock()
    server.manager = MagicMock()
    server.manager.get_participants = MagicMock(return_value=[])
    ns.server = server

    # 正常路径
    ok, err = ns.on_server_join_office("sid", {"role": "computer", "name": "n", "office_id": "o"})
    assert ok is True and err is None

    # enter_room 抛错 -> 回滚
    ns.enter_room = MagicMock(side_effect=RuntimeError("boom"))
    ok2, err2 = ns.on_server_join_office("sid", {"role": "computer", "name": "n", "office_id": "o"})
    assert ok2 is False and "Internal server error" in err2


def test_on_server_leave_office_ok_and_error():
    ns = SyncSMCPNamespace(_DummyAuthProv())
    ns.leave_room = MagicMock()
    ok, err = ns.on_server_leave_office("sid", {"office_id": "o"})
    assert ok is True and err is None

    ns.leave_room = MagicMock(side_effect=RuntimeError("x"))
    ok2, err2 = ns.on_server_leave_office("sid", {"office_id": "o"})
    assert ok2 is False and "Internal server error" in err2


def test_on_server_tool_call_cancel_and_update_config_and_client_paths():
    ns = SyncSMCPNamespace(_DummyAuthProv())

    # cancel 仅允许 agent
    ns.get_session = MagicMock(return_value={"role": "agent", "name": "a1"})
    ns.emit = MagicMock()
    ns.on_server_tool_call_cancel("a1", {"agent": "a1", "req_id": "r1"})
    ns.emit.assert_called_once()
    args1, kwargs1 = ns.emit.call_args
    assert args1[0] == CANCEL_TOOL_CALL_NOTIFICATION
    assert kwargs1.get("skip_sid") == "a1"

    # update_config 仅允许 computer
    ns.get_session = MagicMock(return_value={"role": "computer", "office_id": "roomR"})
    ns.emit = MagicMock()
    ns.on_server_update_config("c1", {"computer": "c1"})
    ns.emit.assert_called_once()
    _args2, kwargs2 = ns.emit.call_args
    assert kwargs2.get("room") == "roomR"

    # client tool_call：仅允许 agent，使用 call 等待响应
    ns.get_session = MagicMock(return_value={"role": "agent"})
    ns.call = MagicMock(return_value={"ok": True, "result": "success"})
    ns.get_sid_by_name = MagicMock(return_value="c1")
    ret = ns.on_client_tool_call("a1", {"robot_id": "a1", "computer": "c1", "tool_name": "t", "params": {}, "timeout": 5})
    assert ret == {"ok": True, "result": "success"}
    ns.call.assert_called_once()
    args, kwargs = ns.call.call_args
    assert kwargs.get("to") == "c1"

    # client get_tools：校验在同一房间并转发
    ns.get_session = MagicMock(
        side_effect=[
            {"role": "computer", "office_id": "room1"},  # computer sess
            {"role": "agent", "office_id": "room1"},  # agent sess
        ],
    )
    ns.call = MagicMock(
        return_value={
            "req_id": "r3",
            "tools": [
                {
                    "name": "t1",
                    "description": "d",
                    "params_schema": {"type": "object", "properties": {}, "required": []},
                    "return_schema": None,
                },
            ],
        },
    )
    ret2 = ns.on_client_get_tools("a1", {"computer": "c1", "req_id": "r3", "agent": "a1"})
    assert isinstance(ret2, dict) and ret2["req_id"] == "r3" and isinstance(ret2.get("tools"), list)
    ns.call.assert_called_once()


def test_on_server_list_room_success(monkeypatch):
    """
    测试成功列出房间内所有会话信息（同步版本）
    Test successfully listing all sessions in a room (sync version)
    """
    ns = SyncSMCPNamespace(_DummyAuthProv())
    server = MagicMock()
    ns.server = server

    # 准备测试数据：Agent 和两个 Computer 在同一房间
    # Prepare test data: Agent and two Computers in the same room
    agent_sid = "agent_1"
    comp_sid_1 = "comp_1"
    comp_sid_2 = "comp_2"
    office_id = "test_office"

    # Mock 会话数据 / Mock session data
    sessions_data = [
        {"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": office_id},
        {"sid": comp_sid_1, "name": "Computer 1", "role": "computer", "office_id": office_id},
        {"sid": comp_sid_2, "name": "Computer 2", "role": "computer", "office_id": office_id},
    ]

    # Mock get_session 返回 Agent 会话 / Mock get_session to return Agent session
    ns.get_session = MagicMock(return_value={"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": office_id})

    # 使用 monkeypatch Mock get_all_sessions_in_office
    def mock_get_all_sessions(office_id_param, sio):
        return sessions_data

    monkeypatch.setattr("a2c_smcp.server.sync_namespace.get_all_sessions_in_office", mock_get_all_sessions)

    # 执行测试 / Execute test
    result = ns.on_server_list_room(
        agent_sid,
        {"agent": agent_sid, "req_id": "req_123", "office_id": office_id},
    )

    # 验证结果 / Verify result
    assert result["req_id"] == "req_123"
    assert len(result["sessions"]) == 3
    assert all(s["office_id"] == office_id for s in result["sessions"])
    assert any(s["sid"] == agent_sid for s in result["sessions"])
    assert any(s["sid"] == comp_sid_1 for s in result["sessions"])
    assert any(s["sid"] == comp_sid_2 for s in result["sessions"])


def test_on_server_list_room_permission_denied():
    """
    测试权限检查：Agent 只能查询自己所在房间（同步版本）
    Test permission check: Agent can only query their own room (sync version)
    """
    from a2c_smcp.smcp import ListRoomReq

    ns = SyncSMCPNamespace(_DummyAuthProv())
    server = MagicMock()
    ns.server = server

    # Agent 在 office_A，但请求查询 office_B
    # Agent is in office_A but requests to query office_B
    agent_sid = "agent_1"
    agent_session = {"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": "office_A"}

    ns.get_session = MagicMock(return_value=agent_session)

    # 执行测试，应该抛出 AssertionError / Execute test, should raise AssertionError
    import pytest

    with pytest.raises(AssertionError, match="Agent只能查询自己所在房间的会话信息"):
        ns.on_server_list_room(
            agent_sid,
            {"agent": agent_sid, "req_id": "req_456", "office_id": "office_B"},
        )


def test_on_server_list_room_filters_invalid_sessions(monkeypatch):
    """
    测试过滤无效会话：只返回有效的 computer 和 agent 角色（同步版本）
    Test filtering invalid sessions: only return valid computer and agent roles (sync version)
    """
    ns = SyncSMCPNamespace(_DummyAuthProv())
    server = MagicMock()
    ns.server = server

    agent_sid = "agent_1"
    office_id = "test_office"

    # Mock 会话数据，包含一些无效的会话 / Mock session data with some invalid sessions
    sessions_data = [
        {"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": office_id},
        {"sid": "comp_1", "name": "Computer 1", "role": "computer", "office_id": office_id},
        {"sid": "invalid_1", "name": "Invalid", "role": "unknown", "office_id": office_id},  # 无效角色
        {"sid": "comp_2", "name": "Computer 2", "role": "computer", "office_id": office_id},
    ]

    agent_session = {"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": office_id}
    ns.get_session = MagicMock(return_value=agent_session)

    # 使用 monkeypatch Mock get_all_sessions_in_office
    def mock_get_all_sessions(office_id_param, sio):
        return sessions_data

    monkeypatch.setattr("a2c_smcp.server.sync_namespace.get_all_sessions_in_office", mock_get_all_sessions)

    result = ns.on_server_list_room(
        agent_sid,
        {"agent": agent_sid, "req_id": "req_789", "office_id": office_id},
    )

    # 验证结果：应该只包含 3 个有效会话（排除 unknown 角色）
    # Verify result: should only contain 3 valid sessions (excluding unknown role)
    assert len(result["sessions"]) == 3
    assert all(s["role"] in ["computer", "agent"] for s in result["sessions"])


def test_enter_room_computer_duplicate_name_raises_error(ns):
    """
    测试Computer重名检查：当房间内已存在同名Computer时，应抛出ValueError
    Test Computer duplicate name check: should raise ValueError when same name exists in room
    """
    # 设置房间内已有一个名为 "comp1" 的 Computer
    # Setup: room already has a Computer named "comp1"
    existing_computer_sid = "existing_sid"
    existing_session = {"role": "computer", "name": "comp1", "office_id": "room1", "sid": existing_computer_sid}

    # 新的 Computer 也叫 "comp1"，尝试加入同一房间
    # New Computer also named "comp1" tries to join the same room
    new_computer_sid = "new_sid"
    new_session = {"role": "computer", "name": "comp1", "sid": new_computer_sid}

    # Mock get_participants 返回房间内已有的参与者
    # Mock get_participants to return existing participant
    ns.server.manager.get_participants.return_value = [(existing_computer_sid, "eio_sid")]

    # Mock get_session：第一次返回新Computer的session，第二次返回已存在Computer的session
    # Mock get_session: first call returns new Computer's session, second returns existing Computer's session
    ns.get_session.side_effect = [new_session, existing_session]

    # 应该抛出 ValueError，提示重名
    # Should raise ValueError indicating duplicate name
    with pytest.raises(ValueError, match="Computer with name 'comp1' already exists in room 'room1'"):
        ns.enter_room(new_computer_sid, "room1")


def test_enter_room_computer_different_name_succeeds(ns, monkeypatch):
    """
    测试Computer不同名可以成功加入：房间内已有Computer，但名字不同，应该成功
    Test Computer with different name can join: room has Computer but different name, should succeed
    """
    from a2c_smcp.server.sync_base import SyncBaseNamespace

    # 房间内已有一个名为 "comp1" 的 Computer
    # Room already has a Computer named "comp1"
    existing_computer_sid = "existing_sid"
    existing_session = {"role": "computer", "name": "comp1", "office_id": "room1", "sid": existing_computer_sid}

    # 新的 Computer 叫 "comp2"，名字不同
    # New Computer named "comp2", different name
    new_computer_sid = "new_sid"
    new_session = {"role": "computer", "name": "comp2", "sid": new_computer_sid}

    # Mock get_participants 返回房间内已有的参与者
    ns.server.manager.get_participants.return_value = [(existing_computer_sid, "eio_sid")]

    # Mock get_session
    ns.get_session.side_effect = [new_session, existing_session, new_session]
    ns._register_name = MagicMock()

    # Mock 父类的 enter_room 方法
    # Mock parent class enter_room method
    monkeypatch.setattr(SyncBaseNamespace, "enter_room", MagicMock())

    # 应该成功加入，不抛出异常
    # Should succeed without raising exception
    ns.enter_room(new_computer_sid, "room1")

    # 验证 save_session 被调用
    # Verify save_session was called
    assert ns.save_session.called


def test_enter_room_computer_same_sid_allowed(ns, monkeypatch):
    """
    测试同一个Computer重新加入（幂等操作）：同一个sid重复加入应该被允许
    Test same Computer re-joining (idempotent): same sid rejoining should be allowed
    """
    from a2c_smcp.server.sync_base import SyncBaseNamespace

    # Computer 尝试重新加入同一房间
    # Computer tries to rejoin the same room
    computer_sid = "comp_sid"
    session = {"role": "computer", "name": "comp1", "sid": computer_sid}

    # Mock get_participants 返回自己
    # Mock get_participants returns itself
    ns.server.manager.get_participants.return_value = [(computer_sid, "eio_sid")]

    ns.get_session.side_effect = [session, session]
    ns._register_name = MagicMock()

    # Mock 父类的 enter_room 方法
    monkeypatch.setattr(SyncBaseNamespace, "enter_room", MagicMock())

    # 应该成功，不抛出异常（跳过自己的检查）
    # Should succeed without exception (skip self-check)
    ns.enter_room(computer_sid, "room1")

    assert ns.save_session.called
