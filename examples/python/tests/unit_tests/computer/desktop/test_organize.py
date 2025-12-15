# -*- coding: utf-8 -*-
# filename: test_organize.py
# @Time    : 2025/10/02 16:27
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
组织策略单元测试
Unit tests for desktop organizing policy

仅测试 organize_desktop 的行为，不依赖 Computer。
Only verify organize_desktop behavior, decoupled from Computer.
"""

from types import SimpleNamespace

import pytest
from mcp.types import ReadResourceResult, TextResourceContents

from a2c_smcp.computer.desktop.organize import organize_desktop


@pytest.mark.asyncio
async def test_priority_within_server_and_size_cap():
    """
    - 同一服务器内按 priority 降序
    - 全局 size 截断
    """
    # 资源详情：每个窗口都有一段不同的文本，便于断言渲染
    d1 = ReadResourceResult(contents=[TextResourceContents(text="w1-text", uri="window://srv/w1?priority=10")])
    d2 = ReadResourceResult(contents=[TextResourceContents(text="w2-text", uri="window://srv/w2?priority=90")])
    d3 = ReadResourceResult(contents=[TextResourceContents(text="w3-text", uri="window://srv/w3")])

    windows = [
        ("srv", SimpleNamespace(uri="window://srv/w1?priority=10"), d1),
        ("srv", SimpleNamespace(uri="window://srv/w2?priority=90"), d2),
        ("srv", SimpleNamespace(uri="window://srv/w3"), d3),  # 默认0
    ]
    ret = await organize_desktop(windows=windows, size=2, history=tuple())
    # 应优先 w2，再 w1；且渲染包含文本内容
    assert ret[0].startswith("window://srv/w2?priority=90") and "w2-text" in ret[0]
    assert ret[1].startswith("window://srv/w1?priority=10") and "w1-text" in ret[1]


@pytest.mark.asyncio
async def test_fullscreen_one_per_server_then_next_server():
    """
    - 若遇到 fullscreen=True 的窗口，则该 MCP 仅推入这一个；然后进入下一个 MCP
    """
    d_a1 = ReadResourceResult(contents=[TextResourceContents(uri="window://A/a1?priority=50", text="a1")])
    d_a2 = ReadResourceResult(contents=[TextResourceContents(uri="window://A/a2?fullscreen=true&priority=10", text="a2-full")])
    d_a3 = ReadResourceResult(contents=[TextResourceContents(uri="window://A/a3?priority=90", text="a3")])
    d_b1 = ReadResourceResult(contents=[TextResourceContents(uri="window://B/b1?priority=5", text="b1")])
    windows = [
        ("A", SimpleNamespace(uri="window://A/a1?priority=50"), d_a1),
        ("A", SimpleNamespace(uri="window://A/a2?fullscreen=true&priority=10"), d_a2),
        ("A", SimpleNamespace(uri="window://A/a3?priority=90"), d_a3),
        ("B", SimpleNamespace(uri="window://B/b1?priority=5"), d_b1),
    ]
    # history 让 A 在前
    history = ({"server": "A"},)
    ret = await organize_desktop(windows=windows, size=None, history=history)
    # A 只应输出 fullscreen 的 a2，然后进入 B
    assert ret[0].startswith("window://A/a2?fullscreen=true&priority=10") and "a2-full" in ret[0]
    assert any(x.startswith("window://B/b1?priority=5") and "b1" in x for x in ret)  # B 的内容随后加入


@pytest.mark.asyncio
async def test_server_order_by_recent_history():
    """
    - 服务器顺序按最近历史倒序优先
    """
    d_a = ReadResourceResult(contents=[TextResourceContents(uri="window://A/a1?priority=1", text="a")])
    d_b = ReadResourceResult(contents=[TextResourceContents(uri="window://B/b1?priority=1", text="b")])
    d_c = ReadResourceResult(contents=[TextResourceContents(uri="window://C/c1?priority=1", text="c")])
    windows = [
        ("A", SimpleNamespace(uri="window://A/a1?priority=1"), d_a),
        ("B", SimpleNamespace(uri="window://B/b1?priority=1"), d_b),
        ("C", SimpleNamespace(uri="window://C/c1?priority=1"), d_c),
    ]
    # 最近使用顺序：C -> A（B 未使用）
    history = (
        {"server": "A"},
        {"server": "C"},
    )
    ret = await organize_desktop(windows=windows, size=None, history=history)
    # C 在 A 前，剩余 B 按名称排序追加（B）
    assert ret[0].startswith("window://C/c1?priority=1") and "c" in ret[0]
    assert ret[1].startswith("window://A/a1?priority=1") and "a" in ret[1]
    assert ret[2].startswith("window://B/b1?priority=1") and "b" in ret[2]


@pytest.mark.asyncio
async def test_size_zero_returns_empty():
    ret = await organize_desktop(windows=[], size=0, history=tuple())
    assert ret == []


@pytest.mark.asyncio
async def test_skip_empty_contents_and_detail_exception():
    """
    空内容与 detail.contents 访问异常会被跳过。
    Empty contents and exception when accessing detail.contents should be skipped.
    """
    from types import SimpleNamespace as NS

    from mcp.types import ReadResourceResult, TextResourceContents

    # 空 contents -> 被跳过
    empty_detail = ReadResourceResult(contents=[])
    # 访问 contents 抛出异常 -> 被跳过

    class BadDetail:
        def __getattribute__(self, name):
            if name == "contents":
                raise RuntimeError("boom")
            return super().__getattribute__(name)

    ok_detail = ReadResourceResult(contents=[TextResourceContents(text="ok", uri="window://S/ok")])

    windows = [
        ("S", NS(uri="window://S/empty"), empty_detail),
        ("S", NS(uri="window://S/bad"), BadDetail()),
        ("S", NS(uri="window://S/ok"), ok_detail),
    ]
    ret = await organize_desktop(windows=windows, size=None, history=tuple())
    # 仅保留 ok 窗口
    assert len(ret) == 1 and ret[0].startswith("window://S/ok") and "ok" in ret[0]


@pytest.mark.asyncio
async def test_invalid_window_uri_is_skipped():
    """
    非法的 URI 解析失败会被跳过。
    Invalid WindowURI should be skipped when parsing fails.
    """
    from types import SimpleNamespace as NS

    from mcp.types import ReadResourceResult, TextResourceContents

    bad_detail = ReadResourceResult(contents=[TextResourceContents(text="bad", uri="bad://whatever")])
    good_detail = ReadResourceResult(contents=[TextResourceContents(text="good", uri="window://G/good")])

    windows = [
        ("G", NS(uri=":::this_is_not_a_uri"), bad_detail),  # 将触发 WindowURI 解析失败
        ("G", NS(uri="window://G/good"), good_detail),
    ]
    ret = await organize_desktop(windows=windows, size=None, history=tuple())
    # 仅包含合法 URI
    assert len(ret) == 1 and ret[0].startswith("window://G/good") and "good" in ret[0]


@pytest.mark.asyncio
async def test_render_blob_and_unknown_and_render_exception():
    """
    渲染分支：
    - BlobResourceContents -> 仅记录日志，不加入文本
    - 未知类型 -> 仅记录错误日志
    - 渲染异常 -> 捕获并回退为 URI 字符串
    Rendering branches: Blob, unknown type, and exception fallback.
    """
    from types import SimpleNamespace as NS

    from mcp.types import BlobResourceContents, ReadResourceResult, TextResourceContents

    # Blob + 文本混合 -> 返回包含 URI 与文本（Blob 不追加文本）
    detail_blob = ReadResourceResult(
        contents=[
            BlobResourceContents(uri="window://R/blob", mimeType="application/octet-stream", blob="eHg="),
            TextResourceContents(uri="window://R/blob", text="t1"),
        ],
    )
    # 为了触发 _render 的未知类型分支，先创建合法实例，再在运行期覆盖 contents
    detail_unknown = ReadResourceResult(contents=[TextResourceContents(uri="window://R/u", text="t2")])
    detail_unknown.contents = [object(), TextResourceContents(uri="window://R/u", text="t2")]
    # 渲染异常：contents 不是可迭代对象 -> TypeError -> except 分支返回纯 URI
    detail_exception = ReadResourceResult(contents=[TextResourceContents(uri="window://R/ex", text="ex")])
    detail_exception.contents = 123  # 非可迭代

    windows = [
        ("R", NS(uri="window://R/blob"), detail_blob),
        ("R", NS(uri="window://R/u"), detail_unknown),
        ("R", NS(uri="window://R/ex"), detail_exception),
    ]
    ret = await organize_desktop(windows=windows, size=None, history=tuple())
    # 断言：
    # 1) blob 项包含文本 t1（Blob 本身不拼接文本）
    assert any(x.startswith("window://R/blob") and "t1" in x for x in ret)
    # 2) unknown 类型不影响已有文本 t2
    assert any(x.startswith("window://R/u") and "t2" in x for x in ret)
    # 3) 渲染异常时仅返回 URI
    assert any(x == "window://R/ex" for x in ret)


@pytest.mark.asyncio
async def test_server_level_cap_breaks_iteration():
    """
    当第一个服务器已满足 size 上限时，后续服务器在服务器层级立即中断。
    When cap reached after first server, loop breaks before processing later servers.
    """
    from types import SimpleNamespace as NS

    from mcp.types import ReadResourceResult, TextResourceContents

    d_a = ReadResourceResult(contents=[TextResourceContents(uri="window://A/a", text="a")])
    d_b = ReadResourceResult(contents=[TextResourceContents(uri="window://B/b", text="b")])
    windows = [
        ("A", NS(uri="window://A/a"), d_a),
        ("B", NS(uri="window://B/b"), d_b),
    ]
    # 使用 history 让 A 在前，size=1 使得在进入 B 时触发服务器层级的 break
    ret = await organize_desktop(windows=windows, size=1, history=({"server": "A"},))
    assert len(ret) == 1 and ret[0].startswith("window://A/a")
