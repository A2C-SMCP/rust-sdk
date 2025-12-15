# -*- coding: utf-8 -*-
# filename: test_namespace_sync.py
# @Time    : 2025/09/30 23:42
# @Author  : A2C-SMCP
"""
中文：针对 `a2c_smcp/server/sync_namespace.py` 的同步命名空间集成测试。
English: Integration tests for SyncSMCPNamespace in `a2c_smcp/server/sync_namespace.py`.

说明：
- 仅在本测试包使用的 `_local_sync_server.py` 启动同步 Socket.IO 服务器。
- 使用 werkzeug 在独立进程中运行 WSGI 服务器，彻底解决 GIL 阻塞问题。
"""

import multiprocessing
import socket
import threading
import time
from collections.abc import Generator
from multiprocessing import synchronize
from typing import Any

import pytest
from socketio import Client, Namespace, SimpleClient
from werkzeug.serving import make_server

from a2c_smcp.smcp import (
    ENTER_OFFICE_NOTIFICATION,
    GET_TOOLS_EVENT,
    JOIN_OFFICE_EVENT,
    LEAVE_OFFICE_EVENT,
    LEAVE_OFFICE_NOTIFICATION,
    SMCP_NAMESPACE,
    TOOL_CALL_EVENT,
    UPDATE_CONFIG_EVENT,
)
from tests.integration_tests.server._local_sync_server import create_local_sync_server


@pytest.fixture
def sync_server_port() -> int:
    """
    中文：查找可用端口。
    English: Find an available TCP port.
    """
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _run_server_process(port: int, ready_event: synchronize.Event) -> None:
    """在独立进程中运行服务器"""
    try:
        sio, ns, wsgi_app = create_local_sync_server()
        # 禁用监控任务避免关闭时出错
        sio.eio.start_service_task = False

        server = make_server("localhost", port, wsgi_app, threaded=True)

        # 通知主进程服务器已准备好
        ready_event.set()

        # 运行服务器
        server.serve_forever()
    except Exception as e:
        print(f"服务器进程错误: {e}")
        ready_event.set()  # 即使出错也要设置事件，避免主进程无限等待


@pytest.fixture
def startup_and_shutdown_local_sync_server(sync_server_port: int) -> Generator[None, Any, None]:
    # 创建进程间通信事件
    ready_event = multiprocessing.Event()

    # 启动服务器进程
    server_process = multiprocessing.Process(
        target=_run_server_process,
        args=(sync_server_port, ready_event),
        daemon=True,
    )
    server_process.start()

    # 等待服务器准备好
    if not ready_event.wait(timeout=5):
        server_process.terminate()
        server_process.join(timeout=2)
        pytest.fail("服务器进程启动超时")

    try:
        yield
    finally:
        # 终止服务器进程
        if server_process.is_alive():
            server_process.terminate()
            server_process.join(timeout=3)

        # 如果进程仍然存活，强制杀死
        if server_process.is_alive():
            server_process.kill()
            server_process.join(timeout=1)


def _join_office(client: Client | SimpleClient, role: str, office_id: str, name: str) -> None:
    ok, err = (
        client.call(
            JOIN_OFFICE_EVENT,
            {"role": role, "office_id": office_id, "name": name},
            namespace=SMCP_NAMESPACE,
        )
        if isinstance(client, Client)
        else client.call(JOIN_OFFICE_EVENT, {"role": role, "office_id": office_id, "name": name})
    )
    if not (ok and err is None):
        print(f"加入房间失败: role={role}, office_id={office_id}, name={name}, ok={ok}, err={err}")
    assert ok and err is None


def test_enter_and_broadcast_sync(startup_and_shutdown_local_sync_server, sync_server_port: int) -> None:
    agent = Client()
    computer = Client()

    enter_events: list[dict] = []

    @agent.on(ENTER_OFFICE_NOTIFICATION, namespace=SMCP_NAMESPACE)
    def _on_enter(data: dict):  # noqa: ANN001
        enter_events.append(data)

    agent.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    office_id = "office-sync-s1"
    _join_office(agent, role="agent", office_id=office_id, name="robot-S1")

    computer.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    _join_office(computer, role="computer", office_id=office_id, name="comp-S1")

    time.sleep(0.2)
    assert enter_events, "Agent 应收到 ENTER_OFFICE_NOTIFICATION"

    agent.disconnect()
    computer.disconnect()


def test_leave_and_broadcast_sync(startup_and_shutdown_local_sync_server, sync_server_port: int) -> None:
    agent = Client()
    computer = Client()

    leave_events: list[dict] = []

    @agent.on(LEAVE_OFFICE_NOTIFICATION, namespace=SMCP_NAMESPACE)
    def _on_leave(data: dict):  # noqa: ANN001
        leave_events.append(data)

    agent.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    office_id = "office-sync-s2"
    _join_office(agent, role="agent", office_id=office_id, name="robot-S2")

    computer.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    _join_office(computer, role="computer", office_id=office_id, name="comp-S2")

    ok, err = computer.call(LEAVE_OFFICE_EVENT, {"office_id": office_id}, namespace=SMCP_NAMESPACE)
    assert ok and err is None

    time.sleep(0.2)
    assert leave_events, "Agent 应收到 LEAVE_OFFICE_NOTIFICATION"

    agent.disconnect()
    computer.disconnect()


def _run_computer_client_process(port: int, computer_name_queue: multiprocessing.Queue, error_queue: multiprocessing.Queue) -> None:
    """在独立进程中运行Computer客户端"""
    computer = Client()

    @computer.on(GET_TOOLS_EVENT, namespace=SMCP_NAMESPACE)
    def _on_get_tools(data: dict):  # noqa: ANN001
        return {
            "tools": [
                {
                    "name": "echo",
                    "description": "echo text",
                    "params_schema": {"type": "object"},
                    "return_schema": None,
                },
            ],
            "req_id": data["req_id"],
        }

    try:
        computer.connect(f"http://localhost:{port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
        office_id = "office-sync-s3"
        computer_name = "comp-S3"
        _join_office(computer, role="computer", office_id=office_id, name=computer_name)

        # 将computer_sid发送给主进程
        computer_name_queue.put(computer_name)

        # 等待并处理GET_TOOLS_EVENT
        computer.wait()
    except Exception as e:
        error_queue.put(f"Computer客户端错误: {str(e)}")
    finally:
        computer.disconnect()


def _run_agent_client_process(
    port: int,
    computer_name: str,
    result_queue: multiprocessing.Queue,
    error_queue: multiprocessing.Queue,
) -> None:
    """在独立进程中运行Agent客户端"""
    try:
        agent = Client()
        agent_id = "robot-S3"
        agent.connect(f"http://localhost:{port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
        office_id = "office-sync-s3"
        _join_office(agent, role="agent", office_id=office_id, name=agent_id)

        # 确保连接稳定后再进行调用
        time.sleep(0.2)

        # 执行GET_TOOLS调用
        res = agent.call(
            GET_TOOLS_EVENT,
            {"computer": computer_name, "robot_id": agent_id, "req_id": "req-sync-1"},
            namespace=SMCP_NAMESPACE,
            timeout=15,
        )

        # 将结果发送给主进程
        result_queue.put(res)

        agent.disconnect()
    except Exception as e:
        error_queue.put(f"Agent客户端错误: {str(e)}")


# @pytest.mark.skip
def test_get_tools_success_sync(startup_and_shutdown_local_sync_server: Namespace, sync_server_port: int) -> None:
    """测试同步环境下获取工具列表，使用多进程避免GIL阻塞"""

    # 创建进程间通信队列
    computer_name_queue = multiprocessing.Queue()
    result_queue = multiprocessing.Queue()
    error_queue = multiprocessing.Queue()

    # 1. 启动Computer客户端进程并获取computer_sid
    computer_process = multiprocessing.Process(
        target=_run_computer_client_process,
        args=(sync_server_port, computer_name_queue, error_queue),
        daemon=True,
    )
    computer_process.start()

    try:
        # 等待获取computer_sid
        try:
            computer_name = computer_name_queue.get(timeout=5)
        except Exception:
            # 检查是否有错误
            if not error_queue.empty():
                error_msg = error_queue.get()
                pytest.fail(f"Computer客户端启动失败: {error_msg}")
            else:
                pytest.fail("获取Computer SID超时")

        print(f"获取到Computer NAME: {computer_name}")

        # 2. 启动Agent客户端进程执行工具列表获取
        agent_process = multiprocessing.Process(
            target=_run_agent_client_process,
            args=(sync_server_port, computer_name, result_queue, error_queue),
            daemon=True,
        )
        agent_process.start()

        try:
            # 等待Agent执行结果
            try:
                result = result_queue.get(timeout=20)
                # 验证结果
                assert isinstance(result, dict), f"期望返回dict，实际返回: {type(result)}"
                assert result.get("tools") and result["tools"][0]["name"] == "echo"
            except Exception:
                # 检查是否有错误
                if not error_queue.empty():
                    error_msg = error_queue.get()
                    pytest.fail(f"Agent客户端执行失败: {error_msg}")
                else:
                    pytest.fail("Agent执行超时")
        finally:
            # 清理Agent进程
            if agent_process.is_alive():
                agent_process.terminate()
                agent_process.join(timeout=2)
    finally:
        # 清理Computer进程
        if computer_process.is_alive():
            computer_process.terminate()
            computer_process.join(timeout=2)


def test_update_config_broadcast_sync(startup_and_shutdown_local_sync_server, sync_server_port: int) -> None:
    agent = Client()
    computer = Client()

    received = {"count": 0}

    @agent.on("notify:update_config", namespace=SMCP_NAMESPACE)
    def _on_update(data: dict):  # noqa: ANN001
        received["count"] += 1

    agent.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    office_id = "office-sync-s4"
    _join_office(agent, role="agent", office_id=office_id, name="robot-S4")

    computer.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    _join_office(computer, role="computer", office_id=office_id, name="comp-S4")

    computer.call(UPDATE_CONFIG_EVENT, {"computer": computer.get_sid(namespace=SMCP_NAMESPACE)}, namespace=SMCP_NAMESPACE)

    time.sleep(0.2)
    assert received["count"] >= 1

    agent.disconnect()
    computer.disconnect()


def test_tool_call_forward_sync(startup_and_shutdown_local_sync_server, sync_server_port: int) -> None:
    """测试同步环境下工具调用转发，使用多线程避免阻塞"""
    agent = Client()
    computer = Client()

    received = {"count": 0, "data": None}
    call_result: dict = {"error": None}
    # 用于同步的事件
    computer_ready = threading.Event()
    call_completed = threading.Event()

    @computer.on(TOOL_CALL_EVENT, namespace=SMCP_NAMESPACE)
    def _on_tool_call(data: dict):  # noqa: ANN001
        received["count"] += 1
        received["data"] = data
        # 返回响应给 Agent
        return {"ok": True, "echo": data}

    def run_computer_client():
        """在独立线程中运行Computer客户端"""
        try:
            computer.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
            office_id = "office-sync-s5"
            _join_office(computer, role="computer", office_id=office_id, name="comp-S5")
            computer_ready.set()  # 通知Computer客户端已准备好

            # 等待调用完成
            call_completed.wait(timeout=20)
        except Exception as e:
            call_result["error"] = f"Computer客户端错误: {str(e)}"
            computer_ready.set()
        finally:
            try:
                computer.disconnect()
            except Exception:
                pass

    # 先连接Agent客户端
    agent.connect(f"http://localhost:{sync_server_port}", namespaces=[SMCP_NAMESPACE], socketio_path="/socket.io")
    office_id = "office-sync-s5"
    _join_office(agent, role="agent", office_id=office_id, name="robot-S5")

    # 启动Computer客户端线程
    computer_thread = threading.Thread(target=run_computer_client, daemon=True)
    computer_thread.start()

    try:
        # 等待Computer客户端准备好
        if not computer_ready.wait(timeout=10):
            pytest.fail("Computer客户端连接超时")

        if call_result["error"]:
            pytest.fail(call_result["error"])

        # 确保Computer客户端完全连接后再进行调用
        time.sleep(0.2)

        # 执行Agent工具调用
        res = agent.call(
            TOOL_CALL_EVENT,
            {
                "robot_id": "robot-S5",
                "computer": "comp-S5",
                "tool_name": "echo",
                "params": {"text": "hi"},
                "req_id": "req-sync-2",
                "timeout": 5,
            },
            namespace=SMCP_NAMESPACE,
            timeout=15,
        )

        # 同步命名空间现在使用 call 方法，等待 Computer 响应
        assert isinstance(res, dict), f"期望返回 dict，实际返回: {type(res)}"
        assert res.get("ok") is True, f"期望 ok=True，实际返回: {res}"
        assert res.get("echo") is not None, f"期望有 echo 字段，实际返回: {res}"

        # 验证 Computer 收到了工具调用
        assert received["count"] == 1, f"Computer应该收到1次工具调用事件，实际收到{received['count']}次"
        assert received["data"] is not None
        assert received["data"]["tool_name"] == "echo"
        assert received["data"]["params"]["text"] == "hi"

    finally:
        call_completed.set()
        computer_thread.join(timeout=5)
        agent.disconnect()


def test_computer_duplicate_name_rejected(startup_and_shutdown_local_sync_server, sync_server_port: int):
    """
    中文：测试Computer重名检查：当房间内已存在同名Computer时，第二个Computer加入应失败
    English: Test Computer duplicate name check: second Computer with same name should fail to join
    """
    computer1 = SimpleClient()
    computer2 = SimpleClient()

    try:
        # 连接第一个 Computer
        # Connect first Computer
        computer1.connect(f"http://localhost:{sync_server_port}", namespace=SMCP_NAMESPACE)
        office_id = "office-sync-dup-test"
        computer_name = "duplicate-comp-sync"

        # 第一个 Computer 成功加入
        # First Computer joins successfully
        ok, err = computer1.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": office_id, "name": computer_name},
        )
        assert ok and err is None, f"第一个Computer应该加入成功 / First Computer should join successfully, error: {err}"

        # 连接第二个 Computer（同名）
        # Connect second Computer (same name)
        computer2.connect(f"http://localhost:{sync_server_port}", namespace=SMCP_NAMESPACE)

        # 第二个 Computer 尝试加入同一房间，应该失败
        # Second Computer tries to join same room, should fail
        ok2, err2 = computer2.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": office_id, "name": computer_name},
        )

        # 验证失败
        # Verify failure
        assert not ok2, "第二个同名Computer应该加入失败 / Second Computer with same name should fail to join"
        assert err2 is not None, "应该返回错误信息 / Should return error message"
        assert "already exists" in err2, f"错误信息应包含'already exists'，实际: {err2} / Error should contain 'already exists'"

    finally:
        computer1.disconnect()
        computer2.disconnect()


def test_computer_different_name_allowed(startup_and_shutdown_local_sync_server, sync_server_port: int):
    """
    中文：测试不同名Computer可以加入：房间内已有Computer，但名字不同，应该成功
    English: Test different name Computer can join: room has Computer but different name, should succeed
    """
    computer1 = SimpleClient()
    computer2 = SimpleClient()

    try:
        # 连接第一个 Computer
        # Connect first Computer
        computer1.connect(f"http://localhost:{sync_server_port}", namespace=SMCP_NAMESPACE)
        office_id = "office-sync-diff-name-test"

        # 第一个 Computer 加入
        # First Computer joins
        ok, err = computer1.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": office_id, "name": "comp-sync-1"},
        )
        assert ok and err is None, f"第一个Computer应该加入成功 / First Computer should join successfully, error: {err}"

        # 连接第二个 Computer（不同名）
        # Connect second Computer (different name)
        computer2.connect(f"http://localhost:{sync_server_port}", namespace=SMCP_NAMESPACE)

        # 第二个 Computer 加入同一房间，应该成功
        # Second Computer joins same room, should succeed
        ok2, err2 = computer2.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": office_id, "name": "comp-sync-2"},
        )

        # 验证成功
        # Verify success
        assert ok2, f"不同名Computer应该加入成功 / Different name Computer should succeed, error: {err2}"
        assert err2 is None, "不应该有错误信息 / Should not have error message"

    finally:
        computer1.disconnect()
        computer2.disconnect()


def test_computer_switch_room_with_same_name_allowed(startup_and_shutdown_local_sync_server, sync_server_port: int):
    """
    中文：测试Computer切换房间：同一个Computer从一个房间切换到另一个房间应该成功
    English: Test Computer switching rooms: same Computer switching from one room to another should succeed
    """
    computer = SimpleClient()

    try:
        # 连接 Computer
        # Connect Computer
        computer.connect(f"http://localhost:{sync_server_port}", namespace=SMCP_NAMESPACE)
        computer_name = "switching-comp-sync"

        # 加入第一个房间
        # Join first room
        ok, err = computer.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": "office-sync-room-1", "name": computer_name},
        )
        assert ok and err is None, f"加入第一个房间应该成功 / Joining first room should succeed, error: {err}"

        # 切换到第二个房间（同名Computer）
        # Switch to second room (same name Computer)
        ok2, err2 = computer.call(
            JOIN_OFFICE_EVENT,
            {"role": "computer", "office_id": "office-sync-room-2", "name": computer_name},
        )

        # 验证成功
        # Verify success
        assert ok2, f"Computer切换房间应该成功 / Computer switching rooms should succeed, error: {err2}"
        assert err2 is None, "不应该有错误信息 / Should not have error message"

    finally:
        computer.disconnect()
