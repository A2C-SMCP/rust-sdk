# -*- coding: utf-8 -*-
# filename: tools_paged_stdio_server.py
# @Time    : 2025/10/02 22:40
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文: 提供带分页的工具列表的 MCP Stdio 服务器，用于覆盖 BaseMCPClient.list_tools 的分页路径。
英文: MCP Stdio server with paginated tools list to cover BaseMCPClient.list_tools pagination path.

文件名: tools_paged_stdio_server.py / filename: tools_paged_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/10/02 / created at: 2025/10/02
最后修改日期: 2025/10/02 / last modified at: 2025/10/02
版权: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 第一页返回一个工具并携带 nextCursor='page2'；第二页返回另一工具。
"""

from __future__ import annotations

import anyio
from mcp import types
from mcp.server.lowlevel.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import ListToolsRequest


async def run() -> None:
    server = Server(name="itest-tools-paged", version="0.0.1", instructions="itest-tools")

    @server.list_tools()
    async def list_tools(req: ListToolsRequest):  # type: ignore[override]
        # 返回分页结果 / return paginated result
        if req and req.params and req.params.cursor:
            # 第二页 / second page
            return types.ListToolsResult(
                tools=[
                    types.Tool(
                        name="page2_tool",
                        description="中文: 第2页工具; 英文: page2 tool",
                        inputSchema={"type": "object", "properties": {}},
                    ),
                ],
                nextCursor=None,
            )
        return types.ListToolsResult(
            tools=[
                types.Tool(
                    name="page1_tool",
                    description="中文: 第1页工具; 英文: page1 tool",
                    inputSchema={"type": "object", "properties": {}},
                ),
            ],
            nextCursor="page2",
        )

    @server.call_tool()
    async def call_tool(name: str, arguments: dict | None):  # noqa: ARG001
        return [types.TextContent(type="text", text=f"ok:{name}")]

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
