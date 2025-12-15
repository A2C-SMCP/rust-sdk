"""
测试 ToolMeta tags 字段在配置解析时不会丢失
Test that ToolMeta tags field is not lost during config parsing
"""

import json

from pydantic import TypeAdapter

from a2c_smcp.smcp import MCPServerConfig as SMCPServerConfigDict


def test_tool_meta_tags_preserved_in_config_parsing():
    """
    测试通过 TypeAdapter 验证配置时，tags 字段不会丢失
    Test that tags field is preserved when validating config with TypeAdapter
    """
    # 模拟用户通过 CLI 传入的配置
    config_dict = {
        "name": "playwright",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "default_tool_meta": {"tags": ["browser"], "auto_apply": True},
        "server_parameters": {
            "command": "npx",
            "args": ["@playwright/mcp@latest"],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
        "vrl": None,
    }

    # 使用 TypeAdapter 验证配置（模拟 CLI 的行为）
    validated = TypeAdapter(SMCPServerConfigDict).validate_python(config_dict)

    print(f"\nvalidated config: {json.dumps(validated, indent=2, ensure_ascii=False)}")
    print(f"\ndefault_tool_meta: {validated.get('default_tool_meta')}")

    # 验证 tags 字段没有丢失
    assert "default_tool_meta" in validated
    assert validated["default_tool_meta"] is not None
    assert "tags" in validated["default_tool_meta"]
    assert validated["default_tool_meta"]["tags"] == ["browser"]
    assert validated["default_tool_meta"]["auto_apply"] is True


def test_tool_meta_all_fields():
    """
    测试 ToolMeta 的所有字段都能正确解析
    Test that all ToolMeta fields can be correctly parsed
    """
    config_dict = {
        "name": "test_server",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {
            "test_tool": {
                "auto_apply": False,
                "alias": "custom_tool",
                "tags": ["tag1", "tag2"],
                "ret_object_mapper": {"key": "value"},
            },
        },
        "default_tool_meta": {
            "auto_apply": True,
            "alias": None,
            "tags": ["default_tag"],
            "ret_object_mapper": None,
        },
        "server_parameters": {
            "command": "test",
            "args": [],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }

    validated = TypeAdapter(SMCPServerConfigDict).validate_python(config_dict)

    # 验证 tool_meta
    assert "test_tool" in validated["tool_meta"]
    tool_meta = validated["tool_meta"]["test_tool"]
    assert tool_meta["auto_apply"] is False
    assert tool_meta["alias"] == "custom_tool"
    assert tool_meta["tags"] == ["tag1", "tag2"]
    assert tool_meta["ret_object_mapper"] == {"key": "value"}

    # 验证 default_tool_meta
    default_meta = validated["default_tool_meta"]
    assert default_meta["auto_apply"] is True
    assert default_meta["tags"] == ["default_tag"]
