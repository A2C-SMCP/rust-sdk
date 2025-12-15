# -*- coding: utf-8 -*-
# filename: window_uri.py
# @Time    : 2025/10/01 19:43
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
窗口协议 WindowURI 封装
WindowURI encapsulation for MCP window:// resources

协议说明 / Protocol:
- scheme 固定为 window -> 形如 window://host/path1/path2?.../pathN?priority=..&fullscreen=..
  Scheme is fixed to 'window' -> e.g., window://host/path1/.../pathN?priority=..&fullscreen=..
- host 为 MCP 的唯一标识（建议域名），由 MCP 自定义，需避免与其他 MCP 冲突
  host is MCP unique id (domain-like recommended), defined by MCP, must not collide with others
- path 可以有 0..N 个段，一个 MCP 可暴露多个 window，是否渲染由桌面系统决定
  path can have 0..N segments; desktop decides actual rendering
- 查询参数 / Query params：
  - priority: 0-100 的整数，仅在同一 MCP 内部窗口间比较时生效，影响布局排序（越大越靠上）
              integer 0-100, only intra-MCP comparison; affects layout ordering (higher -> higher)
  - fullscreen: 布尔值；若为 true，Agent 调用该 MCP 工具后，Desktop 将尽量完整渲染该 window。
                多个 fullscreen 仅第一个生效。
                boolean; if true, desktop attempts full rendering; only the first fullscreen window takes effect.

Pydantic v2 兼容：支持字符串 <-> WindowURI 的校验与序列化
Pydantic v2 compatibility: supports string <-> WindowURI validation and serialization
"""

from __future__ import annotations

from collections.abc import Iterable
from functools import cached_property
from typing import Any, Self, cast

from pydantic import AnyUrl, GetCoreSchemaHandler, GetJsonSchemaHandler
from pydantic.json_schema import JsonSchemaValue
from pydantic_core import CoreSchema
from yarl import URL as YURL


class WindowURI:
    """
    WindowURI 用于解析与构建 window:// 协议的 URI。
    WindowURI is for parsing and building window:// protocol URIs.
    """

    def __init__(self, uri: str) -> None:
        """
        使用 yarl.URL 解析并缓存 URL 组件，避免第三方在 Py3.12 的兼容性问题。
        Parse and cache URL components via yarl.URL to avoid compat issues on Py3.12.
        """
        self._url = YURL(uri)
        if self._url.scheme != "window":
            raise ValueError(f"Invalid URI scheme: {self._url.scheme}, uri={uri}")
        if not self._url.host:
            raise IndexError(f"Missing host (MCP id) in URI: {uri}")
        # 校验查询参数合法性 / validate query params
        _ = self.priority  # will raise if invalid
        _ = self.fullscreen  # normalize

    # -------------------------
    # 基础属性 / Basic properties
    # -------------------------
    @cached_property
    def mcp_id(self) -> str:
        """
        获取 MCP 唯一标识（host）
        Get MCP unique identifier (host)
        """
        return str(self._url.host)

    @cached_property
    def windows(self) -> list[str]:
        """
        获取 window 路径段列表，允许为空列表
        Get list of window path segments, can be empty
        """
        return list(self.parts)

    @cached_property
    def parts(self) -> list[str]:
        """
        路径段列表：基于原始编码的 path 分割后逐段解码，保证如 "c%2Fd" -> "c/d" 保持为单段
        Path segments: split raw-encoded path then decode each segment, keeping "c%2Fd" as one "c/d" part
        """
        from urllib.parse import unquote

        raw = self._url.raw_path.lstrip("/")
        if not raw:
            return []
        return [unquote(seg) for seg in raw.split("/")]

    # -------------------------
    # 查询参数 / Query parameters
    # -------------------------
    @cached_property
    def priority(self) -> int | None:
        """
        获取 priority（0-100），不存在则返回 None；非法值抛出异常
        Get priority (0-100); None if absent; raise on invalid
        """
        val = self._url.query.get("priority")
        if val is None:
            return None
        raw = val
        try:
            int_val = int(raw)
        except Exception as e:
            raise ValueError(f"Invalid priority value: {raw}") from e
        if not (0 <= int_val <= 100):
            raise ValueError(f"priority must be in [0, 100], got: {val}")
        return int_val

    @cached_property
    def fullscreen(self) -> bool | None:
        """
        获取 fullscreen 布尔值；不存在则返回 None
        Get fullscreen boolean; None if absent
        """
        val = self._url.query.get("fullscreen")
        if val is None:
            return None
        raw = val.strip().lower()
        if raw in {"1", "true", "yes", "on"}:
            return True
        if raw in {"0", "false", "no", "off"}:
            return False
        raise ValueError(f"Invalid fullscreen value: {raw}")

    # -------------------------
    # 构造函数 / Builder
    # -------------------------
    @classmethod
    def build_uri(
        cls,
        *,
        host: str,
        windows: Iterable[str] | None = None,
        priority: int | None = None,
        fullscreen: bool | None = None,
    ) -> Self:
        """
        通过 host、windows（0..N 段路径）、可选 priority 与 fullscreen 构建 WindowURI。
        Build WindowURI from host, windows (0..N path segments), optional priority and fullscreen.
        所有路径段进行 URL 编码；仅在提供时添加查询参数。
        All path segments are URL-encoded; query params only included when provided.
        """
        from urllib.parse import quote, urlencode

        if not host:
            raise ValueError("host (MCP id) is required")

        # 对每个段进行编码，确保如 "c/d" -> "c%2Fd" 保持为单段
        segs = [quote(p, safe="") for p in (list(windows) if windows else [])]
        path = "/" + "/".join(segs) if segs else ""

        query_items: dict[str, str] = {}
        if priority is not None:
            if not isinstance(priority, int) or not (0 <= priority <= 100):
                raise ValueError(f"priority must be int in [0, 100], got: {priority}")
            query_items["priority"] = str(priority)
        if fullscreen is not None:
            query_items["fullscreen"] = "true" if fullscreen else "false"

        query_str = urlencode(query_items) if query_items else ""
        s = f"window://{host}{path}"
        if query_str:
            s += f"?{query_str}"
        return cls(s)

    # -------------------------
    # Pydantic v2 支持 / Pydantic v2 support
    # -------------------------
    @classmethod
    def __get_pydantic_core_schema__(cls, source_type: Any, handler: GetCoreSchemaHandler) -> Any:
        """
        定义 Pydantic 的校验与序列化行为：
        1. 校验：允许字符串或 WindowURI 实例 -> 返回 WindowURI 实例
        2. 序列化：WindowURI 实例 -> 字符串
        Define Pydantic validation and serialization behavior:
        1. Validation: allow str or WindowURI -> return WindowURI
        2. Serialization: WindowURI -> str
        """
        from pydantic_core import core_schema

        def validate(value: str | WindowURI) -> WindowURI:
            if isinstance(value, cls):
                return value
            try:
                return cls(cast(str, value))
            except Exception as e:
                raise ValueError(f"Invalid WindowURI: {value}") from e

        def serialize(value: WindowURI) -> str:
            return str(value)

        return core_schema.no_info_plain_validator_function(
            function=validate,
            serialization=core_schema.plain_serializer_function_ser_schema(function=serialize),
        )

    @classmethod
    def __get_pydantic_json_schema__(cls, core_schema: CoreSchema, handler: GetJsonSchemaHandler) -> JsonSchemaValue:
        return {"type": "string", "format": "uri"}

    # -------------------------
    # 基础访问 / Basic delegates
    # -------------------------
    def __str__(self) -> str:  # noqa: D401
        """返回标准字符串形式 / Return canonical string form"""
        return str(self._url)


def is_window_uri(value: str | AnyUrl | object) -> bool:
    """
    判断给定字符串是否为合法的 WindowURI（window:// 协议）
    Check whether the given string is a valid WindowURI (window:// scheme)

    规则 / Rules:
    - 必须为 window scheme
    - 必须包含 host（MCP 唯一标识）
    - 查询参数若存在则需可被解析（如 priority 范围/类型、fullscreen 布尔）
    """
    try:
        s = str(value)
    except Exception:
        return False
    try:
        _ = WindowURI(s)
        return True
    except Exception:
        return False
