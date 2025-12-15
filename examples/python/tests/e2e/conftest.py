# -*- coding: utf-8 -*-
# filename: conftest.py
# @Time    : 2025/10/05 16:20
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: E2E 测试根目录公共夹具，提供 Computer-Agent-Server 三者集成测试所需的基础设施。
English: Root-level E2E test fixtures providing infrastructure for Computer-Agent-Server integration tests.
"""

from __future__ import annotations

import contextlib
import json
import multiprocessing
import socket
import sys
import time
from collections.abc import Iterator
from multiprocessing.synchronize import Event
from pathlib import Path
from typing import Any

import pytest
import socketio
from socketio import Namespace, Server, WSGIApp
from werkzeug.serving import make_server

from a2c_smcp.server import SyncSMCPNamespace
from a2c_smcp.server.sync_auth import SyncAuthenticationProvider

# ============================================================================
# 中文: 测试用认证提供者 / English: Test authentication provider
# ============================================================================


class _PassSyncAuth(SyncAuthenticationProvider):
    """中文: 测试用的放行认证提供者 / English: Permissive auth provider for testing"""

    def authenticate(self, sio: Server, environ: dict, auth: dict | None, headers: list) -> bool:  # type: ignore[override]
        return True


class LocalSyncSMCPNamespace(SyncSMCPNamespace):
    """中文: 同步命名空间，继承自正式实现，仅替换认证 / English: Sync namespace with test auth"""

    def __init__(self) -> None:
        super().__init__(auth_provider=_PassSyncAuth())


def create_local_sync_server() -> tuple[Server, Namespace, WSGIApp]:
    """中文: 创建同步 Socket.IO Server 并注册本地命名空间 / English: Create sync Socket.IO Server with local namespace"""
    sio = Server(
        cors_allowed_origins="*",
        ping_timeout=5,  # 中文: 测试环境使用较短超时 / English: Use shorter timeout for testing
        ping_interval=3,  # 中文: 测试环境使用较短间隔 / English: Use shorter interval for testing
        async_handlers=True,  # 如果想使用 call 方法，则必定需要将此参数设置为True / Required for call method
        always_connect=True,
    )
    ns = LocalSyncSMCPNamespace()
    sio.register_namespace(ns)
    app = WSGIApp(sio, socketio_path="/socket.io")
    return sio, ns, app


# ============================================================================
# 中文: 多进程服务器启动函数 / English: Multiprocess server startup
# ============================================================================


def _run_server_process(port: int, ready_event: Event) -> None:
    """
    中文: 在独立进程中运行服务器
    English: Run server in a separate process
    """
    try:
        sio, ns, wsgi_app = create_local_sync_server()
        # 禁用监控任务避免关闭时出错 / Disable monitoring task to avoid shutdown errors
        sio.eio.start_service_task = False

        server = make_server("127.0.0.1", port, wsgi_app, threaded=True)

        # 通知主进程服务器已准备好 / Notify main process that server is ready
        ready_event.set()

        # 运行服务器 / Run server
        server.serve_forever()
    except Exception as e:
        print(f"服务器进程错误 / Server process error: {e}")
        ready_event.set()  # 即使出错也要设置事件，避免主进程无限等待 / Set event even on error


@contextlib.contextmanager
def run_http_server() -> Iterator[tuple[str, int]]:
    """
    中文: 启动一个基于多进程的同步 Socket.IO Server（真实 HTTP 服务），返回 (host, port)。
    English: Start a multiprocess sync Socket.IO server over real HTTP, return (host, port).
    """
    # 选取随机可用端口 / pick a free port
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    host, port = sock.getsockname()
    sock.close()

    # 创建进程间通信事件 / Create inter-process communication event
    ready_event = multiprocessing.Event()

    # 启动服务器进程 / Start server process
    server_process = multiprocessing.Process(
        target=_run_server_process,
        args=(port, ready_event),
        daemon=True,
    )
    server_process.start()

    # 等待服务器准备好 / Wait for server to be ready
    if not ready_event.wait(timeout=10):
        server_process.terminate()
        server_process.join(timeout=2)
        raise RuntimeError("服务器进程启动超时 / Server process startup timeout")

    # 额外等待确保端口完全可用 / Extra wait to ensure port is fully available
    time.sleep(0.3)

    try:
        yield host, port
    finally:
        # 终止服务器进程 / Terminate server process
        if server_process.is_alive():
            server_process.terminate()
            server_process.join(timeout=3)

        # 如果进程仍然存活，强制杀死 / Force kill if still alive
        if server_process.is_alive():
            server_process.kill()
            server_process.join(timeout=1)


@pytest.fixture(scope="session")
def integration_server_endpoint() -> Iterator[str]:
    """
    中文: 提供形如 http://127.0.0.1:PORT 的服务端地址，用于集成测试。
    English: Provide server endpoint like http://127.0.0.1:PORT for integration tests.
    """
    with run_http_server() as (host, port):
        yield f"http://{host}:{port}"


# ============================================================================
# 中文: MCP Server 配置辅助函数 / English: MCP Server config helpers
# ============================================================================


def create_mcp_server_config(
    name: str,
    script_path: str,
    disabled: bool = False,
) -> dict[str, Any]:
    """
    中文: 创建 MCP Server 配置，用于 Computer 端测试
    English: Create MCP Server config for Computer-side testing
    """
    return {
        "name": name,
        "type": "stdio",
        "disabled": disabled,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": {
            "auto_apply": True,
        },
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [script_path],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }


@pytest.fixture
def mcp_server_config_path(tmp_path: Path) -> Path:
    """
    中文: 创建临时 MCP Server 配置文件路径
    English: Create temporary MCP Server config file path
    """
    config_file = tmp_path / "mcp_servers.json"
    # 创建一个基础配置，包含测试用的 MCP Server
    # Create a basic config with test MCP Server
    config = create_mcp_server_config(
        name="e2e-test-server",
        script_path="tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
    )
    config_file.write_text(json.dumps([config], ensure_ascii=False), encoding="utf-8")
    return config_file


# ============================================================================
# 中文: 异步服务器相关 fixtures
# English: Async server related fixtures
# ============================================================================


def create_local_async_server() -> tuple[socketio.AsyncServer, socketio.AsyncNamespace, Any]:
    """
    中文: 创建本地异步 SMCP 服务器，用于测试。
    English: Create local async SMCP server for testing.
    """
    from a2c_smcp.server import SMCPNamespace
    from a2c_smcp.server.auth import AuthenticationProvider

    class _PassAsyncAuth(AuthenticationProvider):
        """中文: 测试用的放行认证提供者 / English: Permissive auth provider for testing"""

        async def authenticate(self, sio: socketio.AsyncServer, environ: dict, auth: dict | None, headers: list) -> bool:  # type: ignore[override]
            return True

    class LocalAsyncSMCPNamespace(SMCPNamespace):
        """中文: 异步命名空间，继承自正式实现，仅替换认证 / English: Async namespace with test auth"""

        def __init__(self) -> None:
            super().__init__(auth_provider=_PassAsyncAuth())

    sio = socketio.AsyncServer(
        async_mode="asgi",
        cors_allowed_origins="*",
        ping_timeout=5,  # 中文: 测试环境使用较短超时 / English: Use shorter timeout for testing
        ping_interval=3,  # 中文: 测试环境使用较短间隔 / English: Use shorter interval for testing
        logger=False,
        engineio_logger=False,
    )
    # 避免关闭时后台任务异常 / avoid background task issues on shutdown
    sio.eio.start_service_task = False
    ns = LocalAsyncSMCPNamespace()
    sio.register_namespace(ns)
    app = socketio.ASGIApp(sio, socketio_path="/socket.io")
    return sio, ns, app


@pytest.fixture
def async_integration_server_port() -> int:
    """
    中文: 查找可用端口用于异步集成服务器。
    English: Find an available TCP port for async integration server.
    """
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


@pytest.fixture
async def async_integration_socketio_server(async_integration_server_port: int):
    """
    中文: 启动基于 SMCPNamespace 的异步集成测试服务器，返回命名空间。
    English: Start async integration test server based on SMCPNamespace and return the namespace.
    """
    from tests.integration_tests.computer.socketio.mock_uv_server import UvicornTestServer

    setup_start = time.time()
    sio, ns, asgi_app = create_local_async_server()
    print(f"[E2E Fixture] Server creation took {time.time() - setup_start:.2f}s")

    server_start = time.time()
    server = UvicornTestServer(asgi_app, port=async_integration_server_port)
    await server.up()
    print(f"[E2E Fixture] Server startup took {time.time() - server_start:.2f}s")

    try:
        yield ns
    finally:
        shutdown_start = time.time()
        print(f"[E2E Fixture] Starting shutdown Server {shutdown_start}")
        # 强制快速关闭，不等待连接清理 / Force fast shutdown without waiting for connection cleanup
        await server.down(force=True)
        print(f"[E2E Fixture] Server shutdown took {time.time() - shutdown_start:.2f}s")
