# -*- coding: utf-8 -*-
# filename: test_model.py
# @Time    : 2025/8/18 13:35
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

import pytest
from mcp import StdioServerParameters
from mcp.client.session_group import SseServerParameters, StreamableHttpParameters
from polyfactory.factories.pydantic_factory import ModelFactory

from a2c_smcp.computer.mcp_clients.model import (
    BaseMCPServerConfig,
    MCPServerConfig,
    SseServerConfig,
    StdioServerConfig,
    StreamableHttpServerConfig,
    ToolMeta,
)


# ----------- 工厂类生成测试数据 -----------
class ToolMetaFactory(ModelFactory):
    __model__ = ToolMeta

    @classmethod
    def auto_apply(cls) -> bool | None:
        return cls.__random__.choice([True, False, None])

    @classmethod
    def alias(cls) -> str | None:
        return cls.__random__.choice([None, f"alias_{cls.__random__.randint(1, 100)}"])


class BaseMCPServerConfigFactory(ModelFactory):
    __model__ = BaseMCPServerConfig
    __random_seed__ = 42  # 固定随机种子保证可重复性

    @classmethod
    def name(cls) -> str:
        return f"server_{cls.__random__.randint(1, 100)}"

    @classmethod
    def disabled(cls) -> bool:
        return cls.__random__.choice([True, False])

    @classmethod
    def forbidden_tools(cls) -> list:
        return [f"tool_{i}" for i in range(cls.__random__.randint(0, 5))]

    @classmethod
    def tool_meta(cls) -> dict:
        meta = {}
        tools = ToolMetaFactory.batch(size=cls.__random__.randint(1, 3))
        for index, tool_meta in enumerate(tools):
            meta[f"tool_{index}"] = tool_meta
        return meta

    @classmethod
    def vrl(cls) -> str | None:
        """生成有效的VRL脚本或None / Generate valid VRL script or None"""
        # 随机返回None或一个简单的有效VRL脚本
        # Randomly return None or a simple valid VRL script
        return cls.__random__.choice([
            None,
            '.result = "success"',
            ".transformed = true",
            '.status = "ok"',
        ])


# 使用模拟参数创建子类工厂
class StdioServerConfigFactory(BaseMCPServerConfigFactory):
    __model__ = StdioServerConfig

    @classmethod
    def server_parameters(cls) -> StdioServerParameters:
        return ModelFactory.create_factory(StdioServerParameters).build()


class SseServerConfigFactory(BaseMCPServerConfigFactory):
    __model__ = SseServerConfig

    @classmethod
    def server_parameters(cls) -> SseServerParameters:
        return ModelFactory.create_factory(SseServerParameters).build()


class StreamableHttpServerConfigFactory(BaseMCPServerConfigFactory):
    __model__ = StreamableHttpServerConfig

    @classmethod
    def server_parameters(cls) -> StreamableHttpParameters:
        return ModelFactory.create_factory(StreamableHttpParameters).build()


# ----------- 测试用例 -----------
@pytest.mark.parametrize(
    "factory",
    [ToolMetaFactory, BaseMCPServerConfigFactory, StdioServerConfigFactory, SseServerConfigFactory, StreamableHttpServerConfigFactory],
)
def test_model_creation(factory):
    """测试模型实例是否能正确创建"""
    instance = factory.build()
    assert isinstance(instance, factory.__model__)


def test_tool_meta_extra_fields():
    """测试ToolMeta的extra字段功能"""
    data = {"auto_apply": True, "extra_field": "value", "another_field": 42}
    tool_meta = ToolMeta(**data)

    # 验证额外字段被保留
    assert tool_meta.extra_field == "value"
    assert tool_meta.another_field == 42


def test_server_config_inheritance():
    """测试服务器配置类的继承关系"""
    stdio_config = StdioServerConfigFactory.build()
    assert isinstance(stdio_config, BaseMCPServerConfig)
    assert isinstance(stdio_config.server_parameters, StdioServerParameters)

    sse_config = SseServerConfigFactory.build()
    assert isinstance(sse_config, BaseMCPServerConfig)
    assert isinstance(sse_config.server_parameters, SseServerParameters)

    http_config = StreamableHttpServerConfigFactory.build()
    assert isinstance(http_config, BaseMCPServerConfig)
    assert isinstance(http_config.server_parameters, StreamableHttpParameters)


def test_mcp_config_union():
    """测试MCPServerConfig类型别名接受所有子类型"""
    servers: list[MCPServerConfig] = [
        StdioServerConfigFactory.build(),
        SseServerConfigFactory.build(),
        StreamableHttpServerConfigFactory.build(),
    ]

    assert len(servers) == 3
    assert all(isinstance(s, (StdioServerConfig, SseServerConfig, StreamableHttpServerConfig)) for s in servers)


@pytest.mark.parametrize(
    "field, value",
    [
        ("name", 123),  # 错误类型
        ("disabled", "Test"),  # 类型不匹配
        ("forbidden_tools", [1, 2, 3]),  # 列表项类型错误
        ("tool_meta", {"key": "invalid"}),  # 值类型错误
    ],
)
def test_validation_errors(field, value):
    """测试基础模型的验证错误"""
    data = {"name": "valid_name", "disabled": False, "forbidden_tools": [], "tool_meta": {}, field: value}

    with pytest.raises(ValueError):
        BaseMCPServerConfig(**data)


def test_tool_meta_ret_mapper():
    """测试ToolMeta中ret_object_mapper的特殊处理"""
    mapper = {"field": "mapped_field", "nested": {"key": "value"}}
    tool_meta = ToolMeta(alias=None, auto_apply=None, ret_object_mapper=mapper)
    assert tool_meta.ret_object_mapper == mapper


def test_server_config_with_default_tool_meta_only():
    """测试仅设置 default_tool_meta 时模型可创建并持有该字段"""
    cfg = StdioServerConfig(
        name="server_default_only",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        default_tool_meta=ToolMeta(auto_apply=True, alias=None),
        server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
    )
    assert isinstance(cfg.default_tool_meta, ToolMeta)
    assert cfg.default_tool_meta.auto_apply is True


def test_server_config_tool_meta_overrides_default():
    """测试 tool_meta 的根级字段浅覆盖 default_tool_meta"""
    default = ToolMeta(auto_apply=False, alias=None, ret_object_mapper={"a": 1})
    per_tool = {"t": ToolMeta(auto_apply=True)}
    cfg = SseServerConfig(
        name="server_with_default",
        disabled=False,
        forbidden_tools=[],
        tool_meta=per_tool,
        default_tool_meta=default,
        server_parameters=ModelFactory.create_factory(SseServerParameters).build(),
    )
    # 验证模型层面字段存在
    assert cfg.tool_meta["t"].auto_apply is True
    assert cfg.default_tool_meta.ret_object_mapper == {"a": 1}


# ----------- VRL 验证器测试用例 -----------
def test_vrl_validator_with_valid_script():
    """
    测试VRL验证器：有效的VRL脚本应该通过验证
    Test VRL validator: valid VRL script should pass validation
    """
    valid_vrl = '.result = "success"'
    cfg = StdioServerConfig(
        name="test_vrl_valid",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=valid_vrl,
        server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
    )
    assert cfg.vrl == valid_vrl


def test_vrl_validator_with_invalid_script():
    """
    测试VRL验证器：无效的VRL脚本应该抛出ValueError
    Test VRL validator: invalid VRL script should raise ValueError
    """
    invalid_vrl = "this is not valid vrl syntax!!!"

    with pytest.raises(ValueError, match="VRL语法错误"):
        StdioServerConfig(
            name="test_vrl_invalid",
            disabled=False,
            forbidden_tools=[],
            tool_meta={},
            vrl=invalid_vrl,
            server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
        )


def test_vrl_validator_with_none():
    """
    测试VRL验证器：None值应该被接受
    Test VRL validator: None value should be accepted
    """
    cfg = StdioServerConfig(
        name="test_vrl_none",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=None,
        server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
    )
    assert cfg.vrl is None


def test_vrl_validator_with_empty_string():
    """
    测试VRL验证器：空字符串应该被接受
    Test VRL validator: empty string should be accepted
    """
    cfg = SseServerConfig(
        name="test_vrl_empty",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl="",
        server_parameters=ModelFactory.create_factory(SseServerParameters).build(),
    )
    assert cfg.vrl == ""


def test_vrl_validator_with_whitespace_only():
    """
    测试VRL验证器：仅包含空白字符的字符串应该被接受（视为空）
    Test VRL validator: whitespace-only string should be accepted (treated as empty)
    """
    cfg = StreamableHttpServerConfig(
        name="test_vrl_whitespace",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl="   \n\t  ",
        server_parameters=ModelFactory.create_factory(StreamableHttpParameters).build(),
    )
    assert cfg.vrl == "   \n\t  "


def test_vrl_validator_with_complex_valid_script():
    """
    测试VRL验证器：复杂的有效VRL脚本应该通过验证
    Test VRL validator: complex valid VRL script should pass validation
    """
    complex_vrl = """
    .status = "processed"
    .timestamp = now()
    .transformed = true
    """
    cfg = StdioServerConfig(
        name="test_vrl_complex",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=complex_vrl,
        server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
    )
    assert cfg.vrl == complex_vrl


def test_vrl_validator_with_conditional_logic():
    """
    测试VRL验证器：带条件逻辑的VRL脚本应该通过验证
    Test VRL validator: VRL script with conditional logic should pass validation
    """
    conditional_vrl = """
    if .isError == true {
        .status = "error"
    } else {
        .status = "success"
    }
    """
    cfg = SseServerConfig(
        name="test_vrl_conditional",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=conditional_vrl,
        server_parameters=ModelFactory.create_factory(SseServerParameters).build(),
    )
    assert cfg.vrl == conditional_vrl


def test_vrl_validator_error_message_format():
    """
    测试VRL验证器：验证错误消息格式包含详细的诊断信息
    Test VRL validator: validation error message contains detailed diagnostic info
    """
    invalid_vrl = "undefined_variable_xyz"

    with pytest.raises(ValueError) as exc_info:
        StdioServerConfig(
            name="test_vrl_error_msg",
            disabled=False,
            forbidden_tools=[],
            tool_meta={},
            vrl=invalid_vrl,
            server_parameters=ModelFactory.create_factory(StdioServerParameters).build(),
        )

    error_message = str(exc_info.value)
    # 验证错误消息包含中英文提示
    assert "VRL语法错误" in error_message or "VRL syntax error" in error_message
    # 验证错误消息包含详细诊断信息（VRL Runtime提供的格式化消息）
    assert "undefined variable" in error_message.lower() or "error" in error_message.lower()


@pytest.mark.parametrize(
    "config_class,param_factory",
    [
        (StdioServerConfig, lambda: ModelFactory.create_factory(StdioServerParameters).build()),
        (SseServerConfig, lambda: ModelFactory.create_factory(SseServerParameters).build()),
        (StreamableHttpServerConfig, lambda: ModelFactory.create_factory(StreamableHttpParameters).build()),
    ],
)
def test_vrl_validator_across_all_config_types(config_class, param_factory):
    """
    测试VRL验证器：在所有配置类型中都能正常工作
    Test VRL validator: works correctly across all config types
    """
    valid_vrl = ".validated = true"

    # 测试有效脚本
    cfg_valid = config_class(
        name="test_vrl_all_types_valid",
        disabled=False,
        forbidden_tools=[],
        tool_meta={},
        vrl=valid_vrl,
        server_parameters=param_factory(),
    )
    assert cfg_valid.vrl == valid_vrl

    # 测试无效脚本
    invalid_vrl = "invalid syntax here!!!"
    with pytest.raises(ValueError, match="VRL语法错误"):
        config_class(
            name="test_vrl_all_types_invalid",
            disabled=False,
            forbidden_tools=[],
            tool_meta={},
            vrl=invalid_vrl,
            server_parameters=param_factory(),
        )
