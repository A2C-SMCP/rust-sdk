# -*- coding: utf-8 -*-
# filename: test_rust_agent_integration.py
# @Time    : 2025/12/15 14:30
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: Rust Agent 与 Python Server/Computer 集成测试
English: Integration tests for Rust Agent with Python Server/Computer
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import pytest
import socketio

from a2c_smcp.smcp import (
    ENTER_OFFICE_NOTIFICATION,
    LEAVE_OFFICE_NOTIFICATION,
    SMCP_NAMESPACE,
)

pytestmark = pytest.mark.e2e


class RustAgentFixture:
    """中文: Rust Agent 进程管理器 / English: Rust Agent process manager"""
    
    def __init__(self, server_url: str, agent_id: str = "rust-test-agent", 
                 office_id: str = "test-office", api_key: str | None = None):
        self.server_url = server_url
        self.agent_id = agent_id
        self.office_id = office_id
        self.api_key = api_key
        self.process: subprocess.Popen | None = None
        
    def start(self, test_mode: str | None = None) -> None:
        """中文: 启动 Rust Agent 进程 / English: Start Rust Agent process"""
        # 构建 Rust Agent 二进制路径
        rust_sdk_root = Path(__file__).parent.parent.parent.parent
        agent_binary = rust_sdk_root / "target" / "debug" / "e2e_test_agent"
        
        if not agent_binary.exists():
            # 尝试构建二进制
            print("Building Rust Agent binary...")
            result = subprocess.run(
                ["cargo", "build", "--example", "e2e_test_agent"],
                cwd=rust_sdk_root,
                capture_output=True,
                text=True
            )
            if result.returncode != 0:
                raise RuntimeError(f"Failed to build Rust Agent: {result.stderr}")
        
        # 设置环境变量
        env = os.environ.copy()
        env["SMCP_SERVER_URL"] = self.server_url
        env["SMCP_AGENT_ID"] = self.agent_id
        env["SMCP_OFFICE_ID"] = self.office_id
        if self.api_key:
            env["SMCP_API_KEY"] = self.api_key
        if test_mode:
            env["SMCP_TEST_MODE"] = test_mode
            
        # 启动进程
        self.process = subprocess.Popen(
            [str(agent_binary)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True
        )
        
    def stop(self) -> None:
        """中文: 停止 Rust Agent 进程 / English: Stop Rust Agent process"""
        if self.process:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
            self.process = None
            
    def get_logs(self) -> tuple[str, str]:
        """中文: 获取进程日志 / English: Get process logs"""
        if not self.process:
            return "", ""
        stdout, stderr = self.process.communicate()
        return stdout, stderr


@pytest.fixture
def rust_agent(integration_server_endpoint: str):
    """中文: 提供 Rust Agent fixture / English: Provide Rust Agent fixture"""
    agent = RustAgentFixture(integration_server_endpoint)
    try:
        agent.start()
        # 等待 Agent 启动和连接
        time.sleep(2)
        yield agent
    finally:
        agent.stop()


def test_rust_agent_connection_and_events(rust_agent: RustAgentFixture):
    """
    中文: 测试 Rust Agent 连接和事件处理
    English: Test Rust Agent connection and event handling
    """
    # 创建一个客户端来监听事件
    client = socketio.Client()
    events_received = []
    
    @client.on(ENTER_OFFICE_NOTIFICATION)
    def on_enter_office(data):
        events_received.append(('enter', data))
        
    @client.on(LEAVE_OFFICE_NOTIFICATION)
    def on_leave_office(data):
        events_received.append(('leave', data))
    
    # 连接到服务器
    client.connect(integration_server_endpoint, namespaces=[SMCP_NAMESPACE])
    
    try:
        # 等待一段时间让 Rust Agent 完成连接和加入办公室
        time.sleep(3)
        
        # 验证收到了进入办公室事件
        enter_events = [e for e, _ in events_received if e == 'enter']
        assert len(enter_events) > 0, "Should receive enter office notification"
        
        # 验证事件数据
        _, enter_data = enter_events[0]
        assert enter_data['role'] == 'agent'
        assert enter_data['name'] == rust_agent.agent_id
        
    finally:
        client.disconnect()


def test_rust_agent_with_authentication():
    """
    中文: 测试 Rust Agent 认证功能
    English: Test Rust Agent authentication
    """
    # TODO: 实现带认证的测试
    pass


def test_rust_agent_tool_call_flow():
    """
    中文: 测试 Rust Agent 工具调用流程
    English: Test Rust Agent tool call flow
    """
    # TODO: 实现 Agent -> Server -> Computer 的工具调用测试
    pass


def test_rust_agent_ack_mechanism():
    """
    中文: 测试 Rust Agent ACK 机制（验证 vendor rust-socketio 的 ACK 支持）
    English: Test Rust Agent ACK mechanism (verify vendor rust-socketio ACK support)
    """
    # TODO: 专门测试 call() 方法的 ACK 响应
    pass


def test_rust_agent_error_handling():
    """
    中文: 测试 Rust Agent 错误处理
    English: Test Rust Agent error handling
    """
    # TODO: 测试各种错误场景
    pass


if __name__ == "__main__":
    # 允许直接运行此文件进行调试
    pytest.main([__file__, "-v"])
