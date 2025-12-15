"""
文件名: resolver.py
作者: JQQ
创建日期: 2025/9/18
最后修改日期: 2025/9/18
版权: 2023 JQQ. All rights reserved.
依赖: prompt_toolkit, rich
描述:
  中文: inputs 解析器定义与实现。按需根据 id 解析三类输入：promptString、pickString、command。
  English: Input resolvers. Lazily resolve three kinds of inputs by id: promptString, pickString, command.
"""

from __future__ import annotations

from collections.abc import Iterable
from typing import Any

from prompt_toolkit import PromptSession

from a2c_smcp.computer.inputs.base import BaseInputResolver
from a2c_smcp.computer.inputs.cli_io import ainput_pick, ainput_prompt, arun_command
from a2c_smcp.computer.mcp_clients.model import (
    MCPServerCommandInput,
    MCPServerInput,
    MCPServerPickStringInput,
    MCPServerPromptStringInput,
)
from a2c_smcp.utils.logger import logger


class InputNotFoundError(KeyError):
    pass


class InputResolver(BaseInputResolver[PromptSession]):
    """
    中文: 输入解析器，支持基于 id 的惰性解析与结果缓存。
    English: Input resolver with lazy per-id resolution and result cache.
    """

    def __init__(self, inputs: Iterable[MCPServerInput], session: PromptSession | None = None) -> None:
        """
        中文: CLI 特化的输入解析器，支持可选的 PromptSession 注入。
        English: CLI-specialized input resolver with optional PromptSession injection.
        """
        super().__init__(inputs, session=session)

    async def aresolve_by_id(self, input_id: str, *, session: PromptSession | None = None) -> Any:
        if input_id in self._cache:
            return self._cache[input_id]
        cfg = self._inputs.get(input_id)
        if not cfg:
            raise InputNotFoundError(input_id)

        # 优先使用调用时传入的 session，其次使用初始化指定的 self.session
        # Prefer provided session; fallback to self.session
        sess = session or self.session

        if isinstance(cfg, MCPServerPromptStringInput):
            value = await self._aresolve_prompt(cfg, session=sess)
        elif isinstance(cfg, MCPServerPickStringInput):
            value = await self._aresolve_pick(cfg, session=sess)
        elif isinstance(cfg, MCPServerCommandInput):
            value = await self._aresolve_command(cfg)
        else:  # pragma: no cover
            logger.warning(f"未知输入类型: {type(cfg)} / Unknown input type")
            value = None

        self._cache[input_id] = value
        return value

    async def _aresolve_prompt(self, cfg: MCPServerPromptStringInput, *, session: PromptSession | None = None) -> str:
        msg = cfg.description or f"请输入 {cfg.id} / Please input {cfg.id}"
        pwd = bool(cfg.password)
        return await ainput_prompt(msg, password=pwd, default=cfg.default, session=session)

    async def _aresolve_pick(self, cfg: MCPServerPickStringInput, *, session: PromptSession | None = None) -> str:
        msg = cfg.description or f"请选择 {cfg.id} / Please pick {cfg.id}"
        options = cfg.options or []
        default_index = None
        if cfg.default is not None and cfg.default in options:
            default_index = options.index(cfg.default)
        picked = await ainput_pick(msg, options, default_index=default_index, multi=False, session=session)
        return str(picked) or (cfg.default or "")

    async def _aresolve_command(self, cfg: MCPServerCommandInput) -> Any:
        # 约定: command 为完整可执行字符串，由 shell 执行。args 如存在，暂不拼接，后续可扩展。
        return await arun_command(cfg.command, shell=True, parse="raw")
