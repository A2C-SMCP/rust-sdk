"""
中文: 发送 listChanged 通知的最小 MCP Stdio 服务器，用于集成测试。
英文: Minimal MCP Stdio server that sends listChanged notifications for integration tests.

文件名: notifications_stdio_server.py
作者: JQQ
创建日期: 2025/9/28
最后修改日期: 2025/9/28
版权: 2023 JQQ. All rights reserved.
依赖: anyio, mcp
描述: 提供触发工具/资源/提示列表变更通知的能力，并包含基础 list_* 与 call_tool 实现。
"""

import anyio
import mcp.types as types
from mcp.server.lowlevel.server import NotificationOptions, Server
from mcp.server.stdio import stdio_server


async def run() -> None:
    """
    中文: 启动服务器，注册基础 list_* 与一个触发变更通知的工具。
    英文: Start server, register basic list_* and a tool that triggers change notifications.
    """
    server = Server(name="notify-itest-server", version="0.0.1", instructions="itest-notify")

    @server.list_tools()
    async def handle_list_tools() -> list[types.Tool]:
        return [
            types.Tool(
                name="trigger_changes",
                description="Trigger tools/resources/prompts listChanged notifications",
                inputSchema={"type": "object", "properties": {}},
            ),
        ]

    @server.list_resources()
    async def handle_list_resources() -> list[types.Resource]:
        return []

    @server.list_prompts()
    async def handle_list_prompts() -> list[types.Prompt]:
        return []

    @server.call_tool()
    async def handle_call_tool(name: str, arguments: dict | None):
        ctx = server.request_context
        if name == "trigger_changes":
            # 依次发送列表变更通知 / send list-changed notifications
            await ctx.session.send_tool_list_changed()
            await ctx.session.send_resource_list_changed()
            await ctx.session.send_prompt_list_changed()
            return [types.TextContent(type="text", text="changes triggered")]
        return [types.TextContent(type="text", text=f"unknown tool: {name}")]

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options(
            notification_options=NotificationOptions(prompts_changed=True, resources_changed=True, tools_changed=True),
        )
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
