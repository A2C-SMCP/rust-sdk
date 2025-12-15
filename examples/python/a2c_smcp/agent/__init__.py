# -*- coding: utf-8 -*-
# filename: __init__.py
# @Time    : 2025/9/30 13:56
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

"""
A2C-SMCP Agent模块
A2C-SMCP Agent module

提供Agent端的SMCP协议客户端实现，包括同步和异步两种模式
Provides Agent-side SMCP protocol client implementation, including both synchronous and asynchronous modes
"""

from a2c_smcp.agent.auth import AgentAuthProvider, DefaultAgentAuthProvider
from a2c_smcp.agent.base import BaseAgentClient
from a2c_smcp.agent.client import AsyncSMCPAgentClient
from a2c_smcp.agent.sync_client import SMCPAgentClient
from a2c_smcp.agent.types import (
    AgentConfig,
    AgentEventHandler,
    AgentID,
    AgentIDGetter,
    AsyncAgentEventHandler,
    AsyncAgentIDGetter,
    AsyncToolCallCallback,
    ComputerID,
    RequestID,
    ToolCallCallback,
    ToolCallContext,
)

__all__ = [
    # 认证相关 / Authentication related
    "AgentAuthProvider",
    "DefaultAgentAuthProvider",
    # 客户端实现 / Client implementations
    "BaseAgentClient",
    "SMCPAgentClient",
    "AsyncSMCPAgentClient",
    # 类型定义 / Type definitions
    "AgentConfig",
    "AgentEventHandler",
    "AsyncAgentEventHandler",
    "AgentID",
    "ComputerID",
    "RequestID",
    "ToolCallContext",
    "ToolCallCallback",
    "AsyncToolCallCallback",
    "AgentIDGetter",
    "AsyncAgentIDGetter",
]
