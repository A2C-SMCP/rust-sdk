"""
* 文件名: auth
* 作者: JQQ
* 创建日期: 2025/9/30
* 最后修改日期: 2025/9/30
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Agent端认证接口抽象定义 / Agent-side authentication interface abstract definition
"""

from abc import ABC, abstractmethod
from typing import Any

from a2c_smcp.agent.types import AgentConfig


class AgentAuthProvider(ABC):
    """
    Agent认证提供者抽象基类，用于处理Agent连接的认证逻辑
    Abstract base class for Agent authentication providers, handles Agent connection authentication logic
    """

    @abstractmethod
    def get_agent_id(self) -> str:
        """
        获取agent_id，由用户实现具体逻辑
        Get agent_id, implemented by user with specific logic

        Returns:
            str: agent_id
        """
        pass

    @abstractmethod
    def get_connection_auth(self) -> dict[str, Any] | None:
        """
        获取连接认证信息，用于Socket.IO连接时的auth参数
        Get connection authentication info, used for Socket.IO connection auth parameter

        Returns:
            dict[str, Any] | None: 认证信息字典 / Authentication info dictionary
        """
        pass

    @abstractmethod
    def get_connection_headers(self) -> dict[str, str]:
        """
        获取连接请求头，用于Socket.IO连接时的headers参数
        Get connection headers, used for Socket.IO connection headers parameter

        Returns:
            dict[str, str]: 请求头字典 / Headers dictionary
        """
        pass

    @abstractmethod
    def get_agent_config(self) -> AgentConfig:
        """
        获取Agent配置信息
        Get Agent configuration information

        Returns:
            AgentConfig: Agent配置 / Agent configuration
        """
        pass


class DefaultAgentAuthProvider(AgentAuthProvider):
    """
    默认Agent认证提供者，提供基础的认证逻辑实现
    Default Agent authentication provider, provides basic authentication logic implementation
    """

    def __init__(
        self,
        agent_id: str,
        office_id: str,
        api_key: str | None = None,
        api_key_header: str = "x-api-key",
        extra_headers: dict[str, str] | None = None,
        auth_data: dict[str, Any] | None = None,
    ) -> None:
        """
        初始化默认Agent认证提供者
        Initialize default Agent authentication provider

        Args:
            agent_id (str): Agent唯一标识 / Agent unique identifier
            office_id (str): 办公室ID / Office ID
            api_key (str | None): API密钥 / API key
            api_key_header (str): API密钥请求头名称 / API key header name
            extra_headers (dict[str, str] | None): 额外请求头 / Extra headers
            auth_data (dict[str, Any] | None): 额外认证数据 / Extra auth data
        """
        self._agent_id = agent_id
        self._office_id = office_id
        self._api_key = api_key
        self._api_key_header = api_key_header
        self._extra_headers = extra_headers or {}
        self._auth_data = auth_data or {}

    def get_agent_id(self) -> str:
        """
        获取Agent ID
        Get Agent ID
        """
        return self._agent_id

    def get_connection_auth(self) -> dict[str, Any] | None:
        """
        获取连接认证信息
        Get connection authentication info
        """
        if not self._auth_data:
            return None
        return self._auth_data.copy()

    def get_connection_headers(self) -> dict[str, str]:
        """
        获取连接请求头
        Get connection headers
        """
        headers = self._extra_headers.copy()

        # 添加API密钥到请求头
        # Add API key to headers
        if self._api_key:
            headers[self._api_key_header] = self._api_key

        return headers

    def get_agent_config(self) -> AgentConfig:
        """
        获取Agent配置信息
        Get Agent configuration information
        """
        return AgentConfig(
            agent=self._agent_id,
            office_id=self._office_id,
        )
