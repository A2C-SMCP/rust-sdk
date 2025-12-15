# -*- coding: utf-8 -*-
# filename: test_manager.py
# @Time    : 2025/11/21 12:19
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

"""
MCP Manager E2E测试 / MCP Manager E2E Tests

测试真实MCP服务器与VRL转换的集成功能。
Tests integration with real MCP servers and VRL transformation.
"""

import json

import pytest
from mcp import StdioServerParameters

from a2c_smcp.computer.mcp_clients.manager import MCPServerManager
from a2c_smcp.computer.mcp_clients.model import A2C_VRL_TRANSFORMED, StdioServerConfig, ToolMeta


@pytest.mark.asyncio
async def test_playwright_browser_navigate_with_vrl():
    """
    中文: 测试使用Playwright MCP服务的browser_navigate工具打开百度首页，并通过VRL重构返回结果
    English: Test using Playwright MCP's browser_navigate tool to open Baidu homepage with VRL transformation
    """
    # 中文: 定义VRL脚本，将返回结果重构为指定的JSON结构
    # English: Define VRL script to restructure the result into specified JSON format
    vrl_script = """
# 中文: 只对 browser_navigate 和 browser_navigate_back 工具进行转换
# English: Only transform for browser_navigate and browser_navigate_back tools
if .tool_name == "browser_navigate" || .tool_name == "browser_navigate_back" {
    # 中文: 提取URL，browser_navigate_back 可能没有url参数
    # English: Extract URL, browser_navigate_back may not have url parameter
    url = if exists(.parameters.url) {
        .parameters.url
    } else {
        "[BROWSER_BACK_OPERATION]"
    }

    # 中文: 提取内容 / English: Extract content
    content = if length!(.content) > 0 {
        .content[0].text
    } else {
        ""
    }

    # 中文: 使用重新赋值方式，只保留需要的字段
    # English: Use reassignment to keep only needed fields
    . = {
        "url": url,
        "content": content
    }
}
"""

    # 中文: 创建Playwright MCP服务配置
    # English: Create Playwright MCP server configuration
    playwright_config = StdioServerConfig(
        name="playwright",
        type="stdio",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        default_tool_meta=ToolMeta(**{
            "tags": ["browser"],
            "auto_apply": True,
        }),
        vrl=vrl_script,
        server_parameters=StdioServerParameters(
            command="npx",
            args=["@playwright/mcp@latest"],
            env=None,
        ),
    )

    # 中文: 初始化Manager并添加服务器配置
    # English: Initialize Manager and add server configuration
    manager = MCPServerManager(auto_connect=True)
    await manager.aadd_or_aupdate_server(playwright_config)

    try:
        # 中文: 调用browser_navigate工具打开百度首页
        # English: Call browser_navigate tool to open Baidu homepage
        baidu_url = "https://www.baidu.com"
        result = await manager.acall_tool(
            server_name="playwright",
            tool_name="browser_navigate",
            parameters={"url": baidu_url},
            timeout=30.0,  # 中文: 浏览器操作可能需要较长时间 / English: Browser operations may take longer
        )

        # 中文: 验证返回结果的meta中包含VRL转换结果
        # English: Verify that meta contains VRL transformation result
        assert result.meta is not None, "Result meta should not be None"
        assert A2C_VRL_TRANSFORMED in result.meta, f"Result meta should contain {A2C_VRL_TRANSFORMED}"

        # 中文: 解析VRL转换后的JSON结构
        # English: Parse VRL transformed JSON structure
        transformed_json = json.loads(result.meta[A2C_VRL_TRANSFORMED])

        # 中文: 验证转换后的结构符合要求
        # English: Verify transformed structure meets requirements
        assert "url" in transformed_json, "Transformed result should contain 'url' field"
        assert "content" in transformed_json, "Transformed result should contain 'content' field"

        # 中文: 验证URL字段值正确
        # English: Verify URL field value is correct
        assert transformed_json["url"] == baidu_url, f"URL should be {baidu_url}"

        # 中文: 验证content字段存在（内容可能为空或包含页面信息）
        # English: Verify content field exists (content may be empty or contain page info)
        assert isinstance(transformed_json["content"], str), "Content should be a string"

        # 中文: 验证只包含url和content两个字段（VRL脚本删除了其他字段）
        # English: Verify only url and content fields exist (VRL script deleted other fields)
        assert set(transformed_json.keys()) == {
            "url",
            "content",
        }, "Transformed result should only contain 'url' and 'content' fields"

        # 中文: 打印转换结果用于调试
        # English: Print transformed result for debugging
        print(f"\n转换后的结果 / Transformed result:\n{json.dumps(transformed_json, indent=2, ensure_ascii=False)}")

    finally:
        # 中文: 清理：停止所有客户端
        # English: Cleanup: stop all clients
        await manager.astop_all()
