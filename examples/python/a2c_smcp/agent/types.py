"""
* 文件名: types
* 作者: JQQ
* 创建日期: 2025/9/30
* 最后修改日期: 2025/10/8
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Agent端类型定义 / Agent-side type definitions
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING, Protocol, TypeAlias

from mcp.types import CallToolResult
from typing_extensions import TypedDict

from a2c_smcp.smcp import EnterOfficeNotification, LeaveOfficeNotification, SMCPTool, UpdateMCPConfigNotification

# 为避免运行时循环依赖，仅在类型检查时导入具体Client类型
# To avoid runtime circular imports, import concrete Client types only during type checking
if TYPE_CHECKING:  # pragma: no cover - type-only imports
    from a2c_smcp.agent.client import AsyncSMCPAgentClient
    from a2c_smcp.agent.sync_client import SMCPAgentClient

# 类型别名定义 / Type aliases
AgentID: TypeAlias = str
ComputerID: TypeAlias = str
RequestID: TypeAlias = str


class AgentConfig(TypedDict):
    """
    Agent配置信息
    Agent configuration information
    """

    agent: str  # Agent唯一标识 / Agent unique identifier
    office_id: str  # 办公室ID / Office ID


class ToolCallContext(TypedDict):
    """
    工具调用上下文信息
    Tool call context information
    """

    computer: str  # 目标计算机ID / Target computer ID
    tool_name: str  # 工具名称 / Tool name
    params: dict  # 调用参数 / Call parameters
    timeout: int  # 超时时间 / Timeout duration


class AgentEventHandler(Protocol):
    """
    Agent事件处理器协议，定义Agent需要处理的事件回调
    Agent event handler protocol, defines event callbacks that Agent needs to handle
    """

    def on_computer_enter_office(self, data: EnterOfficeNotification, client: SMCPAgentClient) -> None:
        """
        Computer加入办公室时的处理逻辑
        Handling logic when Computer joins office

        Args:
            data: 加入办公室的通知数据 / Office join notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    def on_computer_leave_office(self, data: LeaveOfficeNotification, client: SMCPAgentClient) -> None:
        """
        Computer离开办公室时的处理逻辑
        Handling logic when Computer leaves office

        Args:
            data: 离开办公室的通知数据 / Office leave notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    def on_computer_update_config(self, data: UpdateMCPConfigNotification, client: SMCPAgentClient) -> None:
        """
        Computer更新配置时的处理逻辑
        Handling logic when Computer updates configuration

        Args:
            data: 配置更新通知数据 / Configuration update notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    def on_tools_received(self, computer: str, tools: list[SMCPTool], client: SMCPAgentClient) -> None:
        """
        接收到工具列表时的处理逻辑
        Handling logic when tools list is received

        Args:
            computer: 计算机ID / Computer ID
            tools: 工具列表 / Tools list
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...


class AsyncAgentEventHandler(Protocol):
    """
    异步Agent事件处理器协议
    Async Agent event handler protocol
    """

    async def on_computer_enter_office(self, data: EnterOfficeNotification, client: AsyncSMCPAgentClient) -> None:
        """
        Computer加入办公室时的异步处理逻辑
        Async handling logic when Computer joins office

        Args:
            data: 加入办公室的通知数据 / Office join notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    async def on_computer_leave_office(self, data: LeaveOfficeNotification, client: AsyncSMCPAgentClient) -> None:
        """
        Computer离开办公室时的异步处理逻辑
        Async handling logic when Computer leaves office

        Args:
            data: 离开办公室的通知数据 / Office leave notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    async def on_computer_update_config(self, data: UpdateMCPConfigNotification, client: AsyncSMCPAgentClient) -> None:
        """
        Computer更新配置时的异步处理逻辑
        Async handling logic when Computer updates configuration

        Args:
            data: 配置更新通知数据 / Configuration update notification data
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...

    async def on_tools_received(self, computer: str, tools: list[SMCPTool], client: AsyncSMCPAgentClient) -> None:
        """
        接收到工具列表时的异步处理逻辑
        Async handling logic when tools list is received

        Args:
            computer: 计算机ID / Computer ID
            tools: 工具列表 / Tools list
            client: 调用发生时的Socket.IO Client / Socket.IO Client where make the call
        """
        ...


# 工具调用回调函数类型 / Tool call callback function types
ToolCallCallback = Callable[[str, str, dict, int], CallToolResult]
AsyncToolCallCallback = Callable[[str, str, dict, int], CallToolResult]

# Agent ID获取函数类型 / Agent ID getter function types
AgentIDGetter = Callable[[], str]
AsyncAgentIDGetter = Callable[[], str]
