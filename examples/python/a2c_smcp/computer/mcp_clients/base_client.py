# -*- coding: utf-8 -*-
# filename: base_client.py
# @Time    : 2025/8/18 10:57
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
import asyncio
from abc import ABC, abstractmethod
from collections.abc import Awaitable, Callable
from contextlib import AsyncExitStack
from enum import StrEnum
from typing import Generic, TypeVar, cast

from mcp import ClientSession, Tool
from mcp.client.session import MessageHandlerFnT
from mcp.types import AnyUrl, CallToolResult, InitializeResult, ReadResourceResult, Resource, TextResourceContents
from pydantic import BaseModel
from transitions.core import EventData
from transitions.extensions import AsyncMachine

from a2c_smcp.utils import WindowURI, is_window_uri
from a2c_smcp.utils.async_property import async_property
from a2c_smcp.utils.logger import logger

# 泛型参数，用于约束 MCP Server 参数类型
# Generic parameter for constraining MCP Server parameter types
ParamsT = TypeVar("ParamsT", bound=BaseModel)


class STATES(StrEnum):
    initialized = "initialized"
    connected = "connected"
    disconnected = "disconnected"
    error = "error"


TRANSITIONS = [
    {
        "trigger": "aconnect",
        "source": STATES.initialized,
        "dest": STATES.connected,
        "prepare": "aprepare_connect",
        "conditions": "acan_connect",
        "before": "abefore_connect",
        "after": "aafter_connect",
    },
    {
        "trigger": "adisconnect",
        "source": STATES.connected,
        "dest": STATES.disconnected,
        "prepare": "aprepare_disconnect",
        "conditions": "acan_disconnect",
        "before": "abefore_disconnect",
        "after": "aafter_disconnect",
    },
    {
        "trigger": "aerror",
        "source": "*",
        "dest": STATES.error,
        "prepare": "aprepare_error",
        "conditions": "acan_error",
        "before": "abefore_error",
        "after": "aafter_error",
    },
    {
        "trigger": "ainitialize",
        "source": "*",
        "dest": STATES.initialized,
        "prepare": "aprepare_initialize",
        "conditions": "acan_initialize",
        "before": "abefore_initialize",
        "after": "aafter_initialize",
    },
]


class A2CAsyncMachine(AsyncMachine):
    @staticmethod
    async def await_all(callables: list[Callable]) -> list:
        """
        Executes callables without parameters in parallel and collects their results.

        A2C协议中，需要在状态机的状态变化函数之间管理异步上下文，但由于原生实现 await_all 方法使用 asyncio.gather会导致上下文打开与关闭处于
            不同的async task中进而导致关闭异常。因此重写此实现，将await_all方法变为同步执行。以此实现上下文打开与关闭处于同一个async task中

        Args:
            callables (list): A list of callable functions

        Returns:
            list: A list of results. Using asyncio the list will be in the same order as the passed callables.
        """
        ret = []
        for c in callables:
            ret.append(await c())
        return ret


class BaseMCPClient(ABC, Generic[ParamsT]):
    def __init__(
        self,
        params: ParamsT,
        state_change_callback: Callable[[str, str], None | Awaitable[None]] | None = None,
        message_handler: MessageHandlerFnT | None = None,
    ) -> None:
        """
        基类初始化

        Attributes:
            params (ParamsT): MCP Server启动参数
            state_change_callback (Callable[[str, str], None | Awaitable[None]]): 状态变化回调，兼容同步与异步
            message_handler (Callable[..., Awaitable[None]] | None):
                自定义消息处理回调，符合 MCP ClientSession 的 message_handler 要求；若提供，则在构建 ClientSession 时传入。
                Custom message handler callback compatible with MCP ClientSession's message_handler; if provided,
                    it will be passed when creating the ClientSession.
        """
        self.params: ParamsT = params
        self._state_change_callback = state_change_callback
        # 私有属性：用于处理 ServerNotification（如 listChanged）的通用回调；在创建 ClientSession 时传入
        # Private attribute: general callback to handle ServerNotification (e.g., listChanged);
        # forwarded to ClientSession on creation
        self._message_handler = message_handler
        self._aexit_stack = AsyncExitStack()
        self._async_session: ClientSession | None = None
        self._session_keep_alive_task: asyncio.Task | None = None
        self._create_session_success_event = asyncio.Event()
        self._create_session_failure_event = asyncio.Event()
        self._async_session_closed_event = asyncio.Event()
        # 私有属性：初始化结果（用于后续能力/元信息使用）；断开连接时需清理
        # Private attribute: InitializeResult cached for later capabilities/meta usage; must be cleared on disconnect
        self._initialize_result: InitializeResult | None = None

        # 初始化异步状态机
        self.machine = A2CAsyncMachine(
            model=self,
            states=STATES,
            transitions=TRANSITIONS,
            initial=STATES.initialized,
            send_event=True,  # 传递事件对象给回调
            auto_transitions=False,  # 禁用自动生成的状态转移
            ignore_invalid_triggers=False,  # 忽略无效触发器
        )

    async def _trigger_state_change(self, event: EventData) -> None:
        """
        触发状态变化回调，兼容同步与异步

        Args:
            event (EventData): 事件对象
        """
        if not self._state_change_callback:
            return

        callback_result = self._state_change_callback(event.transition.source, event.transition.dest)
        # 处理异步回调
        if isinstance(callback_result, Awaitable):
            await callback_result

    @async_property
    async def async_session(self) -> ClientSession:
        """
        异步会话对象

        Returns:
            ClientSession: MCP 官方异步会话，用于触发 MCP Server 指令
        """
        if self._async_session is None:
            await self.aconnect()
        return cast(ClientSession, self._async_session)

    @property
    def initialize_result(self) -> InitializeResult | None:
        """
        初始化结果只读访问（可能为None，表示未初始化或已清理）
        Read-only access for InitializeResult (may be None if not initialized or already cleaned)
        """
        return self._initialize_result

    @abstractmethod
    async def _create_async_session(self) -> ClientSession:
        """
        创建异步会话对象。一般在此方法内对需要保持的上下文压栈管理

        Returns:
            ClientSession: MCP 官方异步会话，用于触发 MCP Server 指令
        """
        raise NotImplementedError

    async def _keep_alive_task(self) -> None:
        """
        async_session 保活，进而保证其它连接可以正常使用它。

        在MCP源码设计中，xxx_client与ClientSession均使用了anyio的task_group来管理子任务。但这带来一个维护问题，在Manager中需要管理多个Client，如果
          Client的AsyncSession是基于anyio.task_group打开，那么在关闭时，必须严格按照打开顺序关闭，否则会导致anyio报错。基于这个anyio特性，因为我需要让
          ClientSession在一个独立的Asyncio Task中运行，如此可以保证这个上下文的打开关联在这个内部Task中，从而可以实现自由关闭。在Manager中可
          以独立启停Client

        在这个实现中主要完成以下几个工作：

        1. 完成 self._async_session的创建
        2. 将需要持续保证的上下文压栈 self._aexit_stack
        3. 通过 asyncio.Event().wait() 来保证上下文的持续，同时通过响应 self._session_keep_alive_task.done() 来完成上下文的关闭
        4. 得到关闭信号后，对 self._aexit_stack 里的上下文进行关闭
        """
        logger.debug(f"Session keep-alive task: {asyncio.current_task().get_name()}")
        try:
            # 创建异步会话，同时完成上下文的压栈
            self._async_session = await self._create_async_session()
            # 通知创建成功
            self._create_session_success_event.set()
            # 持续等待关闭信息
            try:
                # 等待任务的cancel
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                # 任务被取消，完成上下文
                logger.debug(f"Session keep-alive task cancelled: {asyncio.current_task().get_name()}")
        except Exception as e:
            logger.error(f"Session keep-alive task error: {asyncio.current_task().get_name()}: {e}")
            self._create_session_failure_event.set()
            await self.aerror()

        finally:
            # 关闭上下文
            await self._aexit_stack.aclose()
            # 清理session
            self._async_session = None
            # 清理初始化结果，确保会话真正关闭时协议初始化态一并清理
            # Cleanup InitializeResult to align with actual session teardown
            self._initialize_result = None
            self._async_session_closed_event.set()

    # region 状态转换回调函数基类实现
    async def aprepare_connect(self, event: EventData) -> None:
        """连接准备操作（可重写）"""
        logger.debug(f"Preparing connection with event: {event}\n\nserver params: {self.params}")

    async def acan_connect(self, event: EventData) -> bool:
        """连接条件检查（可重写）"""
        logger.debug(f"Checking connection conditions with event: {event}\n\nserver params: {self.params}")
        return True

    async def abefore_connect(self, event: EventData) -> None:
        """连接前操作（可重写）"""
        logger.debug(f"Before connection actions with event: {event}\n\nserver params: {self.params}")

    async def on_enter_connected(self, event: EventData) -> None:
        """进入已连接状态（可重写）"""
        logger.debug(f"Entering connected state with event: {event}\n\nserver params: {self.params}")
        self._session_keep_alive_task = asyncio.create_task(self._keep_alive_task())
        # 等待会话创建成功
        await self._create_session_success_event.wait()
        # 初始化client_session
        # 存储初始化返回结果，供后续使用
        # Store InitializeResult for later use
        self._initialize_result = await (await self.async_session).initialize()

    async def aafter_connect(self, event: EventData) -> None:
        """连接后操作（可重写）"""
        logger.debug(f"After connection actions with event: {event}\n\nserver params: {self.params}")
        await self._trigger_state_change(event)

    async def aprepare_disconnect(self, event: EventData) -> None:
        """断开准备操作（可重写）"""
        logger.debug(f"Preparing disconnection with event: {event}\n\nserver params: {self.params}")

    async def acan_disconnect(self, event: EventData) -> bool:
        """断开条件检查（可重写）"""
        logger.debug(f"Checking disconnection conditions with event: {event}\n\nserver params: {self.params}")
        return (await self.async_session) is not None

    async def abefore_disconnect(self, event: EventData) -> None:
        """断开前操作（可重写）"""
        logger.debug(f"Before disconnection actions with event: {event}\n\nserver params: {self.params}")

    async def on_enter_disconnected(self, event: EventData) -> None:
        """状态机进入断开状态时的回调（可重写）"""
        logger.debug(f"Entering disconnected state with event: {event}\n\nserver params: {self.params}")
        # 关闭异步会话，保证资源的正常释放
        logger.debug(f"Enter disconnected state async task: {asyncio.current_task().get_name()}")
        await self._close_task()
        # 等待会话关闭
        await self._async_session_closed_event.wait()

    async def aafter_disconnect(self, event: EventData) -> None:
        """断开后操作（可重写）"""
        logger.debug(f"After disconnection actions with event: {event}\n\nserver params: {self.params}")
        await self._trigger_state_change(event)

    async def aprepare_error(self, event: EventData) -> None:
        """错误准备操作（可重写）"""
        logger.debug(f"Preparing error with event: {event}\n\nserver params: {self.params}")

    async def acan_error(self, event: EventData) -> bool:
        """错误条件检查（可重写）"""
        logger.debug(f"Checking error conditions with event: {event}\n\nserver params: {self.params}")
        return True

    async def abefore_error(self, event: EventData) -> None:
        """错误前操作（可重写）"""
        logger.debug(f"Before error actions with event: {event}\n\nserver params: {self.params}")

    async def on_enter_error(self, event: EventData) -> None:
        """状态机进入错误状态时的回调（可重写）"""
        logger.debug(f"Entered error state with event: {event}\n\nserver params: {self.params}")
        # 将所有异步Event全部clear
        await self._close_task()

    async def aafter_error(self, event: EventData) -> None:
        """错误后操作（可重写）"""
        logger.debug(f"After error actions with event: {event}\n\nserver params: {self.params}")
        await self._trigger_state_change(event)

    async def aprepare_initialize(self, event: EventData) -> None:
        """初始化准备操作（可重写）"""
        logger.debug(f"Preparing initialization with event: {event}\n\nserver params: {self.params}")

    async def acan_initialize(self, event: EventData) -> bool:
        """初始化条件检查（可重写）"""
        logger.debug(f"Checking initialization conditions with event: {event}\n\nserver params: {self.params}")
        return True

    async def abefore_initialize(self, event: EventData) -> None:
        """初始化前操作（可重写）"""
        logger.debug(f"Before initialization actions with event: {event}\n\nserver params: {self.params}")

    async def on_enter_initialized(self, event: EventData) -> None:
        """状态机进入初始化状态时的回调（可重写）"""
        logger.debug(f"Entered initialized state with event: {event}\n\nserver params: {self.params}")
        # 将所有异步Event全部clear
        self._create_session_success_event.clear()
        self._create_session_failure_event.clear()
        self._async_session_closed_event.clear()
        await self._close_task()

    async def aafter_initialize(self, event: EventData) -> None:
        """初始化后操作（可重写）"""
        logger.debug(f"After initialization actions with event: {event}\n\nserver params: {self.params}")
        await self._trigger_state_change(event)

    async def list_tools(self) -> list[Tool]:
        """
        获取可用工具列表，MCP协议获取接口可分页，在此会尝试获取所有。对于大模型使用场景而言，需要一次性给出所有可用工具，没有必要分页，如果数据量过大，则属于设计问题。

        Returns:
            list[Tool]: 工具列表
        """
        if self.state != STATES.connected:
            raise ConnectionError("Not connected to server")
        tools: list[Tool] = []
        if self.initialize_result and self.initialize_result.capabilities.tools:
            asession = cast(ClientSession, await self.async_session)
            ret = await asession.list_tools()
            tools.extend(ret.tools)
            while ret.nextCursor:
                ret = await asession.list_tools(cursor=ret.nextCursor)
                tools.extend(ret.tools)
        return tools

    async def list_windows(self) -> list[Resource]:
        """
        列出当前MCP服务可用的窗口资源列表。需要注意MCP Server如果想开启桌面模式必须打开 resources/subscribe

        同时开发者需要注意维护好 window:// 状态

        Returns:
            list[Resource]: 当前可用的窗口类资源
        """
        if (
            self.initialize_result
            and self.initialize_result.capabilities.resources
            and self.initialize_result.capabilities.resources.subscribe
        ):
            # Get available resources directly from client session
            try:
                asession = cast(ClientSession, await self.async_session)
                # 中文: 支持分页获取资源；与 list_tools 一致，穷举所有页后再进行过滤与订阅
                # 英文: Support pagination; same as list_tools, exhaust all pages then filter and subscribe
                resources: list[Resource] = []
                ret = await asession.list_resources()
                if ret:
                    resources.extend(ret.resources)
                    while ret.nextCursor:
                        ret = await asession.list_resources(cursor=ret.nextCursor)
                        resources.extend(ret.resources)
                # 返回满足WindowURI协议要求的Resource
                # Return only resources that conform to WindowURI (window:// scheme)
                filtered: list[tuple[Resource, int]] = []
                for res in resources:
                    # 类型守卫：快速判定并过滤非 window:// 资源
                    if not is_window_uri(res.uri):
                        continue
                    # 解析优先级（缺省为0）
                    uri = WindowURI(str(res.uri))
                    prio = uri.priority if uri.priority is not None else 0
                    filtered.append((res, prio))

                # 同一 MCP 内按 priority 降序排序（仅在本客户端内比较）
                filtered.sort(key=lambda x: x[1], reverse=True)
                # 如果当前MCP Server开启了 resources 的订阅模式，则将过滤出来的Resources进行订阅
                for r, _ in filtered:
                    await asession.subscribe_resource(r.uri)
                return [r for r, _ in filtered]
            except Exception as e:
                logger.error(f"Error listing resources for connector {self.params.model_dump(mode='json')}: {e}")
                return []
        else:
            return []

    async def get_window_detail(self, resource: Resource | str) -> ReadResourceResult:
        """
        中文: 读取单个窗口资源的详细内容（通过 MCP read_resource）。
        英文: Read details for a single window resource via MCP read_resource.

        Args:
            resource (Resource | str): 要读取的资源（或其 URI 字符串）。

        Returns:
            list[object]: 资源内容块列表（如 TextContent/BlobContent 等）。读取失败返回空列表。
        """
        try:
            asession = cast(ClientSession, await self.async_session)
            uri_val: AnyUrl
            if isinstance(resource, Resource):
                uri_val = resource.uri  # type: ignore[assignment]
            else:
                # 当传入为字符串时，交由底层进行校验/解析
                uri_val = AnyUrl(resource)  # type: ignore[call-arg]

            return await asession.read_resource(uri_val)
        except Exception as e:
            logger.error(f"Read window resource failed: {resource}: {e}")
            return ReadResourceResult(contents=[TextResourceContents(text="获取资源失败", uri=resource.uri)])

    async def call_tool(self, tool_name: str, params: dict) -> CallToolResult:
        """
        运行指定工具（子类必须实现）

        在此不需要再考虑工具Alias的问题，由外层Manager进行处理，因此直接尝试调用触发MCP协议即可

        Args:
            tool_name (str): 被调用的工具名称
            params (dict): 调用参数

        Returns:
            CallToolResult: 调用结果 MCP 协议标准制定
        """
        if self.state != STATES.connected:
            raise ConnectionError("Not connected to server")
        return await (await self.async_session).call_tool(tool_name, params)

    async def _close_task(self) -> None:
        """
        关闭异步任务
        """
        # 将所有异步Event全部clear
        if self._session_keep_alive_task and not self._session_keep_alive_task.done():
            self._session_keep_alive_task.cancel()

            # 等待_session_keep_alive_task结束
            try:
                await self._session_keep_alive_task
            except asyncio.CancelledError:
                logger.debug("Session keep-alive task was cancelled")
            except Exception as e:
                logger.error(f"Session keep-alive task failed: {e}")
