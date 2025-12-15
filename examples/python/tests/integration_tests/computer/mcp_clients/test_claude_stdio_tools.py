# -*- coding: utf-8 -*-
# filename: test_claude_stdio_tools.py
# 目的：连接本机的 Claude Code 以 stdio 方式运行的 MCP Server，列出工具与其入参 Schema
# 运行前提：已安装并可在 PATH 中找到 `claude` 可执行文件

from __future__ import annotations

import json
import shutil

import pytest
from mcp import StdioServerParameters

from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient


@pytest.mark.asyncio
async def test_claude_mcp_tools_list_and_params() -> None:
    """
    中文:
      - 若本机安装了 `claude`，以 stdio 方式启动 `claude mcp serve`
      - 连接后列出所有可用工具及其入参（inputSchema），并打印为 JSON 便于查看
    英文:
      - If `claude` CLI is available, start `claude mcp serve` via stdio
      - Connect and list available tools with their inputSchema, print as JSON
    """
    # 未安装则跳过，避免在 CI 或未配置环境下失败
    if not shutil.which("claude"):
        pytest.skip("`claude` CLI not found in PATH; skip Claude MCP integration test")

    # 如需提高输出上限，可在外部设置: export MAX_MCP_OUTPUT_TOKENS=50000
    params = StdioServerParameters(command="claude", args=["mcp", "serve"])  # env 可按需在此扩展

    client = StdioMCPClient(params)

    # 连接 / connect
    await client.aconnect()
    await client._create_session_success_event.wait()

    # 基础校验 / basic checks
    assert client.initialize_result is not None
    assert client.initialize_result.capabilities.tools, "Claude MCP server should expose tools capability"

    # 获取工具列表 / list tools
    tools = await client.list_tools()
    assert tools, "Claude MCP should expose at least one tool"

    collected: list[dict] = []
    for t in tools:
        # inputSchema 可能是 pydantic BaseModel 或原始 dict
        schema = getattr(t, "inputSchema", None)
        if hasattr(schema, "model_dump"):
            try:
                schema = schema.model_dump(mode="json")
            except Exception:
                # 兜底为可序列化对象
                schema = json.loads(json.dumps(schema, default=str))
        collected.append(
            {
                "name": t.name,
                "description": getattr(t, "description", None),
                "inputSchema": schema,
            },
        )

    # 打印用于人工查看（不作为断言依据） / print for manual inspection
    print(json.dumps(collected, ensure_ascii=False, indent=2))

    # 断开 / disconnect
    await client.adisconnect()
    await client._async_session_closed_event.wait()
