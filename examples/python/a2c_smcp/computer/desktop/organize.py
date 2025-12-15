# -*- coding: utf-8 -*-
# filename: organize.py
# @Time    : 2025/10/02 16:12
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
桌面组织策略（抽象层）
Desktop organizing strategy (abstraction layer)

说明 / Notes:
- 本文件仅定义组织函数签名与最小可运行实现，便于主流程打通；
  后续会基于：最近工具调用历史、MCP内优先级、全屏窗口等规则完善策略。
- This file only defines the function signature and a minimal runnable implementation
  to keep main flow working. We'll implement real policies later based on recent
  tool-call history, in-server priority, and fullscreen windows, etc.
"""

from __future__ import annotations

from mcp.types import BlobResourceContents, ReadResourceResult, Resource, TextResourceContents

from a2c_smcp.computer.types import ToolCallRecord
from a2c_smcp.smcp import Desktop
from a2c_smcp.types import SERVER_NAME
from a2c_smcp.utils import WindowURI

__all__ = ["organize_desktop"]

from a2c_smcp.utils.logger import logger


async def organize_desktop(
    *,
    windows: list[tuple[SERVER_NAME, Resource, ReadResourceResult]],
    size: int | None,
    history: tuple[ToolCallRecord, ...],
) -> list[Desktop]:
    """
    组织来自各 MCP Server 的窗口资源，生成 Desktop 列表。
    Organize window resources from MCP servers into a list of Desktop items.

    组织规则（来自 /desktop 工作流）：
    Rules from /desktop workflow:
      1) 若指定 window_uri，Manager 层已完成过滤；此处按一般规则组织即可。
      2) 按最近工具调用历史对应的 MCP Server 倒序优先（最近使用的服务器优先）。
      3) 同一 MCP Server 内，按 WindowURI.priority 降序推入（默认 0）。
      4) 若遇到 fullscreen=True 的窗口，则该 MCP 仅推入这一个；若 size 仍有剩余，则进入下一个 MCP。
      5) 全局按 size 截断（None 表示不限；size<=0 则返回空）。

    Args:
        windows (list[tuple[SERVER_NAME, Resource]]): (server_name, resource) 列表。
        size (int | None): 期望返回的最大数量；None 表示全部。
        history (tuple[dict[str, Any], ...]): 最近的工具调用历史（未使用，后续版本实现）。

    Returns:
        list[Desktop]: 桌面内容（字符串列表）。
    """
    # 快速处理 size 边界
    if size is not None and size <= 0:
        return []

    # 1) 构建服务器 -> 窗口 列表映射，并解析 priority、fullscreen，保留原始序号以确定“第一个 fullscreen”
    #    同时过滤无内容的资源（contents 为空时跳过）。为后续渲染，保留 detail。
    grouped: dict[SERVER_NAME, list[tuple[Resource, int, bool, int, ReadResourceResult]]] = {}
    for idx, (server, res, detail) in enumerate(windows):
        try:
            contents = detail.contents
            if not contents or (isinstance(contents, list) and len(contents) == 0):
                # 无内容的窗口不进入桌面组织 / skip windows without contents
                continue
        except Exception:
            # detail 异常则保守丢弃 / drop on error
            continue
        try:
            wuri = WindowURI(str(res.uri))
            prio = wuri.priority if wuri.priority is not None else 0
            fullscreen = bool(wuri.fullscreen) if wuri.fullscreen is not None else False
        except Exception:
            # 非法 URI 或解析失败的资源，跳过
            continue
        grouped.setdefault(server, []).append((res, prio, fullscreen, idx, detail))

    # 2) 服务器优先级：根据最近工具调用历史，倒序去重
    recent_servers: list[SERVER_NAME] = []
    seen: set[SERVER_NAME] = set()
    for rec in reversed(history):  # 最近在前
        srv = rec.get("server")
        if srv in grouped and srv not in seen:
            seen.add(srv)
            recent_servers.append(srv)

    # 其余服务器（未在历史中出现）按名称稳定排序追加
    remaining = sorted([s for s in grouped.keys() if s not in seen])
    server_order: list[SERVER_NAME] = recent_servers + remaining

    # 3) 每个服务器内按 priority 降序排序
    for srv in server_order:
        grouped[srv].sort(key=lambda x: x[1], reverse=True)

    # 4) 组装按服务器顺序的窗口列表，处理 fullscreen 规则
    # 渲染函数：将 URI 与内容一起渲染到 Desktop 字符串
    # Render function: combine URI and textual contents into Desktop string
    def _render(ri: Resource, di: ReadResourceResult) -> str:
        try:
            parts: list[str] = []
            for block in di.contents or []:
                if isinstance(block, TextResourceContents):
                    if isinstance(block.text, str) and block.text:
                        parts.append(block.text)
                elif isinstance(block, BlobResourceContents):
                    logger.warning("当前桌面环境尚不支持Blob内容")
                else:
                    logger.error("未成功识别当前Window内容")
            body = "\n\n".join(parts).strip()
            return f"{str(ri.uri)}\n\n{body}" if body else str(ri.uri)
        except Exception as e:
            logger.error(f"发生未知异常: {e}")
            return str(ri.uri)

    result: list[str] = []
    cap = size if size is not None else float("inf")
    for srv in server_order:
        items = grouped.get(srv, [])
        if not items:
            continue
        if len(result) >= cap:
            break
        # 若存在 fullscreen -> 选择“第一个出现的 fullscreen”（按原始 windows 序号最小）
        fullscreen_items = [(res, prio, fs, idx, det) for (res, prio, fs, idx, det) in items if fs]
        if fullscreen_items:
            res, _prio, _fs, _idx, det = min(fullscreen_items, key=lambda x: x[3])
            result.append(_render(res, det))
            continue  # 该服务器仅加入这一个，转下一个服务器

        # 否则按优先级加入多条直到 cap
        for res, _, _, _, det in items:
            if len(result) >= cap:
                break
            result.append(_render(res, det))

    return result
