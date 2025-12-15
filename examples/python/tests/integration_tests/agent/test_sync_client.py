# -*- coding: utf-8 -*-
# filename: test_sync_client.py
# @Time    : 2025/9/30 22:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文：SMCP 同步 Agent 客户端（SMCPAgentClient）的集成测试。
English: Integration tests for SMCPAgentClient (synchronous).
"""

import socket
import threading
import time
from typing import Literal
from unittest.mock import patch

import pytest
from mcp.types import CallToolResult
from socketio import Client, WSGIApp
from werkzeug.serving import make_server

from a2c_smcp.agent.auth import DefaultAgentAuthProvider
from a2c_smcp.agent.sync_client import SMCPAgentClient
from a2c_smcp.agent.types import AgentEventHandler
from a2c_smcp.smcp import (
    JOIN_OFFICE_EVENT,
    SMCP_NAMESPACE,
    EnterOfficeNotification,
    EnterOfficeReq,
    LeaveOfficeNotification,
    SMCPTool,
    UpdateMCPConfigNotification,
)
from a2c_smcp.utils.logger import logger
from tests.integration_tests.mock_sync_smcp_server import create_sync_smcp_socketio


@pytest.fixture
def sync_server_port() -> int:
    """动态分配可用端口 / Dynamically allocate available port"""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# 创建同步 SMCP 服务器
sio = create_sync_smcp_socketio()
sio.eio.start_service_task = False  # 禁用监控任务避免关闭时出错
wsgi_app = WSGIApp(sio, socketio_path="/socket.io")


class ServerThread(threading.Thread):
    """多线程服务器管理类"""

    def __init__(self, app, host: str, port: int) -> None:
        super().__init__(daemon=True)
        # threaded=True 非常重要，允许多线程处理客户端请求
        self.server = make_server(host, port, app, threaded=True)
        self.host = host
        self.port = port

    def run(self) -> None:
        logger.info(f"Starting Werkzeug server on {self.host}:{self.port}")
        self.server.serve_forever()

    def shutdown(self) -> None:
        logger.info("Shutting down Werkzeug server...")
        self.server.shutdown()


class _EH(AgentEventHandler):
    """
    中文：同步事件处理器，记录回调数据。
    English: Sync event handler recording callbacks.
    """

    def __init__(self) -> None:
        self.enter_events: list[EnterOfficeNotification] = []
        self.leave_events: list[LeaveOfficeNotification] = []
        self.update_events: list[UpdateMCPConfigNotification] = []
        self.tools_received: list[tuple[str, list[SMCPTool]]] = []

    def on_computer_enter_office(self, data: EnterOfficeNotification, sio: SMCPAgentClient) -> None:
        self.enter_events.append(data)

    def on_computer_leave_office(self, data: LeaveOfficeNotification, sio: SMCPAgentClient) -> None:
        self.leave_events.append(data)

    def on_computer_update_config(self, data: UpdateMCPConfigNotification, sio: SMCPAgentClient) -> None:
        logger.info(f"[DEBUG] UpdateMCPConfigNotification received: computer={data.get('computer')}")
        self.update_events.append(data)

    def on_tools_received(self, computer: str, tools: list[SMCPTool], sio: SMCPAgentClient) -> None:
        self.tools_received.append((computer, tools))


@pytest.fixture
def startup_and_shutdown_sync_smcp_server(sync_server_port: int):
    """启动和关闭同步 SMCP 服务器的测试夹具"""
    server_thread = ServerThread(wsgi_app, "localhost", sync_server_port)
    server_thread.start()
    logger.info("Starting SMCP server...")
    time.sleep(0.5)  # 等待服务器启动
    yield sync_server_port
    logger.info("Shutting down SMCP server...")
    server_thread.shutdown()


def _join_office(client: Client, role: Literal["computer", "agent"], office_id: str, name: str) -> None:
    """
    中文：通过 server:join_office 进入办公室（同步）。
    English: Join office via server:join_office (sync).
    """
    payload: EnterOfficeReq = {"role": role, "office_id": office_id, "name": name}
    ok, err = client.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)
    assert ok  # 只检查成功状态，忽略返回消息


def test_agent_receives_enter_and_tools_sync(startup_and_shutdown_sync_smcp_server):
    """
    中文：验证同步Agent收到Computer进入办公室事件，并自动拉取工具列表。
    English: Verify sync Agent receives enter event and auto fetches tools.
    """
    port = startup_and_shutdown_sync_smcp_server
    handler = _EH()
    office_id = "office-sync-1"
    auth = DefaultAgentAuthProvider(agent_id="robot-sync-1", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth, event_handler=handler)

    # Agent 连接并加入办公室
    agent.connect_to_server(
        f"http://localhost:{port}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )

    agent.join_office(office_id=office_id, agent_name="robot-sync-1", namespace=SMCP_NAMESPACE)

    # 使用线程启动Computer客户端
    def run_computer_client() -> None:
        logger.info("Starting Computer Client...")
        computer = Client()
        computer.connect(
            f"http://localhost:{port}",
            namespaces=[SMCP_NAMESPACE],
            socketio_path="/socket.io",
        )
        _join_office(computer, role="computer", office_id=office_id, name="comp-sync-01")

        # 等待断开信号
        disconnect_event.wait()
        computer.disconnect()

    disconnect_event = threading.Event()
    computer_thread = threading.Thread(target=run_computer_client)
    computer_thread.start()

    # 等待事件传播
    time.sleep(1.0)

    # 验证事件和工具接收
    assert handler.enter_events, "应收到进入办公室事件"
    assert handler.tools_received, "应收到工具列表"
    assert handler.tools_received[0][1][0]["name"] == "echo"

    # 清理
    disconnect_event.set()
    computer_thread.join()
    agent.disconnect()


def test_agent_tool_call_roundtrip_sync(startup_and_shutdown_sync_smcp_server):
    """
    中文：验证同步Agent发起工具调用，Computer返回CallToolResult。
    English: Verify sync Agent tool-call roundtrip.
    """
    port = startup_and_shutdown_sync_smcp_server
    handler = _EH()
    office_id = "office-sync-2"
    auth = DefaultAgentAuthProvider(agent_id="robot-sync-2", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth, event_handler=handler)

    # Agent 连接并加入办公室
    agent.connect_to_server(
        f"http://localhost:{port}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )

    agent.join_office(office_id=office_id, agent_name="robot-sync-2", namespace=SMCP_NAMESPACE)

    # 使用线程启动Computer客户端
    def run_computer_client() -> None:
        logger.info("Starting Computer Client...")
        computer = Client()
        computer.connect(
            f"http://localhost:{port}",
            namespaces=[SMCP_NAMESPACE],
            socketio_path="/socket.io",
        )
        _join_office(computer, role="computer", office_id=office_id, name="comp-sync-02")

        # 等待断开信号
        disconnect_event.wait()
        computer.disconnect()

    disconnect_event = threading.Event()
    computer_thread = threading.Thread(target=run_computer_client)
    computer_thread.start()

    # 等待Computer加入
    time.sleep(0.5)

    # 发起工具调用（使用Mock服务器返回的结果）
    res = agent.emit_tool_call(
        computer="mock_computer_id",  # 使用模拟的computer ID
        tool_name="echo",
        params={"text": "hi"},
        timeout=5,
    )

    assert isinstance(res, CallToolResult)
    assert not res.isError
    assert any(getattr(c, "text", None) == "mock tool result" for c in res.content)

    # 清理
    disconnect_event.set()
    computer_thread.join()
    agent.disconnect()


def test_agent_receives_update_config_sync(startup_and_shutdown_sync_smcp_server):
    """
    中文：验证当Computer发出更新配置，Agent收到并再次拉取工具列表（同步）。
    English: Verify sync Agent receives update-config and re-fetches tools.
    """
    port = startup_and_shutdown_sync_smcp_server
    handler = _EH()
    office_id = "office-sync-3"
    auth = DefaultAgentAuthProvider(agent_id="robot-sync-3", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth, event_handler=handler)

    # Agent 连接并加入办公室
    agent.connect_to_server(
        f"http://localhost:{port}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )

    agent.join_office(office_id=office_id, agent_name="robot-sync-3", namespace=SMCP_NAMESPACE)

    # 使用线程启动Computer客户端
    def run_computer_client() -> None:
        logger.info("Starting Computer Client...")
        computer = Client()
        computer.connect(
            f"http://localhost:{port}",
            namespaces=[SMCP_NAMESPACE],
            socketio_path="/socket.io",
        )
        computer_name = "comp-sync-03"
        logger.info(f"[DEBUG] Computer name: {computer_name}")
        _join_office(computer, role="computer", office_id=office_id, name=computer_name)

        # 等待初次工具拉取
        time.sleep(0.5)

        # 触发配置更新
        computer_sid = computer.namespaces[SMCP_NAMESPACE]
        logger.info(f"[DEBUG] Computer SID: {computer_sid}")
        ok, err = computer.call(
            "server:update_config",
            {"computer": computer_sid},
            namespace=SMCP_NAMESPACE,
            timeout=3,
        )
        assert ok and err is None

        # 等待断开信号
        disconnect_event.wait()
        computer.disconnect()

    disconnect_event = threading.Event()
    computer_thread = threading.Thread(target=run_computer_client)
    computer_thread.start()

    # 等待事件传播和配置更新
    time.sleep(2.0)

    # 验证收到了更新配置事件或工具重新拉取
    assert handler.update_events or len(handler.tools_received) >= 2

    # 清理
    disconnect_event.set()
    computer_thread.join()
    agent.disconnect()


def test_validate_emit_event_blocks_invalid():
    """
    中文：验证同步客户端对非法事件名进行阻断（来源于 BaseAgentClient.validate_emit_event）。
    English: Verify invalid events are blocked by validate_emit_event.
    """
    auth = DefaultAgentAuthProvider(agent_id="robot-x", office_id="office-x")
    agent = SMCPAgentClient(auth_provider=auth)

    with pytest.raises(ValueError):
        agent.emit("notify:abc", {})

    with pytest.raises(ValueError):
        agent.call("agent:do_something", {})


def test_get_computers_in_office_sync(startup_and_shutdown_sync_smcp_server):
    """
    中文：验证同步Agent可以获取房间内所有Computer的信息。
    English: Verify sync Agent can get all computers info in the office.
    """
    port = startup_and_shutdown_sync_smcp_server
    handler = _EH()
    office_id = "office-sync-4"
    auth = DefaultAgentAuthProvider(agent_id="robot-sync-4", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth, event_handler=handler)

    # Agent连接并加入办公室 / Agent connects and joins office
    agent.connect_to_server(
        f"http://localhost:{port}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )
    agent.join_office(office_id=office_id, agent_name="robot-sync-4", namespace=SMCP_NAMESPACE)

    # 使用线程启动两个Computer客户端 / Start two Computer clients in threads
    def run_computer_client(computer_name: str) -> None:
        logger.info(f"Starting Computer Client: {computer_name}")
        computer = Client()
        computer.connect(
            f"http://localhost:{port}",
            namespaces=[SMCP_NAMESPACE],
            socketio_path="/socket.io",
        )
        _join_office(computer, role="computer", office_id=office_id, name=computer_name)

        # 等待断开信号 / Wait for disconnect signal
        disconnect_event.wait()
        computer.disconnect()

    disconnect_event = threading.Event()
    computer_thread1 = threading.Thread(target=run_computer_client, args=("comp-sync-04-1",))
    computer_thread2 = threading.Thread(target=run_computer_client, args=("comp-sync-04-2",))
    computer_thread1.start()
    computer_thread2.start()

    # 等待所有客户端加入完成 / Wait for all clients to join
    time.sleep(1.0)

    # 调用get_computers_in_office获取Computer列表 / Call get_computers_in_office to get computers list
    computers = agent.get_computers_in_office(office_id)

    # 验证返回的Computer数量 / Verify number of computers returned
    assert len(computers) == 2, f"Expected 2 computers, got {len(computers)}"

    # 验证所有返回的会话都是computer角色 / Verify all returned sessions are computer role
    assert all(c["role"] == "computer" for c in computers), "All sessions should be computer role"

    # 验证Computer名称 / Verify computer names
    computer_names = {c["name"] for c in computers}
    assert "comp-sync-04-1" in computer_names, "comp-sync-04-1 should be in the list"
    assert "comp-sync-04-2" in computer_names, "comp-sync-04-2 should be in the list"

    # 从每个Computer获取工具列表 / Get tools list from each computer
    for computer in computers:
        computer_sid = computer["sid"]
        logger.info(f"Getting tools from computer: {computer['name']} (sid: {computer_sid})")

        # 调用get_tools_from_computer获取工具列表 / Call get_tools_from_computer to get tools list
        tools_ret = agent.get_tools_from_computer(computer_sid)

        # 验证返回结果 / Verify the result
        assert tools_ret is not None, f"Failed to get tools from computer {computer['name']}"
        assert tools_ret.get("tools"), "GetToolsRet should have 'tools' attribute"
        assert tools_ret.get("req_id"), "GetToolsRet should have 'req_id' attribute"
        logger.info(f"Successfully got {len(tools_ret['tools'])} tools from computer {computer['name']}")

    # 清理 / Cleanup
    disconnect_event.set()
    computer_thread1.join()
    computer_thread2.join()
    agent.disconnect()


def test_get_computers_in_office_empty_sync(startup_and_shutdown_sync_smcp_server):
    """
    中文：验证当房间内没有Computer时，同步Agent返回空列表。
    English: Verify sync Agent returns empty list when no computers in office.
    """
    port = startup_and_shutdown_sync_smcp_server
    handler = _EH()
    office_id = "office-sync-5"
    auth = DefaultAgentAuthProvider(agent_id="robot-sync-5", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth, event_handler=handler)

    # Agent连接并加入办公室 / Agent connects and joins office
    agent.connect_to_server(
        f"http://localhost:{port}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )
    agent.join_office(office_id=office_id, agent_name="robot-sync-5", namespace=SMCP_NAMESPACE)

    # 等待连接完成 / Wait for connection to complete
    time.sleep(0.5)

    # 调用get_computers_in_office，应该返回空列表 / Call get_computers_in_office, should return empty list
    computers = agent.get_computers_in_office(office_id)

    # 验证返回空列表 / Verify empty list is returned
    assert len(computers) == 0, f"Expected 0 computers, got {len(computers)}"

    # 清理 / Cleanup
    agent.disconnect()


def test_sync_agent_on_computer_enter_office_with_mock(startup_and_shutdown_sync_smcp_server):
    """
    中文：测试同步Agent Client能自动响应Computer Client的ENTER_OFFICE_EVENT。
    English: Test that sync Agent Client responds to Computer Client's ENTER_OFFICE_EVENT.
    """
    port = startup_and_shutdown_sync_smcp_server
    office_id = "test-office-mock"
    auth = DefaultAgentAuthProvider(agent_id="mock_robot_id", office_id=office_id)
    agent_client = SMCPAgentClient(auth_provider=auth)

    # Mock process_tools_response 方法来验证是否被调用
    with patch.object(agent_client, "process_tools_response") as mock_process_tools_response:
        # Agent 连接并加入办公室
        agent_client.connect_to_server(
            f"http://localhost:{port}",
            namespace=SMCP_NAMESPACE,
            socketio_path="/socket.io",
        )

        agent_client.call(
            JOIN_OFFICE_EVENT,
            {"office_id": office_id, "role": "agent", "name": "mock_robot_id"},
            namespace=SMCP_NAMESPACE,
        )

        # 使用线程启动Computer客户端
        def run_computer_client() -> None:
            logger.info("Starting Computer Client...")
            computer_id = "mock_computer_sid"
            computer_client = Client()
            computer_client.connect(
                f"http://localhost:{port}",
                namespaces=[SMCP_NAMESPACE],
                socketio_path="/socket.io",
            )

            enter_payload: EnterOfficeReq = {
                "office_id": office_id,
                "role": "computer",
                "name": computer_id,
            }
            logger.info("Sending ENTER_OFFICE_EVENT...")
            computer_client.call(JOIN_OFFICE_EVENT, enter_payload, namespace=SMCP_NAMESPACE)

            disconnect_event.wait()
            computer_client.disconnect()

        disconnect_event = threading.Event()
        computer_thread = threading.Thread(target=run_computer_client)
        computer_thread.start()

        # 等待Agent Client响应
        time.sleep(1.0)

        # 断言process_tools_response被触发（即Agent Client响应了事件）
        mock_process_tools_response.assert_called()

        # 断言process_tools_response参数正确
        mock_args, _ = mock_process_tools_response.call_args
        assert mock_args[0]["tools"][0]["name"] == "echo"
        assert mock_args[0]["tools"][0]["description"] == "echo text"

        # 清理
        disconnect_event.set()
        computer_thread.join()
        agent_client.disconnect()
