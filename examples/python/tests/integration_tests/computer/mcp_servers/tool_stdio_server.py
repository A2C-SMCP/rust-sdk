"""
中文: 带有简单工具的最小 MCP Stdio 服务器，用于集成测试。
英文: Minimal MCP Stdio server with a simple tool for integration tests.

文件名: tool_stdio_server.py / filename: tool_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/9/28 / created at: 2025/9/28
最后修改日期: 2025/9/28 / last modified at: 2025/9/28
版权: 2023 JQQ. All rights reserved. / copyright: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 提供一个 echo 工具用于测试 list_tools 与 call_tool。/ description: Provides an echo tool for testing list_tools and call_tool.
"""

import anyio
import mcp.types as types
from mcp.server.lowlevel.server import Server
from mcp.server.stdio import stdio_server


async def run() -> None:
    """
    中文: 启动服务器，注册一个 echo 工具，回显输入文本。
    英文: Start the server and register an echo tool returning the input text.
    """
    server = Server(name="tool-itest-server", version="0.0.1", instructions="itest-tools")

    # 注册 list_tools 与 call_tool 处理器 / register handlers
    @server.list_tools()
    async def handle_list_tools() -> list[types.Tool]:
        return [
            types.Tool(
                name="echo",
                description="Echo back the provided text",
                inputSchema={
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                },
            ),
        ]

    @server.call_tool()
    async def handle_call_tool(name: str, arguments: dict | None):
        if name != "echo":
            return [types.TextContent(type="text", text=f"unknown tool: {name}")]
        text = (arguments or {}).get("text", "")
        return [types.TextContent(type="text", text=f"echo: {text}")]

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
