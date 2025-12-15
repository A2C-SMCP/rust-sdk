"""
中文: 最小可运行的 MCP Stdio 服务器，用于集成测试。
英文: Minimal runnable MCP Stdio server for integration tests.

文件名: minimal_stdio_server.py / filename: minimal_stdio_server.py
作者: JQQ / author: JQQ
创建日期: 2025/9/28 / created at: 2025/9/28
最后修改日期: 2025/9/28 / last modified at: 2025/9/28
版权: 2023 JQQ. All rights reserved. / copyright: 2023 JQQ. All rights reserved.
依赖: anyio, mcp / dependencies: anyio, mcp
描述: 启动一个最小的基于 stdio 的 MCP 服务器，仅用于握手与初始化测试。/ description: Start a minimal stdio-based MCP server
    for handshake/initialize tests.
"""

import anyio
from mcp.server.lowlevel.server import Server
from mcp.server.stdio import stdio_server


async def run() -> None:
    """
    中文: 启动服务器，使用默认能力，不注册任何工具/资源，仅支持初始化。
    英文: Run the server with default capabilities, no tools/resources, only initialization support.
    """
    server = Server(name="itest-server", version="0.0.1", instructions="itest")

    async with stdio_server() as (read_stream, write_stream):
        init_opts = server.create_initialization_options()
        await server.run(read_stream, write_stream, init_opts)


if __name__ == "__main__":
    anyio.run(run)
