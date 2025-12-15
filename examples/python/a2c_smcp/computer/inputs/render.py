"""
文件名: render.py
作者: JQQ
创建日期: 2025/9/18
最后修改日期: 2025/9/18
版权: 2023 JQQ. All rights reserved.
依赖: re, typing
描述:
  中文: 针对 MCP 配置的按需渲染器，识别占位符 ${input:<id>} 并通过回调异步解析，支持递归容器结构与深度限制。
  English: On-demand renderer for MCP configs. Detects placeholders ${input:<id>} and resolves them via async callback,
           supports container recursion with depth limiting.
"""

from __future__ import annotations

import re
from collections.abc import Awaitable, Callable
from typing import Any

from a2c_smcp.utils.logger import logger

# 中文: 匹配占位符 ${input:xxx} 的正则
# English: Regex to match placeholder ${input:xxx}
PLACEHOLDER_PATTERN = re.compile(r"\$\{input:([^}]+)}")


class ConfigRender:
    """
    中文: 配置渲染器，按需解析字符串中的占位符，并递归处理字典与列表。
    English: Config renderer that lazily resolves placeholders in strings and recursively processes dicts and lists.
    """

    def __init__(self, *, max_depth: int = 10) -> None:
        self._max_depth = max_depth

    async def arender(self, data: Any, resolve_input: Callable[[str], Awaitable[Any]], _depth: int = 0) -> Any:
        """
        中文: 递归渲染任意结构的数据，字符串中按需替换占位符。
        English: Recursively render arbitrary structured data, replacing placeholders in strings on demand.
        """
        if _depth > self._max_depth:
            logger.error("渲染深度超过限制，停止递归 / Rendering depth exceeded, stop recursion")
            return data

        if isinstance(data, dict):
            return {k: await self.arender(v, resolve_input, _depth + 1) for k, v in data.items()}
        if isinstance(data, list):
            return [await self.arender(x, resolve_input, _depth + 1) for x in data]
        if isinstance(data, str):
            return await ConfigRender.arender_str(data, resolve_input)
        return data

    @staticmethod
    async def arender_str(s: str, resolve_input: Callable[[str], Awaitable[Any]]) -> Any:
        """
        中文: 对字符串中的所有占位符进行按需替换；若替换为非字符串值，返回替换后的非字符串值（用于纯占位符情况）。
        English: Replace placeholders lazily; if the entire string is a single placeholder and resolves to a non-string,
        return that value.
        """
        matches = list(PLACEHOLDER_PATTERN.finditer(s))
        if not matches:
            return s

        # 如果字符串仅包含单一占位符且没有其他字符，允许返回非字符串类型
        if len(matches) == 1 and matches[0].span() == (0, len(s)):
            input_id = matches[0].group(1)
            try:
                return await resolve_input(input_id)
            except KeyError:
                logger.warning(f"未找到输入项: {input_id} / Input id not found: {input_id}")
                return s
            except Exception as e:  # pragma: no cover
                logger.error(f"解析输入失败: {input_id}, 错误: {e}")
                return s

        # 否则逐个替换为其字符串表现形式
        result = s
        offset = 0
        for m in matches:
            start, end = m.span()
            input_id = m.group(1)
            try:
                value = await resolve_input(input_id)
            except KeyError:
                logger.warning(f"未找到输入项: {input_id} / Input id not found: {input_id}")
                continue
            except Exception as e:  # pragma: no cover
                logger.error(f"解析输入失败: {input_id}, 错误: {e}")
                continue

            # 将非字符串值转换为字符串嵌入
            repl = str(value)
            # 由于我们在原始字符串上替换，位置需要考虑之前替换带来的偏移
            result = result[: start + offset] + repl + result[end + offset :]
            offset += len(repl) - (end - start)
        return result
