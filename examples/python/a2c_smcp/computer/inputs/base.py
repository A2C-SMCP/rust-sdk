# -*- coding: utf-8 -*-
# 文件名: base.py
# 作者: JQQ
# 创建日期: 2025/9/24
# 最后修改日期: 2025/9/24
# 版权: 2023 JQQ. All rights reserved.
# 依赖: typing, a2c_smcp_cc.mcp_clients.model
# 描述:
#   中文: 定义输入解析器的泛型基类，抽象出会话（Session）类型，统一缓存与接口定义。
#   English: Define a generic base class for input resolvers. Abstract the Session type and unify cache and interfaces.

"""
中文: 输入解析器泛型基类
English: Generic base class for input resolvers.

设计要点:
- 将 Session 抽象为泛型 S，便于在 CLI / GUI / Web 等不同环境中复用。
- 统一缓存管理接口（获取/设置/删除/列出/清空）。
- aresolve_by_id 由子类实现具体的交互与解析逻辑。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import Iterable
from typing import Any, Generic, TypeVar

from a2c_smcp.computer.mcp_clients.model import (
    MCPServerCommandInput,
    MCPServerInput,
    MCPServerPickStringInput,
    MCPServerPromptStringInput,
)

S = TypeVar("S")


class BaseInputResolver(Generic[S], ABC):
    """
    中文: 输入解析器的抽象基类，参数化会话类型 S。
    English: Abstract base class for input resolvers, parameterized by session type S.
    """

    def __init__(self, inputs: Iterable[MCPServerInput], session: S | None = None) -> None:
        # 中文: inputs 定义快照与解析缓存
        # English: inputs definition snapshot and resolve cache
        self._inputs: dict[str, MCPServerInput] = {i.id: i for i in inputs}
        self._cache: dict[str, Any] = {}
        # 中文: 可选会话实例，子类在解析时可优先使用
        # English: Optional session instance; subclasses may prefer using it during resolution
        self.session: S | None = session

    # ---------- 缓存管理 / Cache management ----------
    def clear_cache(self, key: str | None = None) -> None:
        if key is None:
            self._cache.clear()
        else:
            self._cache.pop(key, None)

    def get_cached_value(self, input_id: str) -> Any | None:
        """
        中文: 获取指定 id 的已解析缓存值；不存在时返回 None。
        English: Get cached value of given id; return None if not present.
        """
        return self._cache.get(input_id)

    def set_cached_value(self, input_id: str, value: Any) -> bool:
        """
        中文: 设置指定 id 的缓存值；仅当该 id 在 inputs 定义中存在时生效，返回是否成功。
        English: Set cached value for given id; only works if id exists in inputs. Returns success flag.
        """
        if input_id not in self._inputs:
            return False
        self._cache[input_id] = value
        return True

    def delete_cached_value(self, input_id: str) -> bool:
        """
        中文: 删除指定 id 的缓存值，返回是否删除发生。
        English: Delete cached value for given id, returns whether deletion happened.
        """
        if input_id in self._cache:
            self._cache.pop(input_id, None)
            return True
        return False

    def list_cached_values(self) -> dict[str, Any]:
        """
        中文: 返回当前所有 inputs 的缓存值快照（浅拷贝）。
        English: Return a snapshot (shallow copy) of all cached input values.
        """
        return dict(self._cache)

    # ---------- 解析接口 / Resolve API ----------
    @abstractmethod
    async def aresolve_by_id(self, input_id: str, *, session: S | None = None) -> Any:  # pragma: no cover - interface
        """
        中文: 根据 id 解析输入，可能触发交互；子类需要实现具体逻辑。
        English: Resolve input by id, may trigger interactions; subclass must implement.
        """
        raise NotImplementedError

    @abstractmethod
    async def _aresolve_prompt(
        self,
        cfg: MCPServerPromptStringInput,
        *,
        session: S | None = None,
    ) -> str:  # pragma: no cover - interface
        """
        中文: 解析 promptString 类型输入。
        English: Resolve a promptString type input.
        """
        raise NotImplementedError

    @abstractmethod
    async def _aresolve_pick(
        self,
        cfg: MCPServerPickStringInput,
        *,
        session: S | None = None,
    ) -> str:  # pragma: no cover - interface
        """
        中文: 解析 pickString 类型输入。
        English: Resolve a pickString type input.
        """
        raise NotImplementedError

    @abstractmethod
    async def _aresolve_command(self, cfg: MCPServerCommandInput) -> Any:  # pragma: no cover - interface
        """
        中文: 解析 command 类型输入。
        English: Resolve a command type input.
        """
        raise NotImplementedError
