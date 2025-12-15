"""
* 文件名: __init__
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: A2C-SMCP Server模块导出 / A2C-SMCP Server module exports
"""

from .auth import AuthenticationProvider, DefaultAuthenticationProvider
from .base import BaseNamespace
from .namespace import SMCPNamespace
from .sync_auth import DefaultSyncAuthenticationProvider, SyncAuthenticationProvider
from .sync_base import SyncBaseNamespace
from .sync_namespace import SyncSMCPNamespace
from .types import OFFICE_ID, SID, AgentSession, BaseSession, ComputerSession, Session
from .utils import (
    aget_all_sessions_in_office,
    aget_computers_in_office,
    get_all_sessions_in_office,
    get_computers_in_office,
)

__all__ = [
    # 认证相关 / Authentication related
    "AuthenticationProvider",
    "DefaultAuthenticationProvider",
    # 同步认证相关 / Sync authentication
    "SyncAuthenticationProvider",
    "DefaultSyncAuthenticationProvider",
    # 基础类 / Base classes
    "BaseNamespace",
    "SMCPNamespace",
    # 同步基础类 / Sync base classes
    "SyncBaseNamespace",
    "SyncSMCPNamespace",
    # 类型定义 / Type definitions
    "OFFICE_ID",
    "SID",
    "BaseSession",
    "ComputerSession",
    "AgentSession",
    "Session",
    # 工具函数 / Utility functions
    "aget_computers_in_office",
    "get_computers_in_office",
    "aget_all_sessions_in_office",
    "get_all_sessions_in_office",
]
