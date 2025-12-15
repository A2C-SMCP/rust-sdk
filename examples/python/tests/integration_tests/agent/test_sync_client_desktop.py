# -*- coding: utf-8 -*-
# filename: test_sync_client_desktop.py
# @Time    : 2025/10/05 13:30
# @Author  : A2C-SMCP
# @Email   : qa@a2c-smcp.local
# @Software: PyTest
"""
中文：SMCP 同步 Agent 客户端桌面协议的集成测试。
English: Integration tests for SMCPAgentClient desktop protocol (synchronous).
"""

import threading
import time

import pytest
from socketio import Client, WSGIApp
from werkzeug.serving import make_server

from a2c_smcp.agent.auth import DefaultAgentAuthProvider
from a2c_smcp.agent.sync_client import SMCPAgentClient
from a2c_smcp.smcp import JOIN_OFFICE_EVENT, SMCP_NAMESPACE, UPDATE_DESKTOP_EVENT
from a2c_smcp.utils.logger import logger
from tests.integration_tests.mock_sync_smcp_server import create_sync_smcp_socketio

TEST_PORT = 8010

sio = create_sync_smcp_socketio()
sio.eio.start_service_task = False
wsgi_app = WSGIApp(sio, socketio_path="/socket.io")


class ServerThread(threading.Thread):
    def __init__(self, app, host: str, port: int) -> None:
        super().__init__(daemon=True)
        self.server = make_server(host, port, app, threaded=True)
        self.host = host
        self.port = port

    def run(self) -> None:
        logger.info(f"Starting Werkzeug server on {self.host}:{self.port}")
        self.server.serve_forever()

    def shutdown(self) -> None:
        logger.info("Shutting down Werkzeug server...")
        self.server.shutdown()


@pytest.fixture
def startup_and_shutdown_sync_smcp_server_desktop():
    server_thread = ServerThread(wsgi_app, "localhost", TEST_PORT)
    server_thread.start()
    time.sleep(0.5)
    yield
    server_thread.shutdown()


def _join_office(client: Client, role: str, office_id: str, name: str) -> None:
    payload = {"role": role, "office_id": office_id, "name": name}
    ok, err = client.call(JOIN_OFFICE_EVENT, payload, namespace=SMCP_NAMESPACE)
    assert ok and err is None


def test_sync_agent_get_desktop_and_update_flow(startup_and_shutdown_sync_smcp_server_desktop):
    office_id = "office-desktop-sync"
    auth = DefaultAgentAuthProvider(agent_id="robot-desktop-sync", office_id=office_id)
    agent = SMCPAgentClient(auth_provider=auth)

    # 连接并加入
    agent.connect_to_server(
        f"http://localhost:{TEST_PORT}",
        namespace=SMCP_NAMESPACE,
        socketio_path="/socket.io",
    )
    agent.join_office(office_id=office_id, agent_name="robot-desktop-sync", namespace=SMCP_NAMESPACE)

    # 启动一个模拟 Computer 客户端
    disconnect = threading.Event()

    def run_computer():
        comp = Client()
        comp.connect(f"http://localhost:{TEST_PORT}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
        _join_office(comp, role="computer", office_id=office_id, name="comp-desktop-01")
        # 触发一次桌面更新广播
        ok, err = comp.call(UPDATE_DESKTOP_EVENT, {"computer": comp.namespaces[SMCP_NAMESPACE]}, namespace=SMCP_NAMESPACE)
        assert ok and err is None
        disconnect.wait()
        comp.disconnect()

    th = threading.Thread(target=run_computer, daemon=True)
    th.start()

    # 主动拉取桌面
    # 注意：这里直接构造请求以确认 server 支持 client:get_desktop 流程
    ret = agent.get_desktop_from_computer("unused", size=1)
    assert isinstance(ret, dict)
    assert ret["desktops"] and ret["req_id"]

    # 清理
    disconnect.set()
    th.join()
    agent.disconnect()
