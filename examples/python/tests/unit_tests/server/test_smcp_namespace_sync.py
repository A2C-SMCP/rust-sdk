"""
* 文件名: test_smcp_namespace_sync
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: pytest, socketio
* 描述: 同步版 SMCP Namespace 测试用例 / Sync SMCP Namespace test cases
"""

from unittest.mock import MagicMock

import pytest

from a2c_smcp.server import (
    DefaultSyncAuthenticationProvider,
    SyncAuthenticationProvider,
    SyncSMCPNamespace,
)
from a2c_smcp.smcp import SMCP_NAMESPACE, EnterOfficeReq, LeaveOfficeReq


class MockSyncAuthProvider(SyncAuthenticationProvider):
    """Mock同步认证提供者 / Mock sync authentication provider"""

    def authenticate(self, sio, environ: dict, auth: dict | None, headers: list) -> bool:  # noqa: D401
        for header in headers:
            if isinstance(header, (list, tuple)) and len(header) >= 2:
                header_name = header[0].decode("utf-8").lower() if isinstance(header[0], bytes) else str(header[0]).lower()
                header_value = header[1].decode("utf-8") if isinstance(header[1], bytes) else str(header[1])
                if header_name == "x-api-key" and header_value == "valid_key":
                    return True
        return False


@pytest.fixture
def mock_auth_provider():
    return MockSyncAuthProvider()


@pytest.fixture
def smcp_namespace(mock_auth_provider):
    return SyncSMCPNamespace(mock_auth_provider)


@pytest.fixture
def mock_server():
    server = MagicMock()
    server.app = MagicMock()
    server.app.state = MagicMock()
    server.app.state.agent_id = "test_agent"
    server.manager = MagicMock()
    # get_participants 返回空列表
    server.manager.get_participants.return_value = []
    return server


class TestSyncSMCPNamespace:
    def test_namespace_initialization(self, smcp_namespace):
        assert smcp_namespace.namespace == SMCP_NAMESPACE
        assert isinstance(smcp_namespace.auth_provider, MockSyncAuthProvider)

    def test_successful_connection(self, smcp_namespace, mock_server):
        smcp_namespace.server = mock_server

        environ = {
            "asgi": {
                "scope": {
                    "headers": [
                        (b"x-api-key", b"valid_key"),
                    ],
                },
            },
        }

        result = smcp_namespace.on_connect("test_sid", environ, None)
        assert result is True

    def test_failed_authentication(self, smcp_namespace, mock_server):
        smcp_namespace.server = mock_server

        environ = {
            "asgi": {
                "scope": {
                    "headers": [
                        (b"x-api-key", b"invalid_key"),
                    ],
                },
            },
        }

        with pytest.raises(ConnectionRefusedError):
            smcp_namespace.on_connect("test_sid", environ, None)

    def test_join_office_success(self, smcp_namespace):
        # mock 会话相关方法
        session = {}
        smcp_namespace.get_session = MagicMock(return_value=session)
        smcp_namespace.save_session = MagicMock()
        smcp_namespace.enter_room = MagicMock()

        data = EnterOfficeReq(**{
            "role": "computer",
            "name": "test_computer",
            "office_id": "office_123",
        })

        success, error = smcp_namespace.on_server_join_office("test_sid", data)

        assert success is True
        assert error is None
        assert session["role"] == "computer"
        assert session["name"] == "test_computer"

    def test_join_office_role_mismatch(self, smcp_namespace):
        session = {"role": "agent"}
        smcp_namespace.get_session = MagicMock(return_value=session)
        smcp_namespace.save_session = MagicMock()

        data = EnterOfficeReq(**{
            "role": "computer",
            "name": "test_computer",
            "office_id": "office_123",
        })

        success, error = smcp_namespace.on_server_join_office("test_sid", data)

        assert success is False
        assert "Role mismatch" in error

    def test_leave_office(self, smcp_namespace):
        smcp_namespace.leave_room = MagicMock()

        data = LeaveOfficeReq(**{"office_id": "office_123"})

        success, error = smcp_namespace.on_server_leave_office("test_sid", data)

        assert success is True
        assert error is None
        smcp_namespace.leave_room.assert_called_once_with("test_sid", "office_123")
