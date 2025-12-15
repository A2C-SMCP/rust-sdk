# -*- coding: utf-8 -*-
# filename: param_server.py
# 一个带自定义参数读取能力的最简 MCP stdio 服务器
from __future__ import annotations

import os

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("Param Server")


def _get_prefix() -> str:
    # 从环境变量读取一个前缀
    return os.environ.get("GREETING", "Hello")


@mcp.tool()
def config_value(name: str | None = None) -> str:
    """返回基于当前配置生成的问候语。Reads current env-based prefix."""
    prefix = _get_prefix()
    who = name or "World"
    return f"{prefix}, {who}!"


def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()
