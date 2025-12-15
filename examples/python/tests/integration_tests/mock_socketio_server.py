"""
* 文件名: mock_socketio_server
* 作者: JQQ
* 创建日期: 2025/8/21
* 最后修改日期: 2025/9/30
* 版权: 2023 JQQ. All rights reserved.
* 依赖: socketio
* 描述: 基于标准Server命名空间实现的测试用Mock / Test Mock based on standard Server namespace
"""

from typing import Any

from socketio import AsyncServer

from a2c_smcp.server.auth import AuthenticationProvider
from a2c_smcp.server.namespace import SMCPNamespace
from a2c_smcp.server.sync_namespace import SyncSMCPNamespace
from a2c_smcp.smcp import SMCP_NAMESPACE
from a2c_smcp.utils.logger import logger


class _PassAuthenticationProvider(AuthenticationProvider):
    """
    简易认证提供者：测试环境放行所有连接
    Simple auth provider: allow all connections in tests
    """

    async def authenticate(self, sio, environ, auth, headers):  # type: ignore[override]
        return True


class MockComputerServerNamespace(SMCPNamespace):
    """
    基于标准SMCPNamespace的测试命名空间，保持原有测试记录能力
    Test namespace based on standard SMCPNamespace, preserving action recording for tests
    """

    def __init__(self) -> None:
        super().__init__(auth_provider=_PassAuthenticationProvider())
        # 记录客户端关键操作，供断言使用
        # Record client key operations for assertions
        self.client_operations_record: dict[str, tuple[str, Any]] = {}

    async def on_connect(self, sid: str, environ: dict, auth: dict | None = None) -> bool:  # type: ignore[override]
        # 先执行标准认证与连接流程，再记录
        # Execute standard auth/connection then record
        result = await super().on_connect(sid, environ, auth)
        self.client_operations_record[sid] = ("connect", None)
        logger.info(f"Client {sid} 已连接 / connected")
        return result

    async def on_disconnect(self, sid: str) -> None:  # type: ignore[override]
        self.client_operations_record[sid] = ("disconnect", None)
        await super().on_disconnect(sid)

    async def enter_room(self, sid: str, room: str, namespace: str | None = None) -> None:  # type: ignore[override]
        await super().enter_room(sid, room, namespace)
        self.client_operations_record[sid] = ("enter_room", room)

    async def leave_room(self, sid: str, room: str, namespace: str | None = None) -> None:  # type: ignore[override]
        self.client_operations_record[sid] = ("leave_room", room)
        await super().leave_room(sid, room, namespace)

    async def on_server_join_office(self, sid: str, data):  # type: ignore[override]
        # 记录加入办公室事件 / record join office event
        self.client_operations_record[sid] = ("server_join_office", data)
        return await super().on_server_join_office(sid, data)

    async def on_server_leave_office(self, sid: str, data):  # type: ignore[override]
        # 记录离开办公室事件 / record leave office event
        self.client_operations_record[sid] = ("server_leave_office", data)
        return await super().on_server_leave_office(sid, data)

    async def on_server_update_config(self, sid: str, data):  # type: ignore[override]
        # 记录更新配置事件 / record update config event
        self.client_operations_record[sid] = ("server_update_config", data)
        return await super().on_server_update_config(sid, data)


class MockComputerServerSyncNamespace(SyncSMCPNamespace):
    """
    同步版本的测试命名空间
    Synchronous version test namespace
    """

    def __init__(self) -> None:
        super().__init__(auth_provider=_PassAuthenticationProvider())
        self.client_operations_record: dict[str, tuple[str, Any]] = {}

    def on_connect(self, sid: str, environ: dict, auth: dict | None = None) -> bool:  # type: ignore[override]
        result = super().on_connect(sid, environ, auth)
        self.client_operations_record[sid] = ("connect", None)
        logger.info(f"Client {sid} 已连接 / connected (sync)")
        return result

    def on_disconnect(self, sid: str) -> None:  # type: ignore[override]
        self.client_operations_record[sid] = ("disconnect", None)
        super().on_disconnect(sid)

    def enter_room(self, sid: str, room: str, namespace: str | None = None) -> None:  # type: ignore[override]
        super().enter_room(sid, room, namespace)
        self.client_operations_record[sid] = ("enter_room", room)

    def leave_room(self, sid: str, room: str, namespace: str | None = None) -> None:  # type: ignore[override]
        self.client_operations_record[sid] = ("leave_room", room)
        super().leave_room(sid, room, namespace)


def create_computer_test_socketio() -> AsyncServer:
    """
    创建用于测试的Socket.IO服务器（异步）
    Create Async Socket.IO server for tests
    """
    sio = AsyncServer(
        async_mode="asgi",
        logger=True,
        engineio_logger=True,
        cors_allowed_origins="*",
        ping_timeout=10,
        ping_interval=10,
        async_handlers=True,
    )

    namespace = MockComputerServerNamespace()
    sio.register_namespace(namespace)

    # 兼容旧引用路径：通过命名空间映射可在测试中按 SMCP_NAMESPACE 取回
    # Backward compatible: can fetch namespace via SMCP_NAMESPACE in tests
    assert SMCP_NAMESPACE in (namespace.namespace,)

    return sio
