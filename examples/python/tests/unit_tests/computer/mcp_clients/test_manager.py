# -*- coding: utf-8 -*-
# filename: test_manager.py
# @Time    : 2025/8/18 14:59
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
import asyncio
from typing import Any, cast
from unittest.mock import AsyncMock, MagicMock

import pytest
from mcp import StdioServerParameters, Tool
from mcp.client.session_group import SseServerParameters, StreamableHttpParameters
from mcp.types import CallToolResult

from a2c_smcp.computer.mcp_clients.manager import MCPServerManager, ToolNameDuplicatedError
from a2c_smcp.computer.mcp_clients.model import (
    MCPClientProtocol,
    MCPServerConfig,
    SseServerConfig,
    StdioServerConfig,
    StreamableHttpServerConfig,
    ToolMeta,
)

# 模拟类型定义
TOOL_NAME = str
SERVER_NAME = str


# 模拟BaseMCPClient
class MockMCPClient:
    def __init__(self, tools: list[Tool] = None, ret_meta: dict | None = None, message_handler=None):
        self.tools = tools or []
        self.aconnect = AsyncMock()
        self.adisconnect = AsyncMock()
        self.list_tools = AsyncMock(return_value=tools)
        call_ret = MagicMock(spec=CallToolResult)
        call_ret.result = None
        call_ret.meta = ret_meta
        self.call_tool = AsyncMock(return_value=call_ret)
        self.state = "connected"
        # 保存透传进来的 message_handler，便于测试断言
        self.message_handler = message_handler


def create_mock_tool(name: str, meta: dict | None = None) -> Tool:
    tool = MagicMock(name=name, spec=Tool)
    tool.name = name
    tool.meta = meta
    return tool


# 模拟client_factory函数
def mock_client_factory(config: MCPServerConfig, message_handler=None) -> MockMCPClient:
    # 简化处理：根据配置名称返回不同的工具列表
    if "server1" in config.name:
        return MockMCPClient([create_mock_tool("tool1", meta={"test": "meta"}), create_mock_tool("tool2")], message_handler=message_handler)
    elif "server2" in config.name:
        return MockMCPClient(
            [create_mock_tool("tool3"), create_mock_tool("tool4")],
            ret_meta={"test": "ret_meta"},
            message_handler=message_handler,
        )
    elif "alias_server" in config.name:
        return MockMCPClient([create_mock_tool("tool5")], message_handler=message_handler)
    elif "duplicate_server" in config.name:
        return MockMCPClient([create_mock_tool("duplicate_tool")], message_handler=message_handler)
    return MockMCPClient(message_handler=message_handler)


# Monkey patch客户端工厂函数
@pytest.fixture(autouse=True)
def patch_client_factory(monkeypatch):
    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", mock_client_factory)


# 创建示例服务器配置
def create_server_config(
    name: str,
    disabled: bool = False,
    forbidden_tools: list = None,
    tool_meta: dict = None,
    default_tool_meta: ToolMeta | None = None,
) -> MCPServerConfig:
    forbidden_tools = forbidden_tools or []
    tool_meta = tool_meta or {}
    if "sse" in name:
        return SseServerConfig(
            name=name,
            disabled=disabled,
            forbidden_tools=forbidden_tools,
            tool_meta=tool_meta,
            default_tool_meta=default_tool_meta,
            server_parameters=MagicMock(spec=SseServerParameters),
        )
    elif "http" in name:
        return StreamableHttpServerConfig(
            name=name,
            disabled=disabled,
            forbidden_tools=forbidden_tools,
            tool_meta=tool_meta,
            default_tool_meta=default_tool_meta,
            server_parameters=MagicMock(spec=StreamableHttpParameters),
        )
    else:
        return StdioServerConfig(
            name=name,
            disabled=disabled,
            forbidden_tools=forbidden_tools,
            tool_meta=tool_meta,
            default_tool_meta=default_tool_meta,
            server_parameters=MagicMock(spec=StdioServerParameters),
        )


@pytest.fixture
async def manager() -> MCPServerManager:
    manager = MCPServerManager()
    await manager.enable_auto_reconnect()  # 启用自动重连便于测试
    return manager


@pytest.mark.asyncio
async def test_initialize_with_servers(manager):
    """测试初始化和服务器启动"""
    servers = [create_server_config("server1"), create_server_config("server2", disabled=True), create_server_config("sse_server")]

    await manager.ainitialize(servers)

    # 初始化后不会自动启动所有服务。验证活动客户端
    assert "server1" not in manager._active_clients
    assert "sse_server" not in manager._active_clients
    assert "server2" not in manager._active_clients

    # 调用start_all
    await manager.astart_all()

    # 验证启动
    assert "server1" in manager._active_clients
    assert "sse_server" in manager._active_clients
    assert "server2" not in manager._active_clients

    # 验证工具映射
    assert manager._tool_mapping["tool1"] == "server1"
    assert manager._tool_mapping["tool2"] == "server1"
    assert "tool3" not in manager._tool_mapping  # 禁用的服务器

    # 验证状态检查
    statuses = manager.get_server_status()
    assert ("server1", True, "connected") in statuses
    assert ("server2", False, "pending") in statuses
    assert ("sse_server", True, "connected") in statuses


@pytest.mark.asyncio
async def test_tool_execution(manager):
    """测试工具执行流程"""
    servers = [create_server_config("server1")]
    await manager.ainitialize(servers)

    await manager.astart_all()

    # 执行工具
    params = {"key": "value"}
    await manager.aexecute_tool("tool1", params)

    # 验证调用
    client = manager._active_clients["server1"]
    client.call_tool.assert_awaited_once_with("tool1", params)

    with pytest.raises(Exception):
        await manager.aexecute_tool("tool5", params)


@pytest.mark.asyncio
async def test_tool_execution_with_ret_meta(manager):
    """测试工具执行流程"""
    servers = [create_server_config("server2", tool_meta={"tool3": ToolMeta(ret_meta={"test": "ret_meta"})})]
    await manager.ainitialize(servers)

    await manager.astart_all()

    # 执行工具
    params = {"key": "value"}
    ret = await manager.aexecute_tool("tool3", params)
    assert ret.meta["test"] == "ret_meta"

    # 验证调用
    client = manager._active_clients["server2"]
    client.call_tool.assert_awaited_once_with("tool3", params)


@pytest.mark.asyncio
async def test_tool_with_alias(manager):
    """测试别名映射功能"""
    tool_meta = {"tool5": ToolMeta(alias="aliased_tool")}
    servers = [create_server_config("alias_server", tool_meta=tool_meta)]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 验证别名映射
    assert manager._alias_mapping["aliased_tool"] == ("alias_server", "tool5")
    assert "tool5" not in manager._tool_mapping
    assert manager._tool_mapping["aliased_tool"] == "alias_server"

    # 执行别名工具
    await manager.aexecute_tool("aliased_tool", {})
    client = manager._active_clients["alias_server"]
    print("Call args list:", client.call_tool.call_args_list)
    client.call_tool.assert_awaited_once_with("tool5", {})


@pytest.mark.asyncio
async def test_disabled_tool(manager):
    """测试禁用工具处理"""
    servers = [create_server_config("server1", forbidden_tools=["tool2"])]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 验证禁用状态
    assert "tool2" in manager._disabled_tools

    # 尝试执行禁用工具
    with pytest.raises(PermissionError):
        await manager.aexecute_tool("tool2", {})


@pytest.mark.asyncio
async def test_tool_name_conflict(manager):
    """测试工具名冲突处理"""
    servers = [
        create_server_config("server1"),
        create_server_config("duplicate_server", tool_meta={"duplicate_tool": ToolMeta(alias="tool1")}),
    ]

    # 验证初始化时检测到冲突
    with pytest.raises(ToolNameDuplicatedError):
        await manager.ainitialize(servers)
        await manager.astart_all()

    # 工具重名导致的异常是在逐个启动Client的时候抛出的，因此只会回滚检测到异常的Client，
    # 而不会回滚所有Client
    assert len(manager._active_clients) == 1


@pytest.mark.asyncio
async def test_dynamic_server_management(manager):
    """测试动态添加/移除服务器"""
    # 初始配置
    servers = [create_server_config("server1")]
    await manager.ainitialize(servers)
    await manager.astart_all()
    assert "server1" in manager._active_clients

    # 添加新服务器
    new_server = create_server_config("http_server")
    await manager.aadd_or_aupdate_server(new_server)

    # 验证新服务器启动
    assert "http_server" not in manager._active_clients
    await manager._astart_client("http_server")
    assert "http_server" in manager._active_clients

    # 更新服务器配置（启用自动重连）
    updated_server = create_server_config("server1", forbidden_tools=["tool1"])
    # 验证服务器重启
    old_client = manager._active_clients["server1"]  # 要提示保存旧客户端的引用，因为add_or_update_server会销毁旧客户端
    await manager.aadd_or_aupdate_server(updated_server)
    await asyncio.sleep(0.1)  # 等待自动重连 需要释放一次协程才能触发协程任务的执行与调用。

    old_client.adisconnect.assert_awaited()

    # 验证更新应用
    assert "tool1" in manager._disabled_tools

    # 移除服务器
    await manager.aremove_server("http_server")
    assert "http_server" not in manager._active_clients
    assert "http_server" not in manager._servers_config


@pytest.mark.asyncio
async def test_auto_reconnect_disabled(manager):
    """测试禁用自动重连时更新配置"""
    await manager.disable_auto_reconnect()

    servers = [create_server_config("server1")]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 尝试更新活动服务器的配置
    updated_config = create_server_config("server1", forbidden_tools=["tool1"])

    with pytest.raises(RuntimeError):
        await manager.aadd_or_aupdate_server(updated_config)


@pytest.mark.asyncio
async def test_get_available_tools(manager):
    """测试获取可用工具"""
    tool_meta = {"tool1": ToolMeta(auto_apply=True)}
    servers = [create_server_config("server1", tool_meta=tool_meta)]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 获取工具
    tools = []
    async for tool in manager.available_tools():
        tools.append(tool)

    assert len(tools) == 2
    tool1 = next(t for t in tools if t.name == "tool1")
    assert tool1.meta["a2c_tool_meta"].auto_apply


@pytest.mark.asyncio
async def test_default_tool_meta_applies_when_missing_per_tool(manager):
    """当未提供 per-tool 配置时，应回落使用 default_tool_meta。
    When per-tool meta is missing, default_tool_meta should be applied.
    """
    servers = [create_server_config("server1", default_tool_meta=ToolMeta(auto_apply=True))]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 检查 available_tools 注入
    tools = []
    async for tool in manager.available_tools():
        tools.append(tool)
    t1 = next(t for t in tools if t.name == "tool1")
    assert t1.meta["a2c_tool_meta"].auto_apply is True

    # 检查 aexecute_tool 返回元数据注入
    ret = await manager.aexecute_tool("tool1", {})
    assert ret.meta["a2c_tool_meta"].auto_apply is True


@pytest.mark.asyncio
async def test_per_tool_overrides_default(manager):
    """per-tool 配置应覆盖 default_tool_meta 的根级字段。
    Per-tool meta should override default_tool_meta root-level fields.
    """
    servers = [
        create_server_config(
            "server1",
            tool_meta={"tool1": ToolMeta(auto_apply=False)},
            default_tool_meta=ToolMeta(auto_apply=True),
        ),
    ]
    await manager.ainitialize(servers)
    await manager.astart_all()

    ret = await manager.aexecute_tool("tool1", {})
    assert ret.meta["a2c_tool_meta"].auto_apply is False


@pytest.mark.asyncio
async def test_error_handling(manager):
    """测试错误处理"""
    # 模拟客户端连接错误
    bad_server = create_server_config("error_server")
    bad_client = MockMCPClient()
    bad_client.list_tools.side_effect = Exception("Connection failed")
    manager.client_factory = lambda _: bad_client

    await manager.aadd_or_aupdate_server(bad_server)

    # 验证状态
    assert "error_server" not in manager._active_clients

    # 工具执行错误处理
    servers = [create_server_config("server1")]
    await manager.ainitialize(servers)
    await manager.astart_all()

    client = manager._active_clients["server1"]
    client.call_tool.side_effect = TimeoutError("Execution timed out")

    with pytest.raises(TimeoutError):
        await manager.aexecute_tool("tool1", {}, timeout=0.1)


@pytest.mark.asyncio
async def test_meta_data_injection(manager):
    """测试工具元数据注入"""
    tool_meta = {"tool1": ToolMeta(ret_object_mapper={"result": "data"})}
    servers = [create_server_config("server1", tool_meta=tool_meta)]
    await manager.ainitialize(servers)
    await manager.astart_all()

    # 执行工具
    result = await manager.aexecute_tool("tool1", {})

    # 验证元数据注入
    assert "a2c_tool_meta" in result.meta
    assert result.meta["a2c_tool_meta"].ret_object_mapper == {"result": "data"}


@pytest.mark.asyncio
async def test_manager_propagates_message_handler_to_clients():
    """验证 Manager 能将 message_handler 透传到具体 Client。
    Verify Manager forwards message_handler to concrete clients.
    """

    # 定义一个占位回调
    async def dummy_handler(*args, **kwargs):
        return None

    mgr = MCPServerManager(message_handler=dummy_handler)
    await mgr.enable_auto_reconnect()

    servers = [create_server_config("server1"), create_server_config("sse_server")]
    await mgr.ainitialize(servers)
    await mgr.astart_all()

    # 校验每个激活客户端都收到了相同的回调实例
    for name, client in mgr._active_clients.items():
        assert getattr(client, "message_handler", None) is dummy_handler, f"Client {name} did not receive message_handler"


@pytest.mark.asyncio
async def test_manager_message_handler_none_results_in_none_on_clients():
    """当未提供 message_handler 时，客户端应为 None。"""
    mgr = MCPServerManager()  # 不传入 handler
    await mgr.enable_auto_reconnect()

    servers = [create_server_config("server1")]
    await mgr.ainitialize(servers)
    await mgr.astart_all()

    client = mgr._active_clients["server1"]
    assert getattr(client, "message_handler", None) is None


# 覆盖 _add_server_config 的 RuntimeError 分支
# Test _add_server_config RuntimeError branch
@pytest.mark.asyncio
async def test_update_active_server_without_reconnect(manager):
    """
    测试：当 auto_reconnect=False 且尝试更新已激活的服务器配置时抛出 RuntimeError。
    Test: Raise RuntimeError when updating active config with auto_reconnect=False.
    """
    config = create_server_config("server1")
    await manager.disable_auto_reconnect()
    manager._servers_config[config.name] = config
    manager._active_clients[config.name] = cast(MCPClientProtocol, MagicMock(spec=MCPClientProtocol))
    with pytest.raises(RuntimeError):
        await manager._add_or_update_server_config(config)


# 覆盖 astart_client 的 ValueError/RuntimeError 分支
# Test astart_client ValueError/RuntimeError branches
@pytest.mark.asyncio
async def test_astart_client_invalid_cases(manager):
    """
    测试：启动未知服务器/禁用服务器时报错。
    Test: Raise ValueError/RuntimeError for unknown or disabled server.
    """
    # 未知服务器
    with pytest.raises(ValueError):
        await manager._astart_client("not_exist")
    # 禁用服务器
    config = create_server_config("server2", disabled=True)
    manager._servers_config[config.name] = config
    with pytest.raises(RuntimeError):
        await manager._astart_client(config.name)


# 覆盖 aexecute_tool 的 PermissionError/ValueError/RuntimeError 分支
# Test aexecute_tool PermissionError/ValueError/RuntimeError branches
@pytest.mark.asyncio
async def test_aexecute_tool_invalid_cases(manager):
    """
    测试：执行被禁用工具/未注册工具/服务器未激活时报错。
    Test: Raise PermissionError/ValueError/RuntimeError for disabled tool, missing tool, or inactive server.
    """
    # 工具被禁用
    manager._disabled_tools.add("toolX")
    with pytest.raises(PermissionError):
        await manager.aexecute_tool("toolX", {})
    # 工具未注册
    with pytest.raises(ValueError):
        await manager.aexecute_tool("no_tool", {})
    # 工具所在服务器未激活
    manager._tool_mapping["toolY"] = "serverY"
    manager._active_clients.clear()
    manager._servers_config["serverY"] = create_server_config("serverY")
    with pytest.raises(RuntimeError):
        await manager.aexecute_tool("toolY", {})


# 覆盖 aexecute_tool 的 TimeoutError/Exception 分支
# Test aexecute_tool TimeoutError/Exception branches
@pytest.mark.asyncio
async def test_aexecute_tool_timeout_and_exception(manager, monkeypatch):
    """
    测试：执行工具时超时或抛出异常。
    Test: Raise TimeoutError/RuntimeError when tool execution times out or raises.
    """
    config = create_server_config("server3")
    manager._servers_config[config.name] = config
    manager._active_clients[config.name] = cast(MCPClientProtocol, MagicMock(spec=MCPClientProtocol))
    manager._tool_mapping["toolZ"] = config.name
    mock_client = manager._active_clients[config.name]
    # 超时
    mock_client.call_tool = AsyncMock(side_effect=asyncio.TimeoutError)
    with pytest.raises(TimeoutError):
        await manager.aexecute_tool("toolZ", {}, timeout=0.01)
    # 其它异常
    mock_client.call_tool = AsyncMock(side_effect=Exception("fail"))
    with pytest.raises(RuntimeError):
        await manager.aexecute_tool("toolZ", {})


# 覆盖 _arefresh_tool_mapping 的 ToolNameDuplicatedError 分支
# Test _arefresh_tool_mapping ToolNameDuplicatedError branch
@pytest.mark.asyncio
async def test_arefresh_tool_mapping_duplicate(manager, monkeypatch):
    """
    测试：多个服务器存在同名工具时抛出 ToolNameDuplicatedError。
    Test: Raise ToolNameDuplicatedError when duplicate tool name exists across servers.
    """
    # 两个 server 都返回同名工具 duplicate_tool
    config1 = create_server_config("duplicate_server1")
    config2 = create_server_config("duplicate_server2")
    # 强制都只返回同名工具 duplicate_tool

    def always_duplicate_tool(_, message_handler=None):
        return MockMCPClient([create_mock_tool("duplicate_tool")], message_handler=message_handler)

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", always_duplicate_tool)
    manager._servers_config = {config1.name: config1, config2.name: config2}
    manager._active_clients = {config1.name: always_duplicate_tool(config1), config2.name: always_duplicate_tool(config2)}
    with pytest.raises(ToolNameDuplicatedError):
        await manager._arefresh_tool_mapping()


@pytest.mark.asyncio
async def test_astart_client(manager, monkeypatch):
    """
    测试：多个服务器存在同名工具时抛出 ToolNameDuplicatedError。
    Test: Raise ToolNameDuplicatedError when duplicate tool name exists across servers.
    """
    # 两个 server 都返回同名工具 duplicate_tool
    config1 = create_server_config("duplicate_server1")
    config2 = create_server_config("duplicate_server2")
    # 强制都只返回同名工具 duplicate_tool

    def always_duplicate_tool(_, message_handler=None):
        return MockMCPClient([create_mock_tool("duplicate_tool")], message_handler=message_handler)

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", always_duplicate_tool)
    await manager.aadd_or_aupdate_server(config1)
    await manager.aadd_or_aupdate_server(config2)
    assert not manager._active_clients
    await manager.astart_client(config1.name)
    assert manager._active_clients
    assert len(manager._active_clients) == 1
    with pytest.raises(ToolNameDuplicatedError):
        await manager.astart_client(config2.name)
    await manager.astop_client(config1.name)
    assert not manager._active_clients
    await manager.astart_client(config2.name)
    assert manager._active_clients
    assert len(manager._active_clients) == 1


# 覆盖 aremove_server 的删除不存在服务器分支
# Test aremove_server deleting non-existent server
@pytest.mark.asyncio
async def test_aremove_server_not_exist(manager):
    """
    测试：移除不存在的服务器配置应抛出 KeyError。
    Test: Raise KeyError when removing a non-existent server config.
    """
    with pytest.raises(KeyError):
        await manager.aremove_server("not_exist")


@pytest.mark.asyncio
async def test_astart_all_tool_name_duplicate(manager, monkeypatch):
    """
    覆盖 astart_all 的工具名重复异常分支。
    Cover the duplicate tool name exception branch in astart_all.
    """
    # 确保 manager 状态干净
    await manager.aclose()
    config1 = create_server_config("dup_server1")
    config2 = create_server_config("dup_server2")

    # 两个 server 都返回同名工具 duplicate_tool
    def always_duplicate_tool(_, message_handler=None):
        return MockMCPClient([create_mock_tool("duplicate_tool")], message_handler=message_handler)

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", always_duplicate_tool)
    servers = [config1, config2]
    # 因为首次初始化时配置未连接，也就不会检查工具名冲突，因此可以正常初始化
    await manager.ainitialize(servers)
    with pytest.raises(ToolNameDuplicatedError):
        await manager.astart_all()


@pytest.mark.asyncio
async def test_aadd_or_aupdate_server_with_duplicate_tool(manager, monkeypatch):
    """
    测试：在添加/更新服务器时遇到工具名重复会抛出ToolNameDuplicatedError
    Test: Raise ToolNameDuplicatedError when adding/updating server with duplicate tool name
    """
    # 初始配置
    await manager.enable_auto_connect()
    config1 = create_server_config("server1")
    await manager.ainitialize([config1])
    await manager.astart_all()

    # 模拟工具名重复的情况
    def duplicate_tool_factory(config: MCPServerConfig, message_handler=None) -> Any:
        if config.name == "server1":
            return MockMCPClient([create_mock_tool("tool1")], message_handler=message_handler)
        else:
            return MockMCPClient([create_mock_tool("tool1")], message_handler=message_handler)  # 故意返回同名工具

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", duplicate_tool_factory)

    # 添加新服务器（会触发工具名冲突）
    config2 = create_server_config("server2")
    with pytest.raises(ToolNameDuplicatedError):
        await manager.aadd_or_aupdate_server(config2)

    # 验证状态回滚
    assert "server2" not in manager._active_clients
    assert "server2" not in manager._servers_config  # 因为异常导致回滚

    # 更新现有服务器（会触发工具名冲突）
    config1_updated = create_server_config("server1", tool_meta={"tool1": ToolMeta(alias="new_alias")})
    await manager.aadd_or_aupdate_server(config1_updated)
    await manager.aadd_or_aupdate_server(config2)

    # 验证服务器仍然保持原状
    assert manager._active_clients["server1"].list_tools.return_value[0].name == "tool1"
    tools_list = [tool async for tool in manager.available_tools()]
    assert any(tool.name == "tool1" and tool.meta["a2c_tool_meta"].alias == "new_alias" for tool in tools_list) and any(
        tool.name == "tool1" and not tool.meta for tool in tools_list
    )


@pytest.mark.asyncio
async def test_acall_tool_with_vrl_context_injection(manager, monkeypatch):
    """
    测试：acall_tool在VRL转换时注入tool_name和parameters
    Test: acall_tool injects tool_name and parameters during VRL transformation
    """
    import json

    from mcp.types import TextContent

    from a2c_smcp.computer.mcp_clients.model import A2C_VRL_TRANSFORMED

    # 中文: 创建带VRL脚本的配置，脚本会提取tool_name和parameters
    # English: Create config with VRL script that extracts tool_name and parameters
    vrl_script = """
    .context = {
        "tool": .tool_name,
        "params": .parameters
    }
    """

    # 中文: 创建mock客户端，返回带内容的结果
    # English: Create mock client that returns result with content
    def vrl_test_factory(config: MCPServerConfig, message_handler=None) -> Any:
        mock_result = CallToolResult(
            content=[TextContent(text="test result", type="text")],
            isError=False,
        )
        client = MockMCPClient([create_mock_tool("test_tool")], message_handler=message_handler)
        client.call_tool = AsyncMock(return_value=mock_result)
        return client

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", vrl_test_factory)

    # 中文: 创建配置并初始化
    # English: Create config and initialize
    config = StdioServerConfig(
        name="vrl_test_server",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=vrl_script,
        server_parameters=MagicMock(spec=StdioServerParameters),
    )

    await manager.enable_auto_connect()
    await manager.ainitialize([config])
    await manager.astart_all()

    # 中文: 调用工具，传入参数
    # English: Call tool with parameters
    test_params = {"query": "test query", "limit": 10}
    result = await manager.acall_tool("vrl_test_server", "test_tool", test_params)

    # 中文: 验证VRL转换结果包含tool_name和parameters
    # English: Verify VRL transformation result contains tool_name and parameters
    assert result.meta is not None
    assert A2C_VRL_TRANSFORMED in result.meta

    transformed = json.loads(result.meta[A2C_VRL_TRANSFORMED])
    assert "context" in transformed
    assert transformed["context"]["tool"] == "test_tool"
    assert transformed["context"]["params"] == test_params


@pytest.mark.asyncio
async def test_acall_tool_vrl_conditional_on_tool_name(manager, monkeypatch):
    """
    测试：VRL脚本基于tool_name执行条件逻辑
    Test: VRL script executes conditional logic based on tool_name
    """
    import json

    from mcp.types import TextContent

    from a2c_smcp.computer.mcp_clients.model import A2C_VRL_TRANSFORMED

    # 中文: VRL脚本根据tool_name设置不同的result_type
    # English: VRL script sets different result_type based on tool_name
    vrl_script = """
    if .tool_name == "search" {
        .result_type = "search_result"
        .query = .parameters.query
    } else if .tool_name == "execute" {
        .result_type = "execution_result"
        .command = .parameters.cmd
    } else {
        .result_type = "unknown"
    }
    """

    def conditional_vrl_factory(config: MCPServerConfig, message_handler=None) -> Any:
        mock_result = CallToolResult(
            content=[TextContent(text="result", type="text")],
            isError=False,
        )
        client = MockMCPClient(
            [create_mock_tool("search"), create_mock_tool("execute"), create_mock_tool("other")],
            message_handler=message_handler,
        )
        client.call_tool = AsyncMock(return_value=mock_result)
        return client

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", conditional_vrl_factory)

    config = StdioServerConfig(
        name="conditional_server",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=vrl_script,
        server_parameters=MagicMock(spec=StdioServerParameters),
    )

    await manager.enable_auto_connect()
    await manager.ainitialize([config])
    await manager.astart_all()

    # 中文: 测试search工具
    # English: Test search tool
    result1 = await manager.acall_tool("conditional_server", "search", {"query": "test"})
    transformed1 = json.loads(result1.meta[A2C_VRL_TRANSFORMED])
    assert transformed1["result_type"] == "search_result"
    assert transformed1["query"] == "test"

    # 中文: 测试execute工具
    # English: Test execute tool
    result2 = await manager.acall_tool("conditional_server", "execute", {"cmd": "ls"})
    transformed2 = json.loads(result2.meta[A2C_VRL_TRANSFORMED])
    assert transformed2["result_type"] == "execution_result"
    assert transformed2["command"] == "ls"

    # 中文: 测试其他工具
    # English: Test other tool
    result3 = await manager.acall_tool("conditional_server", "other", {})
    transformed3 = json.loads(result3.meta[A2C_VRL_TRANSFORMED])
    assert transformed3["result_type"] == "unknown"


@pytest.mark.asyncio
async def test_acall_tool_vrl_with_nested_parameters(manager, monkeypatch):
    """
    测试：VRL脚本访问嵌套的parameters字段
    Test: VRL script accesses nested parameters fields
    """
    import json

    from mcp.types import TextContent

    from a2c_smcp.computer.mcp_clients.model import A2C_VRL_TRANSFORMED

    # 中文: VRL脚本访问嵌套的parameters
    # English: VRL script accesses nested parameters
    vrl_script = """
    .user_info = {
        "id": .parameters.user.id,
        "name": .parameters.user.name
    }
    .options = .parameters.options
    """

    def nested_params_factory(config: MCPServerConfig, message_handler=None) -> Any:
        mock_result = CallToolResult(
            content=[TextContent(text="result", type="text")],
            isError=False,
        )
        client = MockMCPClient([create_mock_tool("nested_tool")], message_handler=message_handler)
        client.call_tool = AsyncMock(return_value=mock_result)
        return client

    monkeypatch.setattr("a2c_smcp.computer.mcp_clients.manager.client_factory", nested_params_factory)

    config = StdioServerConfig(
        name="nested_server",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=vrl_script,
        server_parameters=MagicMock(spec=StdioServerParameters),
    )

    await manager.enable_auto_connect()
    await manager.ainitialize([config])
    await manager.astart_all()

    # 中文: 调用工具，传入嵌套参数
    # English: Call tool with nested parameters
    nested_params = {"user": {"id": 123, "name": "Alice"}, "options": {"enabled": True, "timeout": 30}}

    result = await manager.acall_tool("nested_server", "nested_tool", nested_params)
    transformed = json.loads(result.meta[A2C_VRL_TRANSFORMED])

    assert transformed["user_info"]["id"] == 123
    assert transformed["user_info"]["name"] == "Alice"
    assert transformed["options"]["enabled"] is True
    assert transformed["options"]["timeout"] == 30
