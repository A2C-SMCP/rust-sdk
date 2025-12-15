# filename: manager.py
# @Time    : 2025/8/17 16:53
# @Author  : JQQ
# @Email   : jiaqia@qknode.com
# @Software: PyCharm
import asyncio
import copy
import json
from collections import defaultdict
from collections.abc import AsyncGenerator, Iterable
from typing import Any

from mcp.client.session import MessageHandlerFnT
from mcp.types import CallToolResult, ReadResourceResult, Resource, Tool
from vrl_python import VRLRuntime

from a2c_smcp.computer.mcp_clients.model import A2C_TOOL_META, A2C_VRL_TRANSFORMED, MCPClientProtocol, MCPServerConfig, ToolMeta
from a2c_smcp.computer.mcp_clients.utils import client_factory
from a2c_smcp.types import SERVER_NAME, TOOL_NAME
from a2c_smcp.utils.logger import logger


class ToolNameDuplicatedError(Exception):
    def __init__(self, *args: Any) -> None:
        super().__init__(*args)


class MCPServerManager:
    """
    MCP Server管理器

    所有以下划线开头的私有方法是非协程安全的。如果外部调用，需要使用普通方法。

    # TODO call_tool的时候需要启动子任务对call_tool进行包装，以便进行超时控制与动态取消。实现超时控制与动态取消后，才可以响应服务端取消任务
    """

    def __init__(
        self,
        auto_connect: bool = False,
        auto_reconnect: bool = True,
        message_handler: MessageHandlerFnT | None = None,
    ) -> None:
        # 存储所有服务器配置
        self._servers_config: dict[SERVER_NAME, MCPServerConfig] = {}
        # 存储活动客户端 {server_name: client}
        self._active_clients: dict[SERVER_NAME, MCPClientProtocol] = {}
        # 工具到服务器的映射 {tool_name: server_name}
        self._tool_mapping: dict[TOOL_NAME, SERVER_NAME] = {}
        # 工具的alias到server+original_name的映射 {alias: (server_name, original_name)}
        self._alias_mapping: dict[str, tuple[SERVER_NAME, TOOL_NAME]] = {}
        # 禁用工具集合
        self._disabled_tools: set[TOOL_NAME] = set()
        # 自动重连标志
        self._auto_reconnect: bool = auto_reconnect
        # 自动连接标志
        self._auto_connect: bool = auto_connect
        # 自定义消息处理器，透传到各具体Client
        self._message_handler: MessageHandlerFnT | None = message_handler
        # 内部锁防止并发修改
        self._lock = asyncio.Lock()

    def get_server_config(self, server_name: SERVER_NAME) -> MCPServerConfig:
        """通过名称获取服务配置"""
        return self._servers_config[server_name]

    def get_tool_meta(self, server_name: SERVER_NAME, tool_name: TOOL_NAME) -> ToolMeta | None:
        """
        中文: 获取指定服务器下某工具合并后的元数据（优先具体 tool_meta，缺失字段回落 default_tool_meta）。
        English: Get merged ToolMeta for a tool under the given server (specific overrides; fallback to default).

        Args:
            server_name (SERVER_NAME): 服务器名称 / server name
            tool_name (TOOL_NAME): 工具原始名称或别名解析后的名称 / tool name

        Returns:
            ToolMeta | None: 合并后的工具元数据；若两侧均为空返回 None / merged ToolMeta or None if both absent.
        """
        config = self.get_server_config(server_name)
        return self._merged_tool_meta(config, tool_name)

    async def enable_auto_connect(self) -> None:
        """启用自动连接"""
        async with self._lock:
            self._auto_connect = True

    async def disable_auto_connect(self) -> None:
        """禁用自动连接"""
        async with self._lock:
            self._auto_connect = False

    async def enable_auto_reconnect(self) -> None:
        """启用自动重连"""
        async with self._lock:
            self._auto_reconnect = True

    async def disable_auto_reconnect(self) -> None:
        """禁用自动重连"""
        async with self._lock:
            self._auto_reconnect = False

    async def ainitialize(self, servers: Iterable[MCPServerConfig]) -> None:
        """
        初始化管理器并添加服务器配置

        Args:
            servers (list[MCPServerConfig]): MCP服务器配置
        """
        async with self._lock:
            # 清理旧设置与配置
            # 1. 停止所有活动客户端
            await self._astop_all()
            # 2. 清空所有状态存储
            self._clear_all()
            # 3. 添加新配置
            for server in servers:
                await self._add_or_update_server_config(server)
            try:
                await self._arefresh_tool_mapping()
            except ToolNameDuplicatedError as e:  # pragma: no cover
                # 极端分支：仅在外部错误用法下触发，主流程不会走到这里
                # 中文：此处为防御性分支，正常流程不会触发
                # English: Defensive branch, not triggered in normal flow
                await self._astop_all()  # pragma: no cover
                self._clear_all()  # pragma: no cover
                raise e  # pragma: no cover

    async def _add_or_update_server_config(self, config: MCPServerConfig) -> None:
        """
        添加/更新服务器配置（不启动客户端）

        如果已存在，检查是否已经建立客户端连接，如果是，检查是否需要自动重连
        如果不存在，直接添加配置

        Args:
            config (MCPServerConfig): MCP服务器配置
        """
        if config.name in self._servers_config:
            # 配置更新时检查是否激活
            if config.name in self._active_clients:
                if self._auto_reconnect:
                    self._servers_config[config.name] = config
                    await self._arestart_server(config.name)
                else:
                    raise RuntimeError(f"Server {config.name} is active. Stop it before updating config")
            else:
                # 配置存在但客户端未激活，更新配置并根据 auto_connect 决定是否启动
                # Config exists but client is not active, update config and start if auto_connect is enabled
                self._servers_config[config.name] = config
                if self._auto_connect:
                    await self._astart_client(config.name)
        else:
            self._servers_config[config.name] = config
            if self._auto_connect:
                await self._astart_client(config.name)

    async def aadd_or_aupdate_server(self, config: MCPServerConfig) -> None:
        """
        添加或更新服务器配置

        Args:
            config (MCPServerConfig): MCP服务器配置
        """
        async with self._lock:
            backup_config = copy.deepcopy(self._servers_config)
            try:
                await self._add_or_update_server_config(config)
                await self._arefresh_tool_mapping()
            except ToolNameDuplicatedError as e:
                self._servers_config = backup_config
                raise e

    async def aremove_server(self, server_name: str) -> None:
        """移除服务器配置"""
        async with self._lock:
            if server_name in self._active_clients:
                await self._astop_client(server_name)
            del self._servers_config[server_name]
            await self._arefresh_tool_mapping()

    async def _arestart_server(self, server_name: str) -> None:
        """
        重启服务器客户端

        Args:
            server_name (str): 服务器名称
        """
        # 明确使用当前管理器中的最新配置
        config = self._servers_config.get(server_name)
        if not config:
            # 极端分支：仅在外部错误用法下触发，主流程不会走到这里
            # 中文：此处为防御性分支，正常流程不会触发
            # English: Defensive branch, not triggered in normal flow
            raise ValueError(f"Server {server_name} not found in config")  # pragma: no cover

        # 确保使用最新配置重启
        if server_name in self._active_clients:
            await self._astop_client(server_name)

        # 只有启用的配置才能重启
        if not config.disabled:
            await self._astart_client(server_name)

    async def astart_all(self) -> None:
        """启动所有启用的服务器"""
        async with self._lock:
            logger.debug(f"Manager Start all async task: {asyncio.current_task()}")
            for server_name in self._servers_config:
                if not self._servers_config[server_name].disabled:
                    await self._astart_client(server_name)

    async def astart_client(self, server_name: str) -> None:
        """启动单个服务器客户端"""
        async with self._lock:
            await self._astart_client(server_name)

    async def _astart_client(self, server_name: str) -> None:
        """启动单个服务器客户端"""
        config = self._servers_config.get(server_name)
        if not config:
            # 极端分支：仅在外部错误用法下触发，主流程不会走到这里
            # 中文：此处为防御性分支，正常流程不会触发
            # English: Defensive branch, not triggered in normal flow
            raise ValueError(f"Unknown server: {server_name}")  # pragma: no cover

        if config.disabled:
            raise RuntimeError(f"Cannot start disabled server: {server_name}")

        if server_name in self._active_clients:
            return  # 已经启动

        # 根据配置类型创建客户端
        client = client_factory(config, message_handler=self._message_handler)
        await client.aconnect()
        self._active_clients[server_name] = client
        try:
            await self._arefresh_tool_mapping()
        except ToolNameDuplicatedError as e:
            await client.adisconnect()
            del self._active_clients[server_name]
            raise e

    async def astop_client(self, server_name: str) -> None:
        """停止单个服务器客户端"""
        async with self._lock:
            await self._astop_client(server_name)

    async def _astop_client(self, server_name: str) -> None:
        """停止单个服务器客户端"""
        client = self._active_clients.pop(server_name, None)
        if client:
            await client.adisconnect()
            await self._arefresh_tool_mapping()

    async def _astop_all(self) -> None:
        """停止所有客户端"""
        for name in list(self._active_clients.keys()):
            await self._astop_client(name)

    async def astop_all(self) -> None:
        """停止所有客户端"""
        async with self._lock:
            logger.debug(f"Manager Stop all async task: {asyncio.current_task()}")
            await self._astop_all()

    def _clear_all(self) -> None:
        """清空所有连接（别名）"""
        self._servers_config.clear()
        self._active_clients.clear()
        self._tool_mapping.clear()
        self._alias_mapping.clear()
        self._disabled_tools.clear()

    async def aclose(self) -> None:
        """关闭所有连接（别名）"""
        await self.astop_all()

        # 2. 清空所有状态存储
        self._clear_all()

    async def _arefresh_tool_mapping(self) -> None:
        """刷新工具映射和禁用状态"""
        # 清空现有映射
        self._tool_mapping.clear()
        self._disabled_tools.clear()
        self._alias_mapping.clear()

        # 临时存储工具源服务器
        tool_sources: dict[TOOL_NAME, list[str]] = defaultdict(list)

        # 收集所有活动服务器的工具
        for server_name, client in self._active_clients.items():
            config = self._servers_config[server_name]
            try:
                tools = await client.list_tools()
                for t in tools:
                    original_tool_name = t.name
                    # 获取合并后的工具元数据（浅合并，具体配置优先，其次使用默认配置）
                    # Get merged tool meta (shallow merge: specific overrides default)
                    tool_meta = self._merged_tool_meta(config, original_tool_name)

                    # 确定最终显示的工具名（优先使用别名）
                    display_name: str = tool_meta.alias if tool_meta and tool_meta.alias else original_tool_name
                    # 如果使用提别名，则更新别名映射
                    if display_name != original_tool_name:
                        self._alias_mapping[display_name] = (server_name, original_tool_name)

                    # 将工具添加到映射
                    tool_sources[display_name].append(server_name)

                    # 检查是否为禁用工具 (根据配置，但此时需要注意如果原始名称在禁用列表中，也应该禁用，因为此处的禁用列表是归属于某个
                    # ServerConfig的，不存在重复名称的情况，用户有可能配置了alias，但是使用原始名称禁用。)
                    if display_name in (config.forbidden_tools or []) or original_tool_name in (config.forbidden_tools or []):
                        self._disabled_tools.add(display_name)
            except Exception as e:
                logger.error(f"Error listing tools for {server_name}: {e}")

        # 构建最终映射（处理工具名冲突）
        for tool, sources in tool_sources.items():
            if len(sources) > 1:
                logger.warning(f"Warning: Tool '{tool}' exists in multiple servers: {sources}")
                suggestion = (
                    "Please use the 'alias' feature in ToolMeta to resolve conflicts. "
                    "Each tool should have a unique name or alias across all servers."
                )
                raise ToolNameDuplicatedError(f"Tool '{tool}' exists in multiple servers: {sources}\n{suggestion}")
            self._tool_mapping[tool] = sources[0]

    async def avalidate_tool_call(self, tool_name: TOOL_NAME, parameters: dict) -> tuple[SERVER_NAME, TOOL_NAME]:
        """
        判断工具调用的合法性，如果合法，返回对应的服务名称与原始工具名称

        Args:
            tool_name (str): 被调用的工具名称，可能是alias
            parameters (dict): 工具调用的参数

        Returns:
            tuple[SERVER_NAME, TOOL_NAME]: 经过校验后的合法服务名与工具名
        """
        # 标记当前parameters尚未被使用
        logger.debug(f"{parameters}未被检查。当前版本不支持Schema校验。")
        # 检查工具是否可用
        if tool_name in self._disabled_tools:
            raise PermissionError(f"Tool '{tool_name}' is disabled by configuration")

        server_name = self._tool_mapping.get(tool_name)
        if not server_name:
            raise ValueError(f"Tool '{tool_name}' not found in any active server")

        # 如果tool_name是一个别名，则使用别名映射到原始名称
        if tool_name in self._alias_mapping:
            original_server_name, tool_name = self._alias_mapping[tool_name]
            assert original_server_name == server_name, "Alias mapping should map to the same server"
        return server_name, tool_name

    async def acall_tool(
        self,
        server_name: SERVER_NAME,
        tool_name: TOOL_NAME,
        parameters: dict,
        timeout: float | None = None,
    ) -> CallToolResult:
        """
        触发MCP工具的调用。注意此方法tool_name必须是工具原始名称，如果是alias别名调用，需要使用 aexecute_tool

        Args:
            server_name (str): 服务名称
            tool_name (str): 工具名称
            parameters (dict): 工具调用参数
            timeout (float | None): 超时时间

        Returns:
            CallToolResult: MCP 标准返回格式
        """
        # 获取MCP服务客户端连接
        client = self._active_clients.get(server_name)
        if not client:
            raise RuntimeError(f"Server '{server_name}' for tool '{tool_name}' is not active")

        # 获取合并后的工具元数据
        config = self._servers_config[server_name]
        tool_meta = self._merged_tool_meta(config, tool_name)

        # 执行工具调用
        try:
            if timeout:
                result = await asyncio.wait_for(client.call_tool(tool_name, parameters), timeout)
            else:
                result = await client.call_tool(tool_name, parameters)

            # 如果有自定义元数据，则利用MCP协议返回Result中的meta元数据携带能力透传。
            if tool_meta:
                if result.meta:
                    result.meta.setdefault(A2C_TOOL_META, {}).update(tool_meta)
                else:
                    result.meta = {A2C_TOOL_META: tool_meta}

            # 中文: 如果配置了VRL脚本,尝试对返回值进行转换
            # English: If VRL script is configured, try to transform the return value
            if config.vrl:
                try:
                    # 中文: 将CallToolResult序列化为字典，并注入tool_name和parameters作为VRL的Event输入
                    # English: Serialize CallToolResult to dict and inject tool_name and parameters as VRL Event input
                    event = result.model_dump(mode="json")
                    # 中文: 注入工具调用的上下文信息
                    # English: Inject tool call context information
                    event["tool_name"] = tool_name
                    event["parameters"] = parameters

                    # 中文: 执行VRL转换（使用系统本地时区）
                    # English: Execute VRL transformation (use system local timezone)
                    # 获取系统时区名称，例如 "Asia/Shanghai" 或 "America/New_York"
                    # Get system timezone name, e.g., "Asia/Shanghai" or "America/New_York"
                    # VRL需要IANA时区名称，尝试从tzlocal获取；若失败则使用UTC
                    # VRL requires IANA timezone name; try to get from tzlocal, fallback to UTC
                    try:
                        import tzlocal

                        timezone_name = str(tzlocal.get_localzone())
                    except Exception:
                        # 如果无法获取本地时区，回退到UTC / Fallback to UTC if local timezone unavailable
                        timezone_name = "UTC"

                    vrl_result = VRLRuntime.run(config.vrl, event, timezone=timezone_name)
                    transformed_event = vrl_result.processed_event

                    # 中文: 将转换后的结果压缩为JSON字符串存入Meta（因为Meta要求简单数据结构）
                    # English: Compress transformed result to JSON string for Meta (Meta requires simple data structure)
                    if result.meta is None:
                        result.meta = {}
                    result.meta[A2C_VRL_TRANSFORMED] = json.dumps(transformed_event, ensure_ascii=False)

                    logger.debug(f"VRL转换成功 / VRL transformation succeeded for tool '{tool_name}'")
                except Exception as e:
                    # 中文: VRL转换失败不影响正常返回，仅记录警告日志
                    # English: VRL transformation failure doesn't affect normal return, just log warning
                    logger.warning(
                        f"VRL转换失败 / VRL transformation failed for tool '{tool_name}': {e}. "
                        f"原始结果将正常返回 / Original result will be returned normally.",
                    )

            return result
        except TimeoutError:
            raise TimeoutError(f"Tool '{tool_name}' execution timed out") from None
        except Exception as e:
            raise RuntimeError(f"Tool execution failed: {e}") from e

    async def aexecute_tool(self, tool_name: TOOL_NAME, parameters: dict, timeout: float | None = None) -> CallToolResult:
        """执行指定工具 与 acall_tool 的区别在于此方法支持使用alias别名进行调用。"""
        server_name, tool_name = await self.avalidate_tool_call(tool_name, parameters)
        return await self.acall_tool(server_name, tool_name, parameters, timeout)

    def get_server_status(self) -> list[tuple[str, bool, str]]:
        """获取服务器状态列表"""
        return [
            (
                server_name,
                server_name in self._active_clients,
                "pending" if server_name not in self._active_clients else self._active_clients[server_name].state,
            )
            for server_name in self._servers_config
        ]

    async def available_tools(self) -> AsyncGenerator[Tool, Any]:
        """获取可用工具及其元数据"""
        async with self._lock:
            servers_cached_tools = defaultdict(list)
            for tool_name, server in self._tool_mapping.items():
                if server not in servers_cached_tools and server in self._active_clients:
                    client = self._active_clients[server]
                    tools = await client.list_tools()
                    servers_cached_tools[server] = tools

                config = self._servers_config[server]
                assert not config.disabled, "Server should not be disabled"

                original_server, original_tool_name = self._alias_mapping.get(tool_name) or (server, tool_name)
                assert original_server == server, "Alias mapping error"

                tool = next((t for t in tools if t.name == original_tool_name), None)
                if tool:
                    a2c_meta = self._merged_tool_meta(config, original_tool_name)
                    if a2c_meta:
                        if tool.meta is None:
                            tool.meta = {A2C_TOOL_META: a2c_meta}
                        else:
                            tool.meta.update({A2C_TOOL_META: a2c_meta})
                    yield tool

    async def list_windows(self, window_uri: str | None = None) -> list[tuple[SERVER_NAME, Resource]]:
        """
        列出所有活动MCP服务器的窗口资源，并附带其归属的server名称。
        List window resources from all active MCP servers with owning server name.

        Args:
            window_uri (str | None): 若提供，则仅返回URI完全匹配的窗口；否则返回所有窗口。

        Returns:
            list[tuple[SERVER_NAME, Resource]]: [(server_name, resource), ...]
        """
        results: list[tuple[SERVER_NAME, Resource]] = []
        # 不加锁读取活跃客户端快照，避免长时间持锁阻塞 I/O
        active_snapshot = list(self._active_clients.items())
        for server_name, client in active_snapshot:
            try:
                resources = await client.list_windows()
            except Exception as e:
                logger.error(f"Error listing windows for {server_name}: {e}")
                continue

            for res in resources:
                if window_uri is not None and str(res.uri) != window_uri:
                    continue
                results.append((server_name, res))
        return results

    async def get_windows_details(self, window_uri: str | None = None) -> list[tuple[SERVER_NAME, Resource, ReadResourceResult]]:
        """
        中文: 读取所有活动 MCP 服务器的窗口资源详情。由于 MCP 协议中的 Resource 仅为标识，需要通过 read_resource 获取内容。
        英文: Read detailed contents for window resources from all active MCP servers. Resource is an identifier; use read_resource.

        Args:
            window_uri (str | None): 若提供，则仅读取该 URI 完全匹配的窗口；否则读取所有窗口。

        Returns:
            list[tuple[SERVER_NAME, Resource, list[object]]]: 列表项为 (server_name, resource, contents)。
        """
        details: list[tuple[SERVER_NAME, Resource, ReadResourceResult]] = []
        active_snapshot = list(self._active_clients.items())
        for server_name, client in active_snapshot:
            try:
                resources = await client.list_windows()
            except Exception as e:
                logger.error(f"Error listing windows for {server_name}: {e}")
                continue

            for res in resources:
                if window_uri is not None and str(res.uri) != window_uri:
                    continue
                content = await client.get_window_detail(res)
                details.append((server_name, res, content))
        return details

    @staticmethod
    def _merged_tool_meta(config: MCPServerConfig, tool_name: TOOL_NAME) -> ToolMeta | None:
        """
        浅层合并工具元数据：优先使用具体 tool_meta，若字段缺失则回落到 default_tool_meta。
        Shallow merge ToolMeta: prefer per-tool meta; fallback to default for missing root-level fields.
        """
        specific = (config.tool_meta or {}).get(tool_name)
        default = config.default_tool_meta
        if specific is None and default is None:
            return None
        if specific is None:
            return default
        if default is None:
            return specific
        # 仅根级字段浅合并；specific优先
        merged: dict = {}
        # Pydantic v2: model_dump 可排除 None，以避免用 None 覆盖
        merged.update(default.model_dump(exclude_none=True))
        merged.update(specific.model_dump(exclude_none=True))
        return ToolMeta(**merged)
