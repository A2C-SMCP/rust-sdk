"""
* 文件名: sync_base
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: socketio, loguru
* 描述: 同步版本基础Namespace抽象类 / Synchronous Base Namespace abstract class
"""

from typing import Any

from socketio import Namespace

from a2c_smcp.server.sync_auth import SyncAuthenticationProvider
from a2c_smcp.server.types import SID
from a2c_smcp.utils.logger import logger


class SyncBaseNamespace(Namespace):
    """
    同步基础Namespace抽象类，提供通用的连接管理和认证功能
    Synchronous Base Namespace abstract class, provides common connection management and authentication
    """

    def __init__(self, namespace: str, auth_provider: SyncAuthenticationProvider) -> None:
        """
        初始化基础Namespace
        Initialize base namespace
        """
        super().__init__(namespace=namespace)
        self.auth_provider = auth_provider
        # name到sid的映射表，用于通过name查找session
        # name-to-sid mapping table for finding session by name
        self._name_to_sid_map: dict[str, SID] = {}

    def on_connect(self, sid: SID, environ: dict, auth: dict | None = None) -> bool:
        """
        客户端连接事件处理，包含认证逻辑（同步）
        Client connection event handler with authentication (sync)
        """
        try:
            logger.info(f"SocketIO Client {sid} connecting to {self.namespace}...")

            # 提取原始请求头
            # Extract raw request headers
            headers = self._extract_headers(environ)

            # 认证逻辑，直接传递原始数据给用户
            # Authentication logic, pass raw data directly to user
            is_authenticated = self.auth_provider.authenticate(self.server, environ, auth, headers)
            if not is_authenticated:
                raise ConnectionRefusedError("Authentication failed")

            logger.info(f"SocketIO Client {sid} connected successfully to {self.namespace}")
            return True
        except Exception as e:
            logger.error(f"Connection error for {sid}: {e}")
            raise ConnectionRefusedError("Invalid connection request") from e

    def on_disconnect(self, sid: SID) -> None:
        """
        客户端断开连接事件处理（同步）
        Client disconnect event handler (sync)
        """
        logger.info(f"SocketIO Client {sid} disconnecting from {self.namespace}...")

        # 清理name映射
        # Clean up name mapping
        self._unregister_name(sid)

        rooms = self.rooms(sid)
        for room in rooms:
            if room == sid:
                continue
            self.leave_room(sid, room)
        logger.info(f"SocketIO Client {sid} disconnected from {self.namespace}")

    def trigger_event(self, event: str, *args: Any) -> Any:
        """
        触发事件，重写触发逻辑，将冒号转换为下划线（同步）
        Trigger event, override logic to replace ':' with '_' (sync)
        """
        return super().trigger_event(event.replace(":", "_"), *args)

    def _register_name(self, name: str, sid: SID) -> None:
        """
        注册name到sid的映射，如果name已存在则抛出异常
        Register name-to-sid mapping, raise exception if name already exists

        Args:
            name (str): 客户端名称 / Client name
            sid (SID): 客户端连接ID / Client connection ID

        Raises:
            ValueError: 当name已被其他sid使用时 / When name is already used by another sid
        """
        if name in self._name_to_sid_map:
            existing_sid = self._name_to_sid_map[name]
            if existing_sid != sid:
                raise ValueError(f"Name '{name}' already registered by sid '{existing_sid}' in namespace {self.namespace}")
            # 如果是同一个sid重新注册，允许（幂等操作）
            # Allow re-registration by the same sid (idempotent operation)
            logger.debug(f"Name '{name}' re-registered by same sid '{sid}'")
        else:
            self._name_to_sid_map[name] = sid
            logger.debug(f"Registered name '{name}' -> sid '{sid}' in namespace {self.namespace}")

    def _unregister_name(self, sid: SID) -> None:
        """
        注销sid对应的name映射
        Unregister name mapping for the given sid

        Args:
            sid (SID): 客户端连接ID / Client connection ID
        """
        # 通过session直接获取name，避免遍历映射表
        # Get name directly from session to avoid iterating through the map
        session = self.get_session(sid)
        name = session.get("name")

        if name and name in self._name_to_sid_map:
            del self._name_to_sid_map[name]
            logger.debug(f"Unregistered name '{name}' for sid '{sid}' in namespace {self.namespace}")

    def get_sid_by_name(self, name: str) -> SID | None:
        """
        通过name获取对应的sid
        Get sid by name

        Args:
            name (str): 客户端名称 / Client name

        Returns:
            SID | None: 对应的sid，如果不存在则返回None / Corresponding sid, or None if not found
        """
        return self._name_to_sid_map.get(name)

    @staticmethod
    def _extract_headers(environ: dict) -> list:
        """
        从请求环境中提取原始请求头列表
        Extract raw request headers list from request environment
        """
        headers: list = environ.get("asgi", {}).get("scope", {}).get("headers", [])
        if not headers:
            headers = environ.get("HTTP_HEADERS", [])
        return headers
