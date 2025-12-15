# -*- coding: utf-8 -*-
# filename: conftest.py
# @Time    : 2025/10/05 15:47
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: e2e Agent 测试公共夹具。启动真实 Socket.IO HTTP 服务器，并提供 Computer 真实客户端模拟。
English: Common fixtures for e2e Agent tests. Boots a real Socket.IO HTTP server and provides real Computer client mock.
"""

from __future__ import annotations

import contextlib
import multiprocessing
import socket
import time
from collections.abc import Iterator
from multiprocessing.synchronize import Event
from typing import Any

import pytest
import socketio
from socketio import Namespace, Server, WSGIApp
from werkzeug.serving import make_server

from a2c_smcp.server import SyncSMCPNamespace
from a2c_smcp.server.sync_auth import SyncAuthenticationProvider
from a2c_smcp.smcp import GET_DESKTOP_EVENT, GET_TOOLS_EVENT, SMCP_NAMESPACE, TOOL_CALL_EVENT

# ============================================================================
# 中文: 本地同步服务器创建函数
# English: Local sync server creation functions
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
        ping_timeout=60,
        ping_interval=25,
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
def server_endpoint() -> Iterator[str]:
    """
    中文: 提供形如 http://127.0.0.1:PORT 的服务端地址。
    English: Provide server endpoint like http://127.0.0.1:PORT
    """
    with run_http_server() as (host, port):
        yield f"http://{host}:{port}"


@contextlib.contextmanager
def _mock_computer_client(url: str) -> Iterator[socketio.Client]:
    """
    中文: 创建并连接一个模拟 Computer 的 socketio.Client，自动注册工具处理器。
    English: Create and connect a mock Computer socketio.Client, auto register tool handlers.
    """
    client = socketio.Client()

    # 注册 Computer 端的请求处理器 / Register Computer-side request handlers
    def _on_get_tools(data):
        # 返回最简工具列表 / minimal tool list
        return {
            "tools": [
                {
                    "name": "echo",
                    "description": "echo input",
                    "params_schema": {"type": "object", "properties": {"message": {"type": "string"}}},
                    "return_schema": {"type": "object"},
                },
                {
                    "name": "add",
                    "description": "add two numbers",
                    "params_schema": {
                        "type": "object",
                        "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                    },
                    "return_schema": {"type": "object"},
                },
            ],
            "req_id": data.get("req_id", "r1"),
        }

    def _on_get_desktop(data):
        return {"desktops": ["window://1", "window://2"], "req_id": data.get("req_id", "r2")}

    def _on_tool_call(data):
        # 简单的工具调用实现 / Simple tool call implementation
        tool_name = data.get("tool_name")
        params = data.get("params", {})

        if tool_name == "echo":
            return {
                "content": [{"type": "text", "text": f"Echo: {params.get('message', '')}"}],
                "isError": False,
            }
        elif tool_name == "add":
            a = params.get("a", 0)
            b = params.get("b", 0)
            return {
                "content": [{"type": "text", "text": f"Result: {a + b}"}],
                "isError": False,
            }
        else:
            return {
                "content": [{"type": "text", "text": f"Unknown tool: {tool_name}"}],
                "isError": True,
            }

    client.on(GET_TOOLS_EVENT, _on_get_tools, namespace=SMCP_NAMESPACE)
    client.on(GET_DESKTOP_EVENT, _on_get_desktop, namespace=SMCP_NAMESPACE)
    client.on(TOOL_CALL_EVENT, _on_tool_call, namespace=SMCP_NAMESPACE)

    client.connect(
        url,
        socketio_path="/socket.io",
        namespaces=[SMCP_NAMESPACE],
        transports=["polling"],  # 仅使用轮询，避免WSGI环境的WebSocket升级失败 / force polling
        wait=True,
        wait_timeout=10,
    )
    try:
        yield client
    finally:
        with contextlib.suppress(Exception):
            client.disconnect()


@pytest.fixture()
def mock_computer_client(server_endpoint: str) -> Iterator[socketio.Client]:
    """
    中文: 已连接到 Server 的模拟 Computer 客户端（同步）。
    English: Connected mock Computer client (sync).
    """
    with _mock_computer_client(server_endpoint) as c:
        yield c


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
def async_server_port() -> int:
    """
    中文: 查找可用端口用于异步服务器。
    English: Find an available TCP port for async server.
    """
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


@pytest.fixture
async def async_socketio_server(async_server_port: int):
    """
    中文: 启动基于 SMCPNamespace 的异步测试服务器，返回命名空间。
    English: Start async test server based on SMCPNamespace and return the namespace.
    """
    import time

    from tests.integration_tests.computer.socketio.mock_uv_server import UvicornTestServer

    start = time.time()
    sio, ns, asgi_app = create_local_async_server()

    server = UvicornTestServer(asgi_app, port=async_server_port)
    await server.up()
    print(f"\n[E2E] Async server started on port {async_server_port} in {time.time() - start:.2f}s")
    try:
        yield ns
    finally:
        shutdown_start = time.time()
        # 强制快速关闭，不等待连接清理 / Force fast shutdown without waiting for connection cleanup
        await server.down(force=True)
        print(f"[E2E] Async server shutdown in {time.time() - shutdown_start:.2f}s")


@pytest.fixture()
async def async_mock_computer_client(async_socketio_server, async_server_port: int):
    """
    中文: 已连接到异步 Server 的模拟 Computer 客户端。每个测试创建独立客户端，但复用服务器。
    English: Connected mock Computer client (async). Each test creates its own client but reuses the server.
    """
    import time

    start = time.time()
    client = socketio.AsyncClient()
    print("\n[E2E] Creating async computer client...")

    # 注册 Computer 端的请求处理器 / Register Computer-side request handlers
    async def _on_get_tools(data):
        return {
            "tools": [
                {
                    "name": "echo",
                    "description": "echo input",
                    "params_schema": {"type": "object", "properties": {"message": {"type": "string"}}},
                    "return_schema": {"type": "object"},
                },
                {
                    "name": "add",
                    "description": "add two numbers",
                    "params_schema": {
                        "type": "object",
                        "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                    },
                    "return_schema": {"type": "object"},
                },
            ],
            "req_id": data.get("req_id", "r1"),
        }

    async def _on_get_desktop(data):
        return {"desktops": ["window://1", "window://2"], "req_id": data.get("req_id", "r2")}

    async def _on_tool_call(data):
        tool_name = data.get("tool_name")
        params = data.get("params", {})

        if tool_name == "echo":
            return {
                "content": [{"type": "text", "text": f"Echo: {params.get('message', '')}"}],
                "isError": False,
            }
        elif tool_name == "add":
            a = params.get("a", 0)
            b = params.get("b", 0)
            return {
                "content": [{"type": "text", "text": f"Result: {a + b}"}],
                "isError": False,
            }
        else:
            return {
                "content": [{"type": "text", "text": f"Unknown tool: {tool_name}"}],
                "isError": True,
            }

    client.on(GET_TOOLS_EVENT, _on_get_tools, namespace=SMCP_NAMESPACE)
    client.on(GET_DESKTOP_EVENT, _on_get_desktop, namespace=SMCP_NAMESPACE)
    client.on(TOOL_CALL_EVENT, _on_tool_call, namespace=SMCP_NAMESPACE)

    await client.connect(
        f"http://127.0.0.1:{async_server_port}",
        socketio_path="/socket.io",
        namespaces=[SMCP_NAMESPACE],
        transports=["polling"],  # 仅使用 polling，避免 WebSocket 关闭延迟 / Use polling only to avoid WebSocket close delay
        wait=True,
        wait_timeout=5,
    )
    print(f"[E2E] Async computer client connected in {time.time() - start:.2f}s")
    try:
        yield client
    finally:
        disconnect_start = time.time()
        # 快速断开，不等待清理 / Fast disconnect without waiting for cleanup
        if client.connected:
            await client.eio.disconnect(abort=True)  # 强制断开 / Force disconnect
        print(f"[E2E] Async computer client disconnected in {time.time() - disconnect_start:.2f}s")
