# -*- coding: utf-8 -*-
# filename: test_param_server.py
# 集成测试：验证自定义参数化 stdio MCP Server 的动态配置更新能力
from __future__ import annotations

import sys
from pathlib import Path

import pytest
from mcp import StdioServerParameters

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import StdioServerConfig


@pytest.mark.asyncio
async def test_param_server_dynamic_update() -> None:
    """
    1) 启动带 GREETING 环境变量的 param_server（Hello）
    2) 调用工具 config_value -> 期望 "Hello, World!"
    3) 动态更新配置，将 GREETING 修改为 "Hi"，启用自动重连
    4) 再次调用 -> 期望 "Hi, World!"
    """
    server_script = Path(__file__).with_name("param_server.py")

    params = StdioServerParameters(
        command=sys.executable,
        args=[str(server_script)],
        env={"GREETING": "Hello"},
        cwd=None,
        encoding="utf-8",
        encoding_error_handler="strict",
    )
    cfg = StdioServerConfig(name="param_server", server_parameters=params)

    comp = Computer(name="test", inputs=set(), mcp_servers={cfg}, auto_connect=False, auto_reconnect=True)
    async with comp:
        # 启动该 server
        await comp.mcp_manager.astart_client("param_server")

        # 第一次调用
        result1 = await comp.mcp_manager.acall_tool("param_server", "config_value", {})
        assert not result1.isError
        assert result1.content and result1.content[0].type == "text"
        assert result1.content[0].text == "Hello, World!"

        # 更新配置 GREETING -> Hi
        new_params = StdioServerParameters(
            command=sys.executable,
            args=[str(server_script)],
            env={"GREETING": "Hi"},
            cwd=None,
            encoding="utf-8",
            encoding_error_handler="strict",
        )
        new_cfg = StdioServerConfig(name="param_server", server_parameters=new_params)
        await comp.aadd_or_aupdate_server(new_cfg)

        # 再次调用，验证已生效
        result2 = await comp.mcp_manager.acall_tool("param_server", "config_value", {})
        assert not result2.isError
        assert result2.content and result2.content[0].type == "text"
        assert result2.content[0].text == "Hi, World!"
