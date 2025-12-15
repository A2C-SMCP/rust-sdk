# -*- coding: utf-8 -*-
# filename: conftest.py
# @Time    : 2025/8/20 14:03
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
import multiprocessing
import socket
import sys
import time
from collections.abc import Callable, Generator
from datetime import timedelta
from pathlib import Path

import anyio
import pytest
import uvicorn
from mcp import ErrorData, McpError, StdioServerParameters, Tool
from mcp import types as types
from mcp.client.session_group import SseServerParameters, StreamableHttpParameters
from mcp.server import Server
from mcp.server.sse import SseServerTransport
from mcp.server.streamable_http import EventCallback, EventId, EventMessage, EventStore, StreamId
from mcp.server.streamable_http_manager import StreamableHTTPSessionManager
from mcp.server.transport_security import TransportSecuritySettings
from mcp.types import TextContent
from pydantic import AnyUrl
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Mount, Route

from a2c_smcp.computer.mcp_clients.http_client import HttpMCPClient

STREAMABLE_HTTP_SERVER_NAME = "test_streamable_http_server"
TEST_SESSION_ID = "test-session-id-12345"
INIT_REQUEST = {
    "jsonrpc": "2.0",
    "method": "initialize",
    "params": {
        "clientInfo": {"name": "test-client", "version": "1.0"},
        "protocolVersion": "2025-03-26",
        "capabilities": {},
    },
    "id": "init-1",
}


class SimpleEventStore(EventStore):
    """Simple in-memory event store for testing."""

    def __init__(self):
        self._events: list[tuple[StreamId, EventId, types.JSONRPCMessage]] = []
        self._event_id_counter = 0

    async def store_event(self, stream_id: StreamId, message: types.JSONRPCMessage) -> EventId:
        """Store an event and return its ID."""
        self._event_id_counter += 1
        event_id = str(self._event_id_counter)
        self._events.append((stream_id, event_id, message))
        return event_id

    async def replay_events_after(
        self,
        last_event_id: EventId,
        send_callback: EventCallback,
    ) -> StreamId | None:
        """Replay events after the specified ID."""
        # Find the stream ID of the last event
        target_stream_id = None
        for stream_id, event_id, _ in self._events:
            if event_id == last_event_id:
                target_stream_id = stream_id
                break

        if target_stream_id is None:
            # If event ID not found, return None
            return None

        # Convert last_event_id to int for comparison
        last_event_id_int = int(last_event_id)

        # Replay only events from the same stream with ID > last_event_id
        for stream_id, event_id, message in self._events:
            if stream_id == target_stream_id and int(event_id) > last_event_id_int:
                await send_callback(EventMessage(message, event_id))

        return target_stream_id


class StreamableHttpServerTest(Server):
    def __init__(self):
        super().__init__(STREAMABLE_HTTP_SERVER_NAME)
        self._lock = None  # Will be initialized in async context

        @self.read_resource()
        async def handle_read_resource(uri: AnyUrl) -> str | bytes:
            if uri.scheme == "foobar":
                return f"Read {uri.host}"
            elif uri.scheme == "slow":
                # Simulate a slow resource
                await anyio.sleep(2.0)
                return f"Slow response from {uri.host}"

            raise ValueError(f"Unknown resource: {uri}")

        @self.list_tools()
        async def handle_list_tools() -> list[Tool]:
            return [
                Tool(
                    name="test_tool",
                    description="A test tool",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="test_tool_with_standalone_notification",
                    description="A test tool that sends a notification",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="long_running_with_checkpoints",
                    description="A long-running tool that sends periodic notifications",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="test_sampling_tool",
                    description="A tool that triggers server-side sampling",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="wait_for_lock_with_notification",
                    description="A tool that sends a notification and waits for lock",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="release_lock",
                    description="A tool that releases the lock",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="trigger_list_changed",
                    description="Trigger tools/resources/prompts listChanged notifications",
                    inputSchema={"type": "object", "properties": {}},
                ),
            ]

        @self.call_tool()
        async def handle_call_tool(name: str, args: dict) -> list[TextContent]:
            ctx = self.request_context

            # When the tool is called, send a notification to test GET stream
            if name == "test_tool_with_standalone_notification":
                await ctx.session.send_resource_updated(uri=AnyUrl("http://test_resource"))
                return [TextContent(type="text", text=f"Called {name}")]

            elif name == "long_running_with_checkpoints":
                # Send notifications that are part of the response stream
                # This simulates a long-running tool that sends logs

                await ctx.session.send_log_message(
                    level="info",
                    data="Tool started",
                    logger="tool",
                    related_request_id=ctx.request_id,  # need for stream association
                )

                await anyio.sleep(0.1)

                await ctx.session.send_log_message(
                    level="info",
                    data="Tool is almost done",
                    logger="tool",
                    related_request_id=ctx.request_id,
                )

                return [TextContent(type="text", text="Completed!")]

            elif name == "test_sampling_tool":
                # Test sampling by requesting the client to sample a message
                sampling_result = await ctx.session.create_message(
                    messages=[
                        types.SamplingMessage(
                            role="user",
                            content=types.TextContent(type="text", text="Server needs client sampling"),
                        ),
                    ],
                    max_tokens=100,
                    related_request_id=ctx.request_id,
                )

                # Return the sampling result in the tool response
                response = sampling_result.content.text if sampling_result.content.type == "text" else None
                return [
                    TextContent(
                        type="text",
                        text=f"Response from sampling: {response}",
                    ),
                ]

            elif name == "wait_for_lock_with_notification":
                # Initialize lock if not already done
                if self._lock is None:
                    self._lock = anyio.Event()

                # First send a notification
                await ctx.session.send_log_message(
                    level="info",
                    data="First notification before lock",
                    logger="lock_tool",
                    related_request_id=ctx.request_id,
                )

                # Now wait for the lock to be released
                await self._lock.wait()

                # Send second notification after lock is released
                await ctx.session.send_log_message(
                    level="info",
                    data="Second notification after lock",
                    logger="lock_tool",
                    related_request_id=ctx.request_id,
                )

                return [TextContent(type="text", text="Completed")]

            elif name == "release_lock":
                assert self._lock is not None, "Lock must be initialized before releasing"

                # Release the lock
                self._lock.set()
                return [TextContent(type="text", text="Lock released")]
            elif name == "trigger_list_changed":
                # 发送列表变更通知 / send list-changed notifications
                await ctx.session.send_tool_list_changed()
                await ctx.session.send_resource_list_changed()
                await ctx.session.send_prompt_list_changed()
                return [TextContent(type="text", text="changes triggered")]

            elif name == "nonexistent_tool":
                raise McpError(
                    error=types.ErrorData(code=404, message="OOPS! no tool with that name was found"),
                )

            return [TextContent(type="text", text=f"Called {name}")]


def create_app(is_json_response_enabled=False, event_store: EventStore | None = None) -> Starlette:
    """Create a Starlette application for testing using the session manager.

    Args:
        is_json_response_enabled: If True, use JSON responses instead of SSE streams.
        event_store: Optional event store for testing resumability.
    """
    # Create server instance
    server = StreamableHttpServerTest()

    # Create the session manager
    security_settings = TransportSecuritySettings(
        allowed_hosts=["127.0.0.1:*", "localhost:*"],
        allowed_origins=["http://127.0.0.1:*", "http://localhost:*"],
    )
    session_manager = StreamableHTTPSessionManager(
        app=server,
        event_store=event_store,
        json_response=is_json_response_enabled,
        security_settings=security_settings,
    )

    # Create an ASGI application that uses the session manager
    app = Starlette(
        debug=True,
        routes=[
            Mount("/mcp", app=session_manager.handle_request),
        ],
        lifespan=lambda app: session_manager.run(),  # type: ignore
    )

    return app


def run_streamable_http_server(port: int, is_json_response_enabled=False, event_store: EventStore | None = None) -> None:
    """Run the test streamable HTTP server.

    Args:
        port: Port to listen on.
        is_json_response_enabled: If True, use JSON responses instead of SSE streams.
        event_store: Optional event store for testing resumability.
    """

    app = create_app(is_json_response_enabled, event_store)
    # Configure server
    config = uvicorn.Config(
        app=app,
        host="127.0.0.1",
        port=port,
        log_level="info",
        limit_concurrency=10,
        timeout_keep_alive=5,
        access_log=False,
    )

    # Start the server
    server = uvicorn.Server(config=config)

    # This is important to catch exceptions and prevent test hangs
    try:
        server.run()
    except Exception:
        import traceback

        traceback.print_exc()


@pytest.fixture
def basic_server_port() -> int:
    """Find an available port for the basic server."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def json_server_port() -> int:
    """Find an available port for the JSON response server."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def basic_server(basic_server_port: int) -> Generator[None, None, None]:
    """Start a basic server."""
    proc = multiprocessing.Process(target=run_streamable_http_server, kwargs={"port": basic_server_port}, daemon=True)
    proc.start()

    # Wait for server to be running
    max_attempts = 20
    attempt = 0
    while attempt < max_attempts:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.connect(("127.0.0.1", basic_server_port))
                break
        except ConnectionRefusedError:
            time.sleep(0.1)
            attempt += 1
    else:
        raise RuntimeError(f"Server failed to start after {max_attempts} attempts")

    yield

    # Clean up
    proc.kill()
    proc.join(timeout=2)


@pytest.fixture
def event_store() -> SimpleEventStore:
    """Create a test event store."""
    return SimpleEventStore()


@pytest.fixture
def event_server_port() -> int:
    """Find an available port for the event store server."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def event_server(event_server_port: int, event_store: SimpleEventStore) -> Generator[tuple[SimpleEventStore, str], None, None]:
    """Start a server with event store enabled."""
    proc = multiprocessing.Process(
        target=run_streamable_http_server,
        kwargs={"port": event_server_port, "event_store": event_store},
        daemon=True,
    )
    proc.start()

    # Wait for server to be running
    max_attempts = 20
    attempt = 0
    while attempt < max_attempts:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.connect(("127.0.0.1", event_server_port))
                break
        except ConnectionRefusedError:
            time.sleep(0.1)
            attempt += 1
    else:
        raise RuntimeError(f"Server failed to start after {max_attempts} attempts")

    yield event_store, f"http://127.0.0.1:{event_server_port}"

    # Clean up
    proc.kill()
    proc.join(timeout=2)


@pytest.fixture
def json_response_server(json_server_port: int) -> Generator[None, None, None]:
    """Start a server with JSON response enabled."""
    proc = multiprocessing.Process(
        target=run_streamable_http_server,
        kwargs={"port": json_server_port, "is_json_response_enabled": True},
        daemon=True,
    )
    proc.start()

    # Wait for server to be running
    max_attempts = 20
    attempt = 0
    while attempt < max_attempts:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.connect(("127.0.0.1", json_server_port))
                break
        except ConnectionRefusedError:
            time.sleep(0.1)
            attempt += 1
    else:
        raise RuntimeError(f"Server failed to start after {max_attempts} attempts")

    yield

    # Clean up
    proc.kill()
    proc.join(timeout=2)


@pytest.fixture
def basic_server_url(basic_server_port: int) -> str:
    """Get the URL for the basic test server."""
    return f"http://127.0.0.1:{basic_server_port}"


@pytest.fixture
def json_server_url(json_server_port: int) -> str:
    """Get the URL for the JSON response test server."""
    return f"http://127.0.0.1:{json_server_port}"


@pytest.fixture
def http_params(basic_server_url: str) -> StreamableHttpParameters:
    """
    # 根据fixture动态生成StreamableHttpParameters，指向运行时server
    # Dynamically generate StreamableHttpParameters for runtime server
    """
    return StreamableHttpParameters(
        url=f"{basic_server_url}/mcp",
        timeout=timedelta(seconds=20),
    )


@pytest.fixture
def http_client(http_params: StreamableHttpParameters) -> HttpMCPClient:
    """
    # 创建HttpMCPClient实例
    # Create HttpMCPClient instance
    """
    return HttpMCPClient(http_params)


SSE_SERVER_NAME = "test_server_for_SSE"


@pytest.fixture(scope="session")
def server_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="session")
def server_url(server_port: int) -> str:
    return f"http://127.0.0.1:{server_port}"


class SseServerTest(Server):
    def __init__(self) -> None:
        super().__init__(SSE_SERVER_NAME)

        @self.read_resource()
        async def handle_read_resource(uri: AnyUrl) -> str | bytes:
            if uri.scheme == "foobar":
                return f"Read {uri.host}"
            elif uri.scheme == "slow":
                # Simulate a slow resource
                await anyio.sleep(2.0)
                return f"Slow response from {uri.host}"

            raise McpError(error=ErrorData(code=404, message="OOPS! no resource with that URI was found"))

        @self.list_tools()
        async def handle_list_tools() -> list[Tool]:
            return [
                Tool(
                    name="test_tool",
                    description="A test tool",
                    inputSchema={"type": "object", "properties": {}},
                ),
                Tool(
                    name="trigger_list_changed",
                    description="Trigger tools/resources/prompts listChanged notifications",
                    inputSchema={"type": "object", "properties": {}},
                ),
            ]

        @self.call_tool()
        async def handle_call_tool(name: str, args: dict) -> list[TextContent]:
            if name == "test_tool":
                return [TextContent(type="text", text=f"Called {name}")]
            elif name == "trigger_list_changed":
                ctx = self.request_context
                await ctx.session.send_tool_list_changed()
                await ctx.session.send_resource_list_changed()
                await ctx.session.send_prompt_list_changed()
                return [TextContent(type="text", text="changes triggered")]
            else:
                raise McpError(error=ErrorData(code=404, message="OOPS! no tool with that name was found"))


def make_server_app() -> Starlette:
    """创建测试 Starlette app，带有 SSE 传输\nCreate test Starlette app with SSE transport"""
    # 配置测试安全设置 Configure security for testing
    security_settings: TransportSecuritySettings = TransportSecuritySettings(
        allowed_hosts=["127.0.0.1:*", "localhost:*"],
        allowed_origins=["http://127.0.0.1:*", "http://localhost:*"],
    )
    sse: SseServerTransport = SseServerTransport("/messages/", security_settings=security_settings)
    server: SseServerTest = SseServerTest()

    async def handle_sse(request: Request) -> Response:
        async with sse.connect_sse(request.scope, request.receive, request._send) as streams:
            await server.run(streams[0], streams[1], server.create_initialization_options())
        return Response()

    app: Starlette = Starlette(
        routes=[
            Route("/sse", endpoint=handle_sse),
            Mount("/messages/", app=sse.handle_post_message),
        ],
    )

    return app


def run_sse_server(server_port: int) -> None:
    app: Starlette = make_server_app()
    server = uvicorn.Server(config=uvicorn.Config(app=app, host="127.0.0.1", port=server_port, log_level="error"))
    print(f"starting server on {server_port}")
    server.run()

    # Give server time to start
    while not server.started:
        print("waiting for server to start")
        time.sleep(0.5)


@pytest.fixture(scope="session")
def sse_server(server_port: int) -> Generator[None, None, None]:
    proc: multiprocessing.Process = multiprocessing.Process(target=run_sse_server, kwargs={"server_port": server_port}, daemon=True)
    print("starting process")
    proc.start()

    # Wait for server to be running
    max_attempts: int = 20
    attempt: int = 0
    print("waiting for server to start")
    while attempt < max_attempts:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.connect(("127.0.0.1", server_port))
                break
        except ConnectionRefusedError:
            time.sleep(0.1)
            attempt += 1
    else:
        raise RuntimeError(f"Server failed to start after {max_attempts} attempts")

    yield

    print("killing server")
    # Signal the server to stop
    proc.kill()
    proc.join(timeout=2)
    if proc.is_alive():
        print("server process failed to terminate")


@pytest.fixture
def sse_params(server_url: str) -> SseServerParameters:
    """
    根据fixture动态生成SseServerParameters，指向运行时服务
    Dynamically generate SseServerParameters based on fixture, pointing to runtime server
    """
    return SseServerParameters(url=f"{server_url}/sse")


@pytest.fixture
def track_state() -> tuple[Callable[[str, str], None], list[tuple[str, str]]]:
    """
    跟踪状态变化的辅助函数
    Helper for tracking state changes
    """
    state_history: list[tuple[str, str]] = []

    def callback(from_state: str, to_state: str) -> None:
        state_history.append((from_state, to_state))

    return callback, state_history


TEST_DIR: Path = Path(__file__).parent.parent.parent
MCP_SERVER_SCRIPT: Path = TEST_DIR / "integration_tests" / "computer" / "mcp_servers" / "direct_execution.py"


@pytest.fixture
def stdio_params() -> StdioServerParameters:
    """提供 StdioServerParameters 配置 Provide StdioServerParameters config"""
    return StdioServerParameters(command=sys.executable, args=[str(MCP_SERVER_SCRIPT)])
