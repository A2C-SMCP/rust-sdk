# -*- coding: utf-8 -*-
# filename: types.py
# @Time    : 2025/10/02 16:17
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
计算机模块类型定义
Computer module type definitions

说明 / Notes:
- 提供在 Computer 与 Desktop 组织策略间共用的类型。
- Provide types shared between Computer and Desktop organizing strategy.
"""

from typing import TypedDict

__all__ = ["ToolCallRecord"]


class ToolCallRecord(TypedDict):
    """
    工具调用历史记录结构
    Schema for tool call history record
    """

    timestamp: str
    req_id: str
    server: str
    tool: str
    parameters: dict
    timeout: float | None
    success: bool
    error: str | None
