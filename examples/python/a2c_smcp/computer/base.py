# -*- coding: utf-8 -*-
# 文件名: base.py
# 作者: JQQ
# 创建日期: 2025/9/24
# 最后修改日期: 2025/9/24
# 版权: 2023 JQQ. All rights reserved.
# 依赖: typing, a2c_smcp_cc.mcp_clients.model
# 描述:
#   中文: 定义计算机管理器的泛型基类，抽象出会话（Session）类型以便于不同交互环境（如 CLI、GUI、Web）复用核心能力。
#   English: Define a generic base class for computer managers, abstracting the Session type so that core
#            capabilities can be reused across different interaction environments (CLI, GUI, Web).

"""
中文: 计算机管理器泛型基类
English: Generic base class for computer manager.

说明 Notes:
- 该基类只规定与 Session 相关的关键方法签名，将 Session 抽象为泛型参数 S。
- 具体实现（如 CLI 版）在子类中特化 S 的具体类型，并实现这些方法。
- 其他与 Session 无关的方法（状态查询、工具执行等）可由子类自由扩展。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any, Generic, TypeVar

from a2c_smcp.computer.mcp_clients.model import MCPServerConfig, MCPServerInput

S = TypeVar("S")


class BaseComputer(Generic[S], ABC):
    """
    中文: 计算机管理器的抽象基类，通过泛型 S 抽象会话类型。
    English: Abstract base class for computer manager, parameterized by generic session type S.
    """

    @abstractmethod
    async def boot_up(self, *, session: S | None = None) -> None:  # pragma: no cover - interface definition
        """
        中文: 启动并初始化资源；可选地使用给定的会话。
        English: Boot and initialize resources; optionally use the given session.
        """
        raise NotImplementedError

    @abstractmethod
    async def aadd_or_aupdate_server(self, server: MCPServerConfig | dict, *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 动态添加或更新某个服务配置。
        English: Add or update a server config dynamically.
        """
        raise NotImplementedError

    # -------- MCP Server lifecycle --------
    @abstractmethod
    async def aremove_server(self, server_name: str, *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 动态移除某个服务配置。
        English: Remove a server config dynamically.
        """
        raise NotImplementedError

    # -------- Inputs definition management --------
    @abstractmethod
    def update_inputs(self, inputs: set[MCPServerInput], *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 批量更新 inputs 定义，并重置解析器（可带入默认会话）。
        English: Update inputs definitions and reset resolver (with optional default session).
        """
        raise NotImplementedError

    @abstractmethod
    def add_or_update_input(self, input_cfg: MCPServerInput, *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 新增或更新单个 input 定义。
        English: Add or update a single input definition.
        """
        raise NotImplementedError

    @abstractmethod
    def remove_input(self, input_id: str, *, session: S | None = None) -> bool:  # pragma: no cover
        """
        中文: 按 id 移除单个 input 定义，返回是否删除发生。
        English: Remove a single input definition by id, return whether deletion happened.
        """
        raise NotImplementedError

    @abstractmethod
    def get_input(self, input_id: str, *, session: S | None = None) -> MCPServerInput | None:  # pragma: no cover
        """
        中文: 获取指定 id 的 input 定义。
        English: Get input definition by id.
        """
        raise NotImplementedError

    @abstractmethod
    def list_inputs(self, *, session: S | None = None) -> tuple[MCPServerInput, ...]:  # pragma: no cover
        """
        中文: 列出当前全部 inputs（不可变视图）。
        English: List all current inputs (immutable view).
        """
        raise NotImplementedError

    # -------- Current input values (cache) --------
    @abstractmethod
    def get_input_value(self, input_id: str, *, session: S | None = None) -> Any | None:  # pragma: no cover
        """
        中文: 获取指定 id 的当前值（缓存）。
        English: Get current value for the given id (cache).
        """
        raise NotImplementedError

    @abstractmethod
    def set_input_value(self, input_id: str, value: Any, *, session: S | None = None) -> bool:  # pragma: no cover
        """
        中文: 设置指定 id 的当前值（缓存）。
        English: Set current value for the given id (cache).
        """
        raise NotImplementedError

    @abstractmethod
    def remove_input_value(self, input_id: str, *, session: S | None = None) -> bool:  # pragma: no cover
        """
        中文: 删除指定 id 的当前值（缓存）。
        English: Delete current value for the given id (cache).
        """
        raise NotImplementedError

    @abstractmethod
    def list_input_values(self, *, session: S | None = None) -> dict[str, Any]:  # pragma: no cover
        """
        中文: 列出所有已解析的 inputs 当前值（缓存快照）。
        English: List all resolved input values (cache snapshot).
        """
        raise NotImplementedError

    @abstractmethod
    def clear_input_values(self, input_id: str | None = None, *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 清空所有或指定 id 的输入值缓存。
        English: Clear all cached values or the specified id.
        """
        raise NotImplementedError

    # -------- Lifecycle --------
    @abstractmethod
    async def shutdown(self, *, session: S | None = None) -> None:  # pragma: no cover
        """
        中文: 关闭资源。
        English: Shutdown resources.
        """
        raise NotImplementedError
