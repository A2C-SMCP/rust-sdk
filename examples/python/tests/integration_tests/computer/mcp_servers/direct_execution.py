# -*- coding: utf-8 -*-
# filename: direct_execution.py
# @Time    : 2025/8/19 11:11
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""Copy from https://github.com/modelcontextprotocol/python-sdk/blob/main/examples/snippets/servers/direct_execution.py
Example showing direct execution of an MCP server.

This is the simplest way to run an MCP server directly.
cd to the `examples/snippets` directory and run:
    python servers/direct_execution.py

注意此进程启动后，如果要关闭，需要使用 Ctrl+D 而不是 Ctrl+C。本质上是通过 Ctrl+D 退出Stdin。从而达到关闭进程的目的
这个效果是MCP Server封装的默认行为。
"""

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("My App")


@mcp.tool()
def hello(name: str = "World") -> str:
    """Say hello to someone."""
    return f"Hello, {name}!"


def main():
    """Entry point for the direct execution server."""
    print("starting...")
    mcp.run()


if __name__ == "__main__":
    main()
