# -*- coding: utf-8 -*-
# filename: test_utils.py
# @Time    : 2025/8/19 16:58
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
测试 client_factory 工厂方法，确保不同类型配置能正确实例化对应客户端。
Test client_factory factory method to ensure correct client instantiation for each config type.
"""

import pytest
from polyfactory.factories.pydantic_factory import ModelFactory

from a2c_smcp.computer.mcp_clients.http_client import HttpMCPClient
from a2c_smcp.computer.mcp_clients.model import SseServerConfig, StdioServerConfig, StreamableHttpServerConfig
from a2c_smcp.computer.mcp_clients.sse_client import SseMCPClient
from a2c_smcp.computer.mcp_clients.stdio_client import StdioMCPClient
from a2c_smcp.computer.mcp_clients.utils import client_factory


class StdioServerConfigFactory(ModelFactory[StdioServerConfig]):
    __model__ = StdioServerConfig

    @classmethod
    def vrl(cls) -> str | None:
        """生成有效的VRL脚本或None / Generate valid VRL script or None"""
        return cls.__random__.choice([None, '.result = "success"', ".transformed = true"])


class SseServerConfigFactory(ModelFactory[SseServerConfig]):
    __model__ = SseServerConfig

    @classmethod
    def vrl(cls) -> str | None:
        """生成有效的VRL脚本或None / Generate valid VRL script or None"""
        return cls.__random__.choice([None, '.result = "success"', ".transformed = true"])


class StreamableHttpServerConfigFactory(ModelFactory[StreamableHttpServerConfig]):
    __model__ = StreamableHttpServerConfig

    @classmethod
    def vrl(cls) -> str | None:
        """生成有效的VRL脚本或None / Generate valid VRL script or None"""
        return cls.__random__.choice([None, '.result = "success"', ".transformed = true"])


@pytest.mark.parametrize(
    "config_factory,expected_type",
    [
        (StdioServerConfigFactory, StdioMCPClient),
        (SseServerConfigFactory, SseMCPClient),
        (StreamableHttpServerConfigFactory, HttpMCPClient),
    ],
)
def test_client_factory_type(config_factory, expected_type):
    """
    检查 client_factory 根据不同配置类型返回正确的客户端类型。
    Check client_factory returns the correct client type for each config type.
    """
    config = config_factory.build()
    client = client_factory(config)
    assert isinstance(client, expected_type)


def test_client_factory_invalid_type():
    """
    检查 client_factory 对不支持的类型抛出异常。
    Check client_factory raises exception for unsupported config types.
    """

    class DummyConfig:
        pass

    with pytest.raises(ValueError):
        client_factory(DummyConfig())  # noqa
