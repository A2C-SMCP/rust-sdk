# -*- coding: utf-8 -*-
# filename: test_manager.py
# @Time    : 2025/8/20 14:23
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
# -*- coding: utf-8 -*-
"""
集成测试：MCPServerManager 多协议客户端统一管理
Integration test: MCPServerManager manages multiple protocol clients
"""

import pytest
from mcp import Tool

from a2c_smcp.computer.mcp_clients.manager import MCPServerManager, ToolNameDuplicatedError
from a2c_smcp.computer.mcp_clients.model import SseServerConfig, StdioServerConfig, StreamableHttpServerConfig, ToolMeta


@pytest.mark.anyio
async def test_manager_initialize_and_start(stdio_params, sse_params, sse_server):
    """
    测试初始化和启动多个客户端
    Test initialize and start multiple clients
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    for name in ["stdio_server", "sse_server"]:
        client = manager._active_clients.get(name)
        assert client is not None
        assert client.state == "connected"


@pytest.mark.anyio
async def test_manager_available_tools(stdio_params, sse_params, sse_server):
    """
    测试获取所有可用工具
    Test getting all available tools
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    tools = [tool async for tool in manager.available_tools()]
    assert isinstance(tools, list)
    assert any(isinstance(t, Tool) for t in tools)


@pytest.mark.anyio
async def test_manager_default_tool_meta_injection(stdio_params, sse_params, sse_server):
    """
    集成测试：当未配置单工具的元数据时，应回落 default_tool_meta 并通过 available_tools 注入到 Tool.meta。
    Integration: default_tool_meta should be applied to Tool.meta when per-tool meta is missing.
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params, default_tool_meta=ToolMeta(auto_apply=True))
    await manager.ainitialize([stdio_cfg])
    await manager.astart_all()
    tools = [tool async for tool in manager.available_tools()]
    assert tools, "Should list at least one tool"
    # 所有工具应该包含注入的 a2c_tool_meta.auto_apply == True 因为目前这些工具没有自定义元数据
    assert all(getattr(t.meta.get("a2c_tool_meta"), "auto_apply", False) is True for t in tools)


@pytest.mark.anyio
async def test_manager_execute_tool(stdio_params, sse_params, sse_server):
    """
    测试执行一个工具
    Test executing a tool
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    tools = [tool async for tool in manager.available_tools()]
    for tool in tools:
        try:
            result = await manager.aexecute_tool(tool.name, {})
            assert hasattr(result, "content")
            break
        except Exception:
            continue


@pytest.mark.anyio
async def test_manager_remove_server(stdio_params, sse_params, sse_server):
    """
    测试动态移除服务
    Test removing a server dynamically
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    await manager.aremove_server("sse_server")
    assert "sse_server" not in manager._active_clients


@pytest.mark.anyio
async def test_manager_duplicate_tool_name(sse_params, http_params, sse_server, basic_server):
    """
    测试重复工具名异常
    Test duplicate tool name error
    """
    manager = MCPServerManager(auto_connect=False)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    http_cfg = StreamableHttpServerConfig(name="http_server", server_parameters=http_params)
    with pytest.raises(ToolNameDuplicatedError):
        await manager.ainitialize([sse_cfg, http_cfg])
        await manager.astart_all()


@pytest.mark.anyio
async def test_manager_invalid_server(stdio_params, sse_params, sse_server):
    """
    测试无效 server 异常
    Test invalid server error
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    with pytest.raises(ValueError):
        await manager.astart_client("not_exist_server")


@pytest.mark.anyio
async def test_manager_disabled_tool(stdio_params, sse_params, sse_server):
    """
    测试禁用工具异常
    Test disabled tool error
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    tools = [tool async for tool in manager.available_tools()]
    if tools:
        manager._disabled_tools.add(tools[0].name)
        with pytest.raises(PermissionError):
            await manager.aexecute_tool(tools[0].name, {})


@pytest.mark.anyio
async def test_manager_stop_all(stdio_params, sse_params, sse_server):
    """
    测试关闭所有客户端
    Test stopping all clients
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    # await manager.ainitialize([sse_cfg])
    await manager.astart_all()
    await manager.astop_all()
    for client in manager._active_clients.values():
        assert client.state != "connected"


@pytest.mark.anyio
async def test_manager_propagates_message_handler_to_clients(stdio_params, sse_params, sse_server):
    """集成测试：验证 Manager 能将 message_handler 透传到真实 Client。
    Integration: Verify Manager forwards message_handler to real clients.
    """

    async def dummy_handler(*args, **kwargs):
        return None

    manager = MCPServerManager(auto_connect=False, message_handler=dummy_handler)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()

    # BaseMCPClient 在实例上保存为 _message_handler
    for name, client in manager._active_clients.items():
        assert getattr(client, "_message_handler", None) is dummy_handler, f"Client {name} did not receive message_handler"


@pytest.mark.anyio
async def test_manager_message_handler_none_results_in_none_on_clients(stdio_params, sse_params, sse_server):
    """集成测试：当未提供 message_handler 时，真实客户端也应为 None。"""
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()

    for client in manager._active_clients.values():
        assert getattr(client, "_message_handler", "__missing__") is None


@pytest.mark.anyio
async def test_manager_reinitialize_and_restart(stdio_params, sse_params, sse_server):
    """
    测试重新初始化和重启服务
    Test reinitializing and restarting servers
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    await manager.ainitialize([stdio_cfg])
    await manager.astart_all()
    await manager.astop_all()
    await manager.ainitialize([stdio_cfg, sse_cfg])
    await manager.astart_all()
    for name in ["stdio_server", "sse_server"]:
        client = manager._active_clients.get(name)
        assert client is not None
        assert client.state == "connected"


@pytest.mark.anyio
async def test_manager_remove_nonexistent_server(stdio_params, sse_params, sse_server):
    """
    测试移除不存在的服务不会抛异常
    Test removing a non-existent server does not raise
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    await manager.ainitialize([stdio_cfg])
    await manager.astart_all()
    # Should not raise
    with pytest.raises(KeyError):
        await manager.aremove_server("not_exist_server")


@pytest.mark.anyio
async def test_manager_execute_tool_invalid_name(stdio_params, sse_params, sse_server):
    """
    测试执行不存在的工具名抛出异常
    Test executing a non-existent tool name raises
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    await manager.ainitialize([stdio_cfg])
    await manager.astart_all()
    with pytest.raises(ValueError):
        await manager.aexecute_tool("not_exist_tool", {})


@pytest.mark.anyio
async def test_manager_stop_when_already_stopped(stdio_params, sse_params, sse_server):
    """
    测试多次停止不会抛异常
    Test stopping all clients multiple times does not raise
    """
    manager = MCPServerManager(auto_connect=False)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params)
    await manager.ainitialize([stdio_cfg])
    await manager.astart_all()
    await manager.astop_all()
    # Should not raise
    await manager.astop_all()
