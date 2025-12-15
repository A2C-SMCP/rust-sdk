"""
* 文件名: test_smcp_namespace
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: pytest, socketio
* 描述: SMCP Namespace测试用例 / SMCP Namespace test cases
"""

from unittest.mock import AsyncMock, MagicMock

import pytest

from a2c_smcp.server import (
    AuthenticationProvider,
    DefaultAuthenticationProvider,
    SMCPNamespace,
)
from a2c_smcp.smcp import (
    GET_DESKTOP_EVENT,
    SMCP_NAMESPACE,
    UPDATE_DESKTOP_EVENT,
    UPDATE_DESKTOP_NOTIFICATION,
    EnterOfficeReq,
    GetDeskTopReq,
    GetDeskTopRet,
    LeaveOfficeReq,
)


class MockAuthProvider(AuthenticationProvider):
    """Mock认证提供者用于测试 / Mock authentication provider for testing"""

    async def authenticate(self, sio: AsyncMock, environ: dict, auth: dict | None, headers: list) -> bool:
        """简单的Mock认证逻辑 / Simple mock authentication logic"""
        # 从headers中提取API密钥进行认证
        # Extract API key from headers for authentication
        for header in headers:
            if isinstance(header, (list, tuple)) and len(header) >= 2:
                header_name = header[0].decode("utf-8").lower() if isinstance(header[0], bytes) else str(header[0]).lower()
                header_value = header[1].decode("utf-8") if isinstance(header[1], bytes) else str(header[1])

                if header_name == "x-api-key" and header_value == "valid_key":
                    return True
        return False

    async def has_admin_permission(self, sio: AsyncMock, agent_id: str, secret: str) -> bool:
        """Mock管理员权限检查 / Mock admin permission check"""
        return secret == "admin_secret"


@pytest.fixture
def mock_auth_provider():
    """创建Mock认证提供者 / Create mock authentication provider"""
    return MockAuthProvider()


@pytest.fixture
def smcp_namespace(mock_auth_provider):
    """创建SMCP命名空间实例 / Create SMCP namespace instance"""
    return SMCPNamespace(mock_auth_provider)


@pytest.fixture
def mock_server():
    """创建Mock服务器 / Create mock server"""
    server = AsyncMock()
    server.app = MagicMock()
    server.app.state = MagicMock()
    server.app.state.agent_id = "test_agent"
    server.manager = MagicMock()
    return server


class TestSMCPNamespace:
    """SMCP命名空间测试类 / SMCP namespace test class"""

    @pytest.mark.asyncio
    async def test_namespace_initialization(self, smcp_namespace):
        """测试命名空间初始化 / Test namespace initialization"""
        assert smcp_namespace.namespace == SMCP_NAMESPACE
        assert isinstance(smcp_namespace.auth_provider, MockAuthProvider)

    @pytest.mark.asyncio
    async def test_successful_connection(self, smcp_namespace, mock_server):
        """测试成功连接 / Test successful connection"""
        smcp_namespace.server = mock_server

        # Mock环境变量和认证数据
        # Mock environment variables and auth data
        environ = {
            "asgi": {
                "scope": {
                    "headers": [
                        (b"x-api-key", b"valid_key"),
                    ],
                },
            },
        }

        # 测试连接
        # Test connection
        result = await smcp_namespace.on_connect("test_sid", environ, None)
        assert result is True

    @pytest.mark.asyncio
    async def test_failed_authentication(self, smcp_namespace, mock_server):
        """测试认证失败 / Test authentication failure"""
        smcp_namespace.server = mock_server

        # Mock环境变量和无效认证数据
        # Mock environment variables and invalid auth data
        environ = {
            "asgi": {
                "scope": {
                    "headers": [
                        (b"x-api-key", b"invalid_key"),
                    ],
                },
            },
        }

        # 测试连接应该失败
        # Test connection should fail
        with pytest.raises(ConnectionRefusedError):
            await smcp_namespace.on_connect("test_sid", environ, None)

    @pytest.mark.asyncio
    async def test_join_office_success(self, smcp_namespace):
        """测试成功加入房间 / Test successful office join"""
        # Mock会话数据
        # Mock session data
        session = {}
        smcp_namespace.get_session = AsyncMock(return_value=session)
        smcp_namespace.save_session = AsyncMock()
        smcp_namespace.enter_room = AsyncMock()

        # 测试数据
        # Test data
        data = EnterOfficeReq(**{
            "role": "computer",
            "name": "test_computer",
            "office_id": "office_123",
        })

        # 执行测试
        # Execute test
        success, error = await smcp_namespace.on_server_join_office("test_sid", data)

        assert success is True
        assert error is None
        assert session["role"] == "computer"
        assert session["name"] == "test_computer"

    @pytest.mark.asyncio
    async def test_join_office_role_mismatch(self, smcp_namespace):
        """测试角色不匹配的情况 / Test role mismatch scenario"""
        # Mock会话数据，已有不同角色
        # Mock session data with different existing role
        session = {"role": "agent"}
        smcp_namespace.get_session = AsyncMock(return_value=session)
        smcp_namespace.save_session = AsyncMock()

        # 测试数据
        # Test data
        data = EnterOfficeReq(**{
            "role": "computer",
            "name": "test_computer",
            "office_id": "office_123",
        })

        # 执行测试
        # Execute test
        success, error = await smcp_namespace.on_server_join_office("test_sid", data)

        assert success is False
        assert "Role mismatch" in error

    @pytest.mark.asyncio
    async def test_leave_office(self, smcp_namespace):
        """测试离开房间 / Test leaving office"""
        smcp_namespace.leave_room = AsyncMock()

        # 测试数据
        # Test data
        data = LeaveOfficeReq(**{"office_id": "office_123"})

        # 执行测试
        # Execute test
        success, error = await smcp_namespace.on_server_leave_office("test_sid", data)

        assert success is True
        assert error is None
        smcp_namespace.leave_room.assert_called_once_with("test_sid", "office_123")

    @pytest.mark.asyncio
    async def test_enter_room_agent_constraints_and_duplicate(self, smcp_namespace, mock_server):
        """覆盖 agent 进入房间的约束：
        - 已在其他房间 -> 抛错
        - 已在同一房间 -> 警告并返回（不重复加入）
        - 房间内已存在 agent -> 抛错
        """
        smcp_namespace.server = mock_server

        # 1) agent 已在其他房间
        session = {"role": "agent", "office_id": "roomA"}
        smcp_namespace.get_session = AsyncMock(return_value=session)
        smcp_namespace.save_session = AsyncMock()
        smcp_namespace.emit = AsyncMock()

        with pytest.raises(ValueError):
            await smcp_namespace.enter_room("sid1", "roomB")

        # 2) agent 未在任何房间，但房间内已有 agent
        session2 = {"role": "agent"}
        smcp_namespace.get_session = AsyncMock(return_value=session2)
        # 房间已有一个 agent 参与者
        mock_server.manager.get_participants.return_value = ["sidAgent"]

        smcp_namespace.get_session = AsyncMock(side_effect=[session2, {"role": "agent"}])
        with pytest.raises(ValueError):
            await smcp_namespace.enter_room("sid2", "roomA")

        # 3) agent 已在同一房间 -> 返回（不抛错）
        session3 = {"role": "agent", "office_id": "roomA"}
        smcp_namespace.get_session = AsyncMock(return_value=session3)
        # 调用不会抛出
        await smcp_namespace.enter_room("sid3", "roomA")

    @pytest.mark.asyncio
    async def test_enter_room_computer_switch_and_duplicate(self, smcp_namespace, mock_server):
        """覆盖 computer 切换房间与重复加入同一房间。"""
        smcp_namespace.server = mock_server
        # computer 从 roomA 切到 roomB，应先 leave_room(roomA)
        session = {"role": "computer", "office_id": "roomA"}
        smcp_namespace.get_session = AsyncMock(return_value=session)
        smcp_namespace.save_session = AsyncMock()
        smcp_namespace.emit = AsyncMock()
        smcp_namespace.leave_room = AsyncMock()
        await smcp_namespace.enter_room("csid", "roomB")
        smcp_namespace.leave_room.assert_called_once_with("csid", "roomA")

        # 重复加入同一房间 -> 直接返回
        session2 = {"role": "computer", "office_id": "roomB"}
        smcp_namespace.get_session = AsyncMock(return_value=session2)
        await smcp_namespace.enter_room("csid", "roomB")

    @pytest.mark.asyncio
    async def test_leave_room_broadcast_and_clear_session(self, smcp_namespace, monkeypatch):
        """覆盖 leave_room 的广播通知与 session 清理。"""
        smcp_namespace.emit = AsyncMock()
        sess = {"role": "computer", "office_id": "roomX"}
        smcp_namespace.get_session = AsyncMock(return_value=sess)
        smcp_namespace.save_session = AsyncMock()
        # 拦截 BaseNamespace.leave_room，避免真实父类逻辑
        from a2c_smcp.server.base import BaseNamespace

        monkeypatch.setattr(BaseNamespace, "leave_room", AsyncMock())
        await smcp_namespace.leave_room("sidX", "roomX")
        assert "office_id" not in sess

    @pytest.mark.asyncio
    async def test_on_client_get_tools_and_tool_call_and_updates(self, smcp_namespace, mock_server):
        """覆盖 get_tools 权限校验、tool_call 与 update/cancel 的广播/调用。"""
        smcp_namespace.server = mock_server

        # 准备会话：agent 与 computer 在同一房间
        agent_id = "a1"
        agent_sid = "a_sid1"
        comp_name = "c1"
        comp_sid = "c_sid1"
        sess_agent = {"role": "agent", "office_id": "room1", "name": agent_id}
        sess_comp = {"role": "computer", "office_id": "room1", "name": comp_name}

        smcp_namespace.get_session = AsyncMock(side_effect=lambda sid: (sess_comp if sid == comp_sid else sess_agent))
        smcp_namespace._name_to_sid_map = {comp_name: comp_sid, agent_id: agent_sid}
        smcp_namespace.call = AsyncMock(return_value={"tools": [], "req_id": "r1"})

        # get_tools 成功
        ret = await smcp_namespace.on_client_get_tools(agent_id, {"computer": comp_name})
        assert isinstance(ret, dict)
        assert "tools" in ret

        # tool_call：由 agent 发起，映射到 call
        smcp_namespace.get_session = AsyncMock(return_value=sess_agent)
        smcp_namespace.call = AsyncMock(return_value={"ok": True})
        res = await smcp_namespace.on_client_tool_call(
            agent_id,
            {
                "agent": agent_id,
                "req_id": "r2",
                "computer": comp_name,
                "tool_name": "t1",
                "params": {},
                "timeout": 5,
            },
        )
        assert res == {"ok": True}

        # update_config：由 computer 发起，广播通知
        smcp_namespace.get_session = AsyncMock(return_value=sess_comp)
        smcp_namespace.emit = AsyncMock()
        await smcp_namespace.on_server_update_config(comp_name, {"computer": comp_name})
        smcp_namespace.emit.assert_awaited()

        # cancel tool call：由 agent 发起，广播通知
        smcp_namespace.get_session = AsyncMock(return_value=sess_agent)
        smcp_namespace.emit = AsyncMock()
        await smcp_namespace.on_server_tool_call_cancel(agent_id, {"agent": agent_id, "req_id": "r3"})
        smcp_namespace.emit.assert_awaited()

    @pytest.mark.asyncio
    async def test_on_server_list_room_success(self, smcp_namespace, mock_server, monkeypatch):
        """
        测试成功列出房间内所有会话信息
        Test successfully listing all sessions in a room
        """
        smcp_namespace.server = mock_server

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
        smcp_namespace.get_session = AsyncMock(
            return_value={"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": office_id},
        )

        # 使用 monkeypatch Mock aget_all_sessions_in_office
        # Use monkeypatch to mock aget_all_sessions_in_office
        async def mock_aget_all_sessions(office_id_param, sio):
            return sessions_data

        monkeypatch.setattr("a2c_smcp.server.namespace.aget_all_sessions_in_office", mock_aget_all_sessions)

        # 执行测试 / Execute test
        result = await smcp_namespace.on_server_list_room(
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

    @pytest.mark.asyncio
    async def test_on_server_list_room_permission_denied(self, smcp_namespace, mock_server):
        """
        测试权限检查：Agent 只能查询自己所在房间
        Test permission check: Agent can only query their own room
        """
        from a2c_smcp.smcp import ListRoomReq

        smcp_namespace.server = mock_server

        # Agent 在 office_A，但请求查询 office_B
        # Agent is in office_A but requests to query office_B
        agent_sid = "agent_1"
        agent_session = {"sid": agent_sid, "name": "Test Agent", "role": "agent", "office_id": "office_A"}

        smcp_namespace.get_session = AsyncMock(return_value=agent_session)

        # 执行测试，应该抛出 AssertionError / Execute test, should raise AssertionError
        with pytest.raises(AssertionError, match="Agent只能查询自己所在房间的会话信息"):
            await smcp_namespace.on_server_list_room(
                agent_sid,
                {"agent": agent_sid, "req_id": "req_456", "office_id": "office_B"},
            )

    @pytest.mark.asyncio
    async def test_on_server_list_room_filters_invalid_sessions(self, smcp_namespace, mock_server, monkeypatch):
        """
        测试过滤无效会话：只返回有效的 computer 和 agent 角色
        Test filtering invalid sessions: only return valid computer and agent roles
        """
        smcp_namespace.server = mock_server

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
        smcp_namespace.get_session = AsyncMock(return_value=agent_session)

        # 使用 monkeypatch Mock aget_all_sessions_in_office
        async def mock_aget_all_sessions(office_id_param, sio):
            return sessions_data

        monkeypatch.setattr("a2c_smcp.server.namespace.aget_all_sessions_in_office", mock_aget_all_sessions)

        result = await smcp_namespace.on_server_list_room(
            agent_sid,
            {"agent": agent_sid, "req_id": "req_789", "office_id": office_id},
        )

        # 验证结果：应该只包含 3 个有效会话（排除 unknown 角色）
        # Verify result: should only contain 3 valid sessions (excluding unknown role)
        assert len(result["sessions"]) == 3
        assert all(s["role"] in ["computer", "agent"] for s in result["sessions"])


class TestDefaultAuthenticationProvider:
    """默认认证提供者测试类 / Default authentication provider test class"""

    def test_initialization(self):
        """测试初始化 / Test initialization"""
        provider = DefaultAuthenticationProvider("admin_secret", "custom-api-key")
        assert provider.admin_secret == "admin_secret"
        assert provider.api_key_name == "custom-api-key"

    @pytest.mark.asyncio
    async def test_admin_authentication(self):
        """测试管理员认证 / Test admin authentication"""
        provider = DefaultAuthenticationProvider("admin_secret")
        mock_sio = AsyncMock()

        headers = [(b"x-api-key", b"admin_secret")]
        result = await provider.authenticate(mock_sio, "agent_123", None, headers)
        assert result is True

    @pytest.mark.asyncio
    async def test_failed_authentication(self):
        """测试认证失败 / Test authentication failure"""
        provider = DefaultAuthenticationProvider("admin_secret")
        mock_sio = AsyncMock()

        headers = [(b"x-api-key", b"wrong_secret")]
        result = await provider.authenticate(mock_sio, "agent_123", None, headers)
        assert result is False

    @pytest.mark.asyncio
    async def test_no_api_key(self):
        """测试无API密钥的情况 / Test no API key scenario"""
        provider = DefaultAuthenticationProvider("admin_secret")
        mock_sio = AsyncMock()

        headers = []  # 空的headers列表
        result = await provider.authenticate(mock_sio, "agent_123", None, headers)
        assert result is False

    @pytest.mark.asyncio
    async def test_enter_room_computer_duplicate_name_raises_error(self, smcp_namespace, mock_server):
        """
        测试Computer重名检查：当房间内已存在同名Computer时，应抛出ValueError
        Test Computer duplicate name check: should raise ValueError when same name exists in room
        """
        smcp_namespace.server = mock_server

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
        mock_server.manager.get_participants.return_value = [(existing_computer_sid, "eio_sid")]

        # Mock get_session：第一次返回新Computer的session，第二次返回已存在Computer的session
        # Mock get_session: first call returns new Computer's session, second returns existing Computer's session
        smcp_namespace.get_session = AsyncMock(side_effect=[new_session, existing_session])
        smcp_namespace.save_session = AsyncMock()

        # 应该抛出 ValueError，提示重名
        # Should raise ValueError indicating duplicate name
        with pytest.raises(ValueError, match="Computer with name 'comp1' already exists in room 'room1'"):
            await smcp_namespace.enter_room(new_computer_sid, "room1")

    @pytest.mark.asyncio
    async def test_enter_room_computer_different_name_succeeds(self, smcp_namespace, mock_server, monkeypatch):
        """
        测试Computer不同名可以成功加入：房间内已有Computer，但名字不同，应该成功
        Test Computer with different name can join: room has Computer but different name, should succeed
        """
        from a2c_smcp.server.base import BaseNamespace

        smcp_namespace.server = mock_server

        # 房间内已有一个名为 "comp1" 的 Computer
        # Room already has a Computer named "comp1"
        existing_computer_sid = "existing_sid"
        existing_session = {"role": "computer", "name": "comp1", "office_id": "room1", "sid": existing_computer_sid}

        # 新的 Computer 叫 "comp2"，名字不同
        # New Computer named "comp2", different name
        new_computer_sid = "new_sid"
        new_session = {"role": "computer", "name": "comp2", "sid": new_computer_sid}

        # Mock get_participants 返回房间内已有的参与者
        mock_server.manager.get_participants.return_value = [(existing_computer_sid, "eio_sid")]

        # Mock get_session
        smcp_namespace.get_session = AsyncMock(side_effect=[new_session, existing_session, new_session])
        smcp_namespace.save_session = AsyncMock()
        smcp_namespace.emit = AsyncMock()
        smcp_namespace._register_name = AsyncMock()

        # Mock 父类的 enter_room 方法
        # Mock parent class enter_room method
        monkeypatch.setattr(BaseNamespace, "enter_room", AsyncMock())

        # 应该成功加入，不抛出异常
        # Should succeed without raising exception
        await smcp_namespace.enter_room(new_computer_sid, "room1")

        # 验证 save_session 被调用
        # Verify save_session was called
        assert smcp_namespace.save_session.called

    @pytest.mark.asyncio
    async def test_enter_room_computer_same_sid_allowed(self, smcp_namespace, mock_server, monkeypatch):
        """
        测试同一个Computer重新加入（幂等操作）：同一个sid重复加入应该被允许
        Test same Computer re-joining (idempotent): same sid rejoining should be allowed
        """
        from a2c_smcp.server.base import BaseNamespace

        smcp_namespace.server = mock_server

        # Computer 尝试重新加入同一房间
        # Computer tries to rejoin the same room
        computer_sid = "comp_sid"
        session = {"role": "computer", "name": "comp1", "sid": computer_sid}

        # Mock get_participants 返回自己
        # Mock get_participants returns itself
        mock_server.manager.get_participants.return_value = [(computer_sid, "eio_sid")]

        smcp_namespace.get_session = AsyncMock(side_effect=[session, session])
        smcp_namespace.save_session = AsyncMock()
        smcp_namespace.emit = AsyncMock()
        smcp_namespace._register_name = AsyncMock()

        # Mock 父类的 enter_room 方法
        monkeypatch.setattr(BaseNamespace, "enter_room", AsyncMock())

        # 应该成功，不抛出异常（跳过自己的检查）
        # Should succeed without exception (skip self-check)
        await smcp_namespace.enter_room(computer_sid, "room1")

        assert smcp_namespace.save_session.called
