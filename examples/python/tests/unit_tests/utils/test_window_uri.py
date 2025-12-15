# -*- coding: utf-8 -*-
# filename: test_window_uri.py
# @Time    : 2025/10/01 19:46
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
WindowURI 单元测试 / Unit tests for WindowURI

测试意图 / Test intentions:
- 解析 window:// 协议的基本能力（host、路径段、查询参数）
- priority 的取值范围与类型校验（0-100）
- fullscreen 的布尔解析（多种等价表示）
- 构造函数 build_uri 的编码与可选参数拼装
- 异常分支：非法 scheme、缺失 host、非法 priority / fullscreen
- Pydantic v2 集成校验与序列化
"""

import pytest
from pydantic import BaseModel

from a2c_smcp.utils import WindowURI

# ------------------------------
# 解析相关 / Parsing tests
# ------------------------------


def test_parse_minimal():
    """最小 URI：仅 host，无路径与参数 / Minimal URI with only host"""
    u = WindowURI("window://com.example.mcp")
    assert u.mcp_id == "com.example.mcp"
    assert u.windows == []
    assert u.priority is None
    assert u.fullscreen is None


def test_parse_with_paths():
    """带多个路径段 / With multiple path segments"""
    u = WindowURI("window://com.example.mcp/dashboard/main")
    assert u.mcp_id == "com.example.mcp"
    assert u.windows == ["dashboard", "main"]


def test_parse_with_query_params():
    """带查询参数 priority 与 fullscreen / With priority and fullscreen query params"""
    u = WindowURI("window://com.example.mcp/page?priority=90&fullscreen=true")
    assert u.windows == ["page"]
    assert u.priority == 90
    assert u.fullscreen is True


# ------------------------------
# priority 解析 / Priority tests
# ------------------------------


def test_priority_bounds_and_types():
    """priority 合法边界 0 与 100 / Legal bounds 0 and 100"""
    assert WindowURI("window://x?a=1&priority=0").priority == 0
    assert WindowURI("window://x?priority=100").priority == 100

    with pytest.raises(ValueError):
        WindowURI("window://x?priority=-1")
    with pytest.raises(ValueError):
        WindowURI("window://x?priority=101")
    with pytest.raises(ValueError):
        WindowURI("window://x?priority=abc")


# ------------------------------
# fullscreen 解析 / Fullscreen tests
# ------------------------------
@pytest.mark.parametrize(
    "val,expected",
    [
        ("true", True),
        ("1", True),
        ("yes", True),
        ("on", True),
        ("false", False),
        ("0", False),
        ("no", False),
        ("off", False),
    ],
)
def test_fullscreen_variants(val, expected):
    """多种布尔等价值解析 / Multiple boolean equivalent values"""
    u = WindowURI(f"window://x?fullscreen={val}")
    assert u.fullscreen is expected


def test_fullscreen_invalid():
    """非法 fullscreen 值抛出异常 / Invalid fullscreen value raises"""
    with pytest.raises(ValueError):
        WindowURI("window://x?fullscreen=maybe")


# ------------------------------
# 构造函数 / Build tests
# ------------------------------


def test_build_uri_basic_and_roundtrip():
    """构造基本 URI 并可解析回属性 / Build basic URI and roundtrip parse"""
    u = WindowURI.build_uri(host="com.example.mcp", windows=["dashboard", "main"], priority=80, fullscreen=False)
    s = str(u)
    assert s.startswith("window://com.example.mcp/dashboard/main")
    assert "priority=80" in s and "fullscreen=false" in s

    # 反向解析 / reverse parse
    u2 = WindowURI(s)
    assert u2.mcp_id == "com.example.mcp"
    assert u2.windows == ["dashboard", "main"]
    assert u2.priority == 80
    assert u2.fullscreen is False


def test_build_uri_encoding():
    """路径段自动 URL 编码，但解析后的 parts 为原始值 / Path segments are URL-encoded, parts are decoded"""
    u = WindowURI.build_uri(host="h", windows=["A B", "c/d"])
    s = str(u)
    # 编码形式 / encoded in string form
    assert "A%20B" in s and "c%2Fd" in s
    # 解析形式 / decoded in parts
    u2 = WindowURI(s)
    assert u2.windows == ["A B", "c/d"]


def test_build_uri_optional_params():
    """未提供可选参数时不拼接查询串 / Without optional params, no query string"""
    u = WindowURI.build_uri(host="h", windows=[])
    assert str(u) == "window://h"


# ------------------------------
# 异常场景 / Error cases
# ------------------------------


def test_invalid_scheme_and_missing_host():
    """非法 scheme 与缺失 host 抛出异常 / Invalid scheme and missing host raise"""
    with pytest.raises(ValueError):
        WindowURI("http://x")
    with pytest.raises(IndexError):
        WindowURI("window://")


# ------------------------------
# Pydantic v2 集成 / Pydantic integration
# ------------------------------


def test_pydantic_integration_validate_and_serialize():
    """Pydantic 字段类型为 WindowURI，验证字符串输入与序列化输出 / Pydantic field of WindowURI"""

    class M(BaseModel):
        uri: WindowURI

    m = M(uri="window://com.example/p1/p2?priority=20&fullscreen=1")
    assert isinstance(m.uri, WindowURI)
    assert m.uri.mcp_id == "com.example"
    assert m.uri.windows == ["p1", "p2"]
    assert m.uri.priority == 20
    assert m.uri.fullscreen is True

    # 序列化 / serialization
    data = m.model_dump()
    assert isinstance(data["uri"], str)
    assert data["uri"].startswith("window://com.example/p1/p2")
