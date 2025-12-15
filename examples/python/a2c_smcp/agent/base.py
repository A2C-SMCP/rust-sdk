"""
* 文件名: base
* 作者: JQQ
* 创建日期: 2025/9/30
* 最后修改日期: 2025/10/8
* 版权: 2023 JQQ. All rights reserved.
* 依赖: socketio, loguru
* 描述: Agent基础客户端抽象类（异步和同步版本）/ Agent base client abstract classes (async and sync versions)
"""

import uuid
from abc import ABC, abstractmethod
from typing import Any, cast

from mcp.types import CallToolResult, TextContent

from a2c_smcp.agent.auth import AgentAuthProvider
from a2c_smcp.agent.types import AgentEventHandler, AsyncAgentEventHandler
from a2c_smcp.smcp import (
    JOIN_OFFICE_EVENT,
    LEAVE_OFFICE_EVENT,
    EnterOfficeNotification,
    EnterOfficeReq,
    GetDeskTopReq,
    GetDeskTopRet,
    GetToolsReq,
    GetToolsRet,
    LeaveOfficeNotification,
    LeaveOfficeReq,
    ToolCallReq,
    UpdateMCPConfigNotification,
)
from a2c_smcp.utils.logger import logger


class BaseAgentClient(ABC):
    """
    Agent异步基础客户端抽象类，提供通用的SMCP协议处理逻辑（异步版本）
    Agent async base client abstract class, provides common SMCP protocol handling logic (async version)
    """

    def __init__(
        self,
        auth_provider: AgentAuthProvider,
        event_handler: AsyncAgentEventHandler | None = None,
    ) -> None:
        """
        初始化异步基础Agent客户端
        Initialize async base Agent client

        Args:
            auth_provider (AgentAuthProvider): 认证提供者 / Authentication provider
            event_handler (AsyncAgentEventHandler | None): 异步事件处理器 / Async event handler
        """
        self.auth_provider = auth_provider
        self.event_handler = event_handler

    @abstractmethod
    async def emit(self, event: str, data: Any = None, namespace: str | None = None, callback: Any = None) -> None:
        """
        异步发送事件的抽象方法
        Abstract method for async sending events
        """
        pass

    @abstractmethod
    async def call(self, event: str, data: Any = None, namespace: str | None = None, timeout: int = 60) -> Any:
        """
        异步调用事件并等待响应的抽象方法
        Abstract method for async calling events and waiting for response
        """
        pass

    def validate_emit_event(self, event: str) -> None:
        """
        验证发送事件的合法性
        Validate the legality of emitted events

        Args:
            event (str): 事件名称 / Event name

        Raises:
            ValueError: 当事件不合法时 / When event is invalid
        """
        if event.startswith("notify:"):
            raise ValueError("AgentClient不允许使用notify:*事件 / AgentClient is not allowed to use notify:* events")
        if event.startswith("agent:"):
            raise ValueError("AgentClient不允许发起agent:*事件 / AgentClient is not allowed to initiate agent:* events")

    def create_tool_call_request(self, computer: str, tool_name: str, params: dict, timeout: int) -> ToolCallReq:
        """
        创建工具调用请求对象
        Create tool call request object

        Args:
            computer (str): 目标计算机ID / Target computer ID
            tool_name (str): 工具名称 / Tool name
            params (dict): 调用参数 / Call parameters
            timeout (int): 超时时间 / Timeout duration

        Returns:
            ToolCallReq: 工具调用请求 / Tool call request
        """
        agent_config = self.auth_provider.get_agent_config()
        return ToolCallReq(
            computer=computer,
            tool_name=tool_name,
            params=params,
            agent=agent_config["agent"],
            req_id=uuid.uuid4().hex,
            timeout=timeout,
        )

    def create_get_tools_request(self, computer: str) -> GetToolsReq:
        """
        创建获取工具请求对象
        Create get tools request object

        Args:
            computer (str): 目标计算机ID / Target computer ID

        Returns:
            GetToolsReq: 获取工具请求 / Get tools request
        """
        agent_config = self.auth_provider.get_agent_config()
        return GetToolsReq(
            computer=computer,
            agent=agent_config["agent"],
            req_id=uuid.uuid4().hex,
        )

    def create_get_desktop_request(self, computer: str, *, size: int | None = None, window: str | None = None) -> GetDeskTopReq:
        """
        创建获取桌面请求对象
        Create get desktop request object

        Args:
            computer (str): 目标计算机ID / Target computer ID
            size (int | None): 桌面窗口数量上限 / Max number of windows
            window (str | None): 指定窗口URI / Specific window URI

        Returns:
            GetDeskTopReq: 获取桌面请求 / Get desktop request
        """
        agent_config = self.auth_provider.get_agent_config()
        req: GetDeskTopReq = {
            "computer": computer,
            "agent": agent_config["agent"],
            "req_id": uuid.uuid4().hex,
        }
        if size is not None:
            req["desktop_size"] = size
        if window is not None:
            req["window"] = window
        return req

    def process_desktop_response(self, response: GetDeskTopRet, computer: str) -> None:
        """
        处理桌面响应（默认仅记录日志；留作后续扩展回调）。
        Process desktop response (log only by default; placeholder for future callbacks).
        """
        try:
            desktops = response.get("desktops", []) if isinstance(response, dict) else []
            logger.info(f"Received desktop from computer {computer}, windows={len(desktops)}")
        except Exception as e:
            logger.error(f"Error processing desktop response: {e}")

    def handle_tool_call_timeout(self, req_id: str) -> CallToolResult:
        """
        处理工具调用超时情况
        Handle tool call timeout situation

        Args:
            req_id (str): 请求ID / Request ID

        Returns:
            CallToolResult: 超时错误结果 / Timeout error result
        """
        return CallToolResult(
            content=[TextContent(text=f"工具调用超时 / Tool call timeout, req_id={req_id}", type="text")],
            isError=True,
        )

    def validate_office_data(self, data: EnterOfficeNotification | LeaveOfficeNotification) -> str:
        """
        验证办公室数据并返回计算机ID
        Validate office data and return computer ID

        Args:
            data: 办公室通知数据 / Office notification data

        Returns:
            str: 计算机ID / Computer ID

        Raises:
            AssertionError: 当数据无效时 / When data is invalid
        """
        agent_config = self.auth_provider.get_agent_config()
        assert data["office_id"] == agent_config["office_id"], "无效的办公室ID / Invalid office ID"
        assert data.get("computer"), "无效的计算机ID / Invalid computer ID"
        return cast(str, data["computer"])

    async def handle_computer_enter_office(self, data: EnterOfficeNotification) -> None:
        """
        异步处理Computer加入办公室事件
        Async handle Computer enter office event

        Args:
            data: 加入办公室通知数据 / Enter office notification data
        """
        try:
            computer = self.validate_office_data(data)
            logger.info(f"Computer {computer} entered office {data['office_id']}")

            # 调用异步事件处理器（强制携带 client 引用）
            # Call async event handler (force passing client reference)
            if self.event_handler:
                await self.event_handler.on_computer_enter_office(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer enter office: {e}")

    async def handle_computer_leave_office(self, data: LeaveOfficeNotification) -> None:
        """
        异步处理Computer离开办公室事件
        Async handle Computer leave office event

        Args:
            data: 离开办公室通知数据 / Leave office notification data
        """
        try:
            computer = self.validate_office_data(data)
            logger.info(f"Computer {computer} left office {data['office_id']}")

            # 调用异步事件处理器（强制携带 client 引用）
            # Call async event handler (force passing client reference)
            if self.event_handler:
                await self.event_handler.on_computer_leave_office(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer leave office: {e}")

    async def handle_computer_update_config(self, data: UpdateMCPConfigNotification) -> None:
        """
        异步处理Computer更新配置事件
        Async handle Computer update config event

        Args:
            data: 配置更新通知数据 / Config update notification data
        """
        try:
            computer = data["computer"]
            logger.info(f"Computer {computer} updated config")

            # 调用异步事件处理器（强制携带 client 引用）
            # Call async event handler (force passing client reference)
            if self.event_handler:
                await self.event_handler.on_computer_update_config(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer update config: {e}")

    async def process_tools_response(self, response: GetToolsRet, computer: str) -> None:
        """
        异步处理工具响应
        Async process tools response

        Args:
            response: 工具响应数据 / Tools response data
            computer: 计算机ID / Computer ID
        """
        try:
            if tools := response.get("tools"):
                logger.info(f"Received {len(tools)} tools from computer {computer}")

                # 调用异步事件处理器（强制携带 client 引用）
                # Call async event handler (force passing client reference)
                if self.event_handler:
                    await self.event_handler.on_tools_received(computer, tools, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error processing tools response: {e}")

    async def join_office(self, office_id: str, agent_name: str, namespace: str | None = None) -> None:
        """
        加入一个Office（Socket.IO中的Room）
        Join an Office (Room in Socket.IO)

        Args:
            office_id (str): 房间ID，在A2C-smcp协议中，OfficeID即为Socket.IO RoomID
                            / Room ID, in A2C-smcp protocol, OfficeID is the Socket.IO RoomID
            agent_name (str): Agent名称，提供给前端展示用
                            / Agent name, for frontend display
            namespace (str | None): 命名空间 / Namespace
        """
        await self.emit(
            JOIN_OFFICE_EVENT,
            EnterOfficeReq(office_id=office_id, role="agent", name=agent_name),
            namespace=namespace,
        )

    async def leave_office(self, office_id: str, namespace: str | None = None) -> None:
        """
        离开一个Office（Socket.IO中的Room）
        Leave an Office (Room in Socket.IO)

        Args:
            office_id (str): 房间ID / Room ID
            namespace (str | None): 命名空间 / Namespace
        """
        await self.emit(LEAVE_OFFICE_EVENT, LeaveOfficeReq(office_id=office_id), namespace=namespace)

    @abstractmethod
    def register_event_handlers(self) -> None:
        """
        注册事件处理器，子类需要实现具体的注册逻辑
        Register event handlers, subclasses need to implement specific registration logic
        """
        # 这个方法需要在子类中实现具体的事件注册逻辑
        # This method needs to implement specific event registration logic in subclasses
        pass


class BaseAgentSyncClient(ABC):
    """
    Agent同步基础客户端抽象类，提供通用的SMCP协议处理逻辑（同步版本）
    Agent sync base client abstract class, provides common SMCP protocol handling logic (sync version)
    """

    def __init__(
        self,
        auth_provider: AgentAuthProvider,
        event_handler: AgentEventHandler | None = None,
    ) -> None:
        """
        初始化同步基础Agent客户端
        Initialize sync base Agent client

        Args:
            auth_provider (AgentAuthProvider): 认证提供者 / Authentication provider
            event_handler (AgentEventHandler | None): 同步事件处理器 / Sync event handler
        """
        self.auth_provider = auth_provider
        self.event_handler = event_handler

    @abstractmethod
    def emit(self, event: str, data: Any = None, namespace: str | None = None, callback: Any = None) -> None:
        """
        发送事件的抽象方法
        Abstract method for sending events
        """
        pass

    @abstractmethod
    def call(self, event: str, data: Any = None, namespace: str | None = None, timeout: int = 60) -> Any:
        """
        调用事件并等待响应的抽象方法
        Abstract method for calling events and waiting for response
        """
        pass

    def validate_emit_event(self, event: str) -> None:
        """
        验证发送事件的合法性
        Validate the legality of emitted events

        Args:
            event (str): 事件名称 / Event name

        Raises:
            ValueError: 当事件不合法时 / When event is invalid
        """
        if event.startswith("notify:"):
            raise ValueError("AgentClient不允许使用notify:*事件 / AgentClient is not allowed to use notify:* events")
        if event.startswith("agent:"):
            raise ValueError("AgentClient不允许发起agent:*事件 / AgentClient is not allowed to initiate agent:* events")

    def create_tool_call_request(self, computer: str, tool_name: str, params: dict, timeout: int) -> ToolCallReq:
        """
        创建工具调用请求对象
        Create tool call request object

        Args:
            computer (str): 目标计算机ID / Target computer ID
            tool_name (str): 工具名称 / Tool name
            params (dict): 调用参数 / Call parameters
            timeout (int): 超时时间 / Timeout duration

        Returns:
            ToolCallReq: 工具调用请求 / Tool call request
        """
        agent_config = self.auth_provider.get_agent_config()
        return ToolCallReq(
            computer=computer,
            tool_name=tool_name,
            params=params,
            agent=agent_config["agent"],
            req_id=uuid.uuid4().hex,
            timeout=timeout,
        )

    def create_get_tools_request(self, computer: str) -> GetToolsReq:
        """
        创建获取工具请求对象
        Create get tools request object

        Args:
            computer (str): 目标计算机ID / Target computer ID

        Returns:
            GetToolsReq: 获取工具请求 / Get tools request
        """
        agent_config = self.auth_provider.get_agent_config()
        return GetToolsReq(
            computer=computer,
            agent=agent_config["agent"],
            req_id=uuid.uuid4().hex,
        )

    def create_get_desktop_request(self, computer: str, *, size: int | None = None, window: str | None = None) -> GetDeskTopReq:
        """
        创建获取桌面请求对象
        Create get desktop request object

        Args:
            computer (str): 目标计算机ID / Target computer ID
            size (int | None): 桌面窗口数量上限 / Max number of windows
            window (str | None): 指定窗口URI / Specific window URI

        Returns:
            GetDeskTopReq: 获取桌面请求 / Get desktop request
        """
        agent_config = self.auth_provider.get_agent_config()
        req: GetDeskTopReq = {
            "computer": computer,
            "agent": agent_config["agent"],
            "req_id": uuid.uuid4().hex,
        }
        if size is not None:
            req["desktop_size"] = size
        if window is not None:
            req["window"] = window
        return req

    def process_desktop_response(self, response: GetDeskTopRet, computer: str) -> None:
        """
        处理桌面响应（默认仅记录日志；留作后续扩展回调）。
        Process desktop response (log only by default; placeholder for future callbacks).
        """
        try:
            desktops = response.get("desktops", []) if isinstance(response, dict) else []
            logger.info(f"Received desktop from computer {computer}, windows={len(desktops)}")
        except Exception as e:
            logger.error(f"Error processing desktop response: {e}")

    def handle_tool_call_timeout(self, req_id: str) -> CallToolResult:
        """
        处理工具调用超时情况
        Handle tool call timeout situation

        Args:
            req_id (str): 请求ID / Request ID

        Returns:
            CallToolResult: 超时错误结果 / Timeout error result
        """
        return CallToolResult(
            content=[TextContent(text=f"工具调用超时 / Tool call timeout, req_id={req_id}", type="text")],
            isError=True,
        )

    def validate_office_data(self, data: EnterOfficeNotification | LeaveOfficeNotification) -> str:
        """
        验证办公室数据并返回计算机ID
        Validate office data and return computer ID

        Args:
            data: 办公室通知数据 / Office notification data

        Returns:
            str: 计算机ID / Computer ID

        Raises:
            AssertionError: 当数据无效时 / When data is invalid
        """
        agent_config = self.auth_provider.get_agent_config()
        assert data["office_id"] == agent_config["office_id"], "无效的办公室ID / Invalid office ID"
        assert data.get("computer"), "无效的计算机ID / Invalid computer ID"
        return cast(str, data["computer"])

    def handle_computer_enter_office(self, data: EnterOfficeNotification) -> None:
        """
        处理Computer加入办公室事件
        Handle Computer enter office event

        Args:
            data: 加入办公室通知数据 / Enter office notification data
        """
        try:
            computer = self.validate_office_data(data)
            logger.info(f"Computer {computer} entered office {data['office_id']}")

            # 调用事件处理器（强制携带 client 引用）
            # Call event handler (force passing client reference)
            if self.event_handler:
                self.event_handler.on_computer_enter_office(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer enter office: {e}")

    def handle_computer_leave_office(self, data: LeaveOfficeNotification) -> None:
        """
        处理Computer离开办公室事件
        Handle Computer leave office event

        Args:
            data: 离开办公室通知数据 / Leave office notification data
        """
        try:
            computer = self.validate_office_data(data)
            logger.info(f"Computer {computer} left office {data['office_id']}")

            # 调用事件处理器（强制携带 client 引用）
            # Call event handler (force passing client reference)
            if self.event_handler:
                self.event_handler.on_computer_leave_office(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer leave office: {e}")

    def handle_computer_update_config(self, data: UpdateMCPConfigNotification) -> None:
        """
        处理Computer更新配置事件
        Handle Computer update config event

        Args:
            data: 配置更新通知数据 / Config update notification data
        """
        try:
            computer = data["computer"]
            logger.info(f"Computer {computer} updated config")

            # 调用事件处理器（强制携带 client 引用）
            # Call event handler (force passing client reference)
            if self.event_handler:
                self.event_handler.on_computer_update_config(data, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error handling computer update config: {e}")

    def process_tools_response(self, response: GetToolsRet, computer: str) -> None:
        """
        处理工具响应
        Process tools response

        Args:
            response: 工具响应数据 / Tools response data
            computer: 计算机ID / Computer ID
        """
        try:
            if tools := response.get("tools"):
                logger.info(f"Received {len(tools)} tools from computer {computer}")

                # 调用事件处理器（强制携带 client 引用）
                # Call event handler (force passing client reference)
                if self.event_handler:
                    self.event_handler.on_tools_received(computer, tools, self)  # type: ignore[arg-type]

        except Exception as e:
            logger.error(f"Error processing tools response: {e}")

    def join_office(self, office_id: str, agent_name: str, namespace: str | None = None) -> None:
        """
        加入一个Office（Socket.IO中的Room）
        Join an Office (Room in Socket.IO)

        Args:
            office_id (str): 房间ID，在A2C-smcp协议中，OfficeID即为Socket.IO RoomID
                            / Room ID, in A2C-smcp protocol, OfficeID is the Socket.IO RoomID
            agent_name (str): Agent名称，提供给前端展示用
                            / Agent name, for frontend display
            namespace (str | None): 命名空间 / Namespace
        """
        self.emit(
            JOIN_OFFICE_EVENT,
            EnterOfficeReq(office_id=office_id, role="agent", name=agent_name),
            namespace=namespace,
        )

    def leave_office(self, office_id: str, namespace: str | None = None) -> None:
        """
        离开一个Office（Socket.IO中的Room）
        Leave an Office (Room in Socket.IO)

        Args:
            office_id (str): 房间ID / Room ID
            namespace (str | None): 命名空间 / Namespace
        """
        self.emit(LEAVE_OFFICE_EVENT, LeaveOfficeReq(office_id=office_id), namespace=namespace)

    @abstractmethod
    def register_event_handlers(self) -> None:
        """
        注册事件处理器，子类需要实现具体的注册逻辑
        Register event handlers, subclasses need to implement specific registration logic
        """
        # 这个方法需要在子类中实现具体的事件注册逻辑
        # This method needs to implement specific event registration logic in subclasses
        pass
