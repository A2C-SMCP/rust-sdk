"""
* 文件名: test_auth
* 作者: JQQ
* 创建日期: 2025/9/30
* 最后修改日期: 2025/9/30
* 版权: 2023 JQQ. All rights reserved.
* 依赖: pytest
* 描述: Agent认证模块测试用例 / Agent authentication module test cases
"""

import pytest

from a2c_smcp.agent.auth import AgentAuthProvider, DefaultAgentAuthProvider
from a2c_smcp.agent.types import AgentConfig


class TestAgentAuthProvider:
    """测试Agent认证提供者抽象基类 / Test Agent authentication provider abstract base class"""

    def test_abstract_methods(self) -> None:
        """测试抽象方法不能直接实例化 / Test abstract methods cannot be instantiated directly"""
        with pytest.raises(TypeError):
            AgentAuthProvider()  # type: ignore


class TestDefaultAgentAuthProvider:
    """测试默认Agent认证提供者 / Test default Agent authentication provider"""

    def test_init_with_minimal_params(self) -> None:
        """测试使用最小参数初始化 / Test initialization with minimal parameters"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
        )

        assert provider.get_agent_id() == "test_agent"

        config = provider.get_agent_config()
        assert config["agent"] == "test_agent"
        assert config["office_id"] == "test_office"

    def test_init_with_full_params(self) -> None:
        """测试使用完整参数初始化 / Test initialization with full parameters"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
            api_key="test_key",
            api_key_header="Authorization",
            extra_headers={"Custom-Header": "custom_value"},
            auth_data={"token": "test_token"},
        )

        assert provider.get_agent_id() == "test_agent"

        # 测试认证数据
        # Test authentication data
        auth_data = provider.get_connection_auth()
        assert auth_data == {"token": "test_token"}

        # 测试请求头
        # Test headers
        headers = provider.get_connection_headers()
        assert headers["Authorization"] == "test_key"
        assert headers["Custom-Header"] == "custom_value"

    def test_get_connection_auth_empty(self) -> None:
        """测试空认证数据 / Test empty authentication data"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
        )

        auth_data = provider.get_connection_auth()
        assert auth_data is None

    def test_get_connection_headers_with_api_key(self) -> None:
        """测试带API密钥的请求头 / Test headers with API key"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
            api_key="secret_key",
        )

        headers = provider.get_connection_headers()
        assert headers["x-api-key"] == "secret_key"

    def test_get_connection_headers_without_api_key(self) -> None:
        """测试不带API密钥的请求头 / Test headers without API key"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
        )

        headers = provider.get_connection_headers()
        assert "x-api-key" not in headers

    def test_get_agent_config(self) -> None:
        """测试获取Agent配置 / Test get Agent configuration"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent_123",
            office_id="test_office_456",
        )

        config = provider.get_agent_config()
        expected_config: AgentConfig = {
            "agent": "test_agent_123",
            "office_id": "test_office_456",
        }
        assert config == expected_config

    def test_custom_api_key_header(self) -> None:
        """测试自定义API密钥请求头 / Test custom API key header"""
        provider = DefaultAgentAuthProvider(
            agent_id="test_agent",
            office_id="test_office",
            api_key="test_key",
            api_key_header="X-Custom-Auth",
        )

        headers = provider.get_connection_headers()
        assert headers["X-Custom-Auth"] == "test_key"
        assert "x-api-key" not in headers
