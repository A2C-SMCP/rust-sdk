# -*- coding: utf-8 -*-
# filename: _local_sync_server.py
# @Time    : 2025/09/30 23:28
# @Author  : A2C-SMCP
"""
中文：仅供本测试包使用的同步 SMCP 服务端（基于 SyncSMCPNamespace），使用放行认证。
English: Sync SMCP server for this test package only (based on SyncSMCPNamespace) with permissive auth.
"""

from typing import Any

from socketio import Namespace, Server, WSGIApp

from a2c_smcp.server import SyncSMCPNamespace
from a2c_smcp.server.sync_auth import SyncAuthenticationProvider


class _PassSyncAuth(SyncAuthenticationProvider):
    def authenticate(self, sio: Server, environ: dict, auth: dict | None, headers: list) -> bool:  # type: ignore[override]
        return True


class LocalSyncSMCPNamespace(SyncSMCPNamespace):
    """同步命名空间，继承自正式实现，仅替换认证。"""

    def __init__(self) -> None:
        super().__init__(auth_provider=_PassSyncAuth())
        # 可选：记录操作
        self.client_operations_record: dict[str, tuple[str, Any]] = {}

    def on_connect(self, sid: str, environ: dict, auth: dict | None = None) -> bool:  # type: ignore[override]
        ok = super().on_connect(sid, environ, auth)
        self.client_operations_record[sid] = ("connect", None)
        return ok


def create_local_sync_server() -> tuple[Server, Namespace, WSGIApp]:
    """创建同步 Socket.IO Server 并注册本地命名空间，返回 (sio, namespace, wsgi_app)。"""
    sio = Server(
        cors_allowed_origins="*",
        ping_timeout=60,
        ping_interval=25,
        async_handlers=True,  # 如果想使用 call 方法，则必定需要将此参数设置为True
        always_connect=True,
    )
    ns = LocalSyncSMCPNamespace()
    sio.register_namespace(ns)
    app = WSGIApp(sio, socketio_path="/socket.io")
    return sio, ns, app
