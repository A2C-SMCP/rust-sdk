# -*- coding: utf-8 -*-
# filename: test_computer_desktop_and_edges.py
# @Time    : 2025/10/05 13:43
# @Author  : A2C-SMCP
# @Email   : qa@a2c-smcp.local
# @Software: PyTest
"""
中文：覆盖 `a2c_smcp/computer/computer.py` 缺失分支，包括：
- boot_up 渲染异常兜底（163-166）
- _on_manager_change 的 ToolListChanged/ResourceListChanged/ResourceUpdated 多分支（193-200, 205-233, 243-252）
- _acollect_window_uris（262-266）
- _arender_and_validate_server 异常分支（327-330）
- get_input/remove_input 边界返回（410, 434）

English: Cover missing branches in computer.py.
"""

from types import SimpleNamespace
from typing import Any

import pytest

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import MCPServerConfig, StdioServerConfig, StdioServerParameters
from a2c_smcp.utils.window_uri import is_window_uri


class _DummyClient:
    def __init__(self) -> None:
        self.update_called = 0
        self.refresh_called = 0

    async def emit_update_tool_list(self) -> None:
        self.update_called += 1

    async def emit_refresh_desktop(self) -> None:
        self.refresh_called += 1


@pytest.mark.asyncio
async def test_on_manager_change_tool_list_changed_triggers_emit(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = Computer(name="test", auto_connect=False, auto_reconnect=False)
    # 绑定伪客户端
    client = _DummyClient()
    comp.socketio_client = client  # weakref setter

    # 直接设置类型名不方便，使用 mcp.types 实例化更复杂；这里依赖 isinstance 仅检查类名不可靠。
    # 因 computer.py 使用 'from mcp.types import ToolListChangedNotification' 并直接 isinstance，
    # 我们改为导入真实类型来创建对象。
    from mcp.types import ToolListChangedNotification  # type: ignore

    real_msg = SimpleNamespace(root=ToolListChangedNotification())

    await comp._on_manager_change(real_msg)  # type: ignore[arg-type]
    assert client.update_called == 1


@pytest.mark.asyncio
async def test_on_manager_change_resource_list_changed_paths(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = Computer(name="test", auto_connect=False, auto_reconnect=False)
    # 绑定伪客户端
    client = _DummyClient()
    comp.socketio_client = client

    from mcp.types import ResourceListChangedNotification  # type: ignore

    # 1) _acollect_window_uris 抛异常 -> 日志后返回，不触发刷新
    async def raise_collect() -> set[str]:
        raise RuntimeError("collect error")

    monkeypatch.setattr(comp, "_acollect_window_uris", raise_collect)
    await comp._on_manager_change(SimpleNamespace(root=ResourceListChangedNotification()))  # type: ignore
    assert client.refresh_called == 0

    # 2) 集合发生变化 -> 触发刷新
    async def changed_collect() -> set[str]:
        return {"window://a"}

    monkeypatch.setattr(comp, "_acollect_window_uris", changed_collect)
    comp._windows_cache = set()  # 原为空
    await comp._on_manager_change(SimpleNamespace(root=ResourceListChangedNotification()))  # type: ignore
    assert client.refresh_called == 1

    # 3) 集合未变化 -> 不触发刷新
    comp._windows_cache = {"window://a"}

    async def same_collect() -> set[str]:
        return {"window://a"}

    monkeypatch.setattr(comp, "_acollect_window_uris", same_collect)
    await comp._on_manager_change(SimpleNamespace(root=ResourceListChangedNotification()))  # type: ignore
    assert client.refresh_called == 1  # 未增加


@pytest.mark.asyncio
async def test_on_manager_change_resource_updated_window_and_nonwindow() -> None:
    comp = Computer(name="test", auto_connect=False, auto_reconnect=False)
    client = _DummyClient()
    comp.socketio_client = client

    from mcp.types import ResourceUpdatedNotification, ResourceUpdatedNotificationParams  # type: ignore

    # 非 window 资源 -> 不触发
    non_window = ResourceUpdatedNotification(params=ResourceUpdatedNotificationParams(uri="file://x"))
    await comp._on_manager_change(SimpleNamespace(root=non_window))  # type: ignore
    assert client.refresh_called == 0

    # window 资源 -> 触发
    window = ResourceUpdatedNotification(params=ResourceUpdatedNotificationParams(uri="window://y"))
    assert is_window_uri("window://y")
    await comp._on_manager_change(SimpleNamespace(root=window))  # type: ignore
    assert client.refresh_called == 1


@pytest.mark.asyncio
async def test_acollect_window_uris_filters_and_none_manager(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = Computer(name="test", auto_connect=False, auto_reconnect=False)

    # manager 未初始化 -> 返回空
    assert await comp._acollect_window_uris() == set()

    # 注入假的 manager 和资源
    class _Res:  # noqa: B903
        def __init__(self, uri: str) -> None:
            self.uri = uri

    class _Mgr:
        async def list_windows(self, window_uri: str | None = None):  # noqa: D401
            return [("s1", _Res("window://ok")), ("s2", _Res("file://ignored"))]

    comp.mcp_manager = _Mgr()  # type: ignore
    uris = await comp._acollect_window_uris()
    assert uris == {"window://ok"}


@pytest.mark.asyncio
async def test_boot_up_render_error_path(monkeypatch: pytest.MonkeyPatch) -> None:
    # 构造一个初始 server 配置
    cfg = StdioServerConfig(name="s", server_parameters=StdioServerParameters(command="/bin/echo"))
    # 注入自定义 Computer，覆写 _config_render.arender 抛出异常
    comp = Computer(name="test", mcp_servers={cfg})

    class _CR:
        async def arender(self, *_: Any, **__: Any) -> dict:
            raise RuntimeError("render fail")

    comp._config_render = _CR()  # type: ignore

    # 替换 MCPServerManager 以避免真实调用
    class _Mgr:
        async def ainitialize(self, servers: list[MCPServerConfig]) -> None:  # noqa: D401
            # 应接受到原始 cfg，因为渲染失败走保底
            assert isinstance(servers[0], MCPServerConfig)

    monkeypatch.setattr("a2c_smcp.computer.computer.MCPServerManager", lambda *a, **k: _Mgr())

    await comp.boot_up()


@pytest.mark.asyncio
async def test_arender_and_validate_server_exception_branch(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = Computer(name="test")

    class _CR:
        async def arender(self, *_: Any, **__: Any) -> dict:
            raise RuntimeError("render fail 2")

    comp._config_render = _CR()  # type: ignore

    # 传入字典，触发渲染异常 -> except 分支记录日志后抛出
    with pytest.raises(RuntimeError):
        await comp._arender_and_validate_server({"type": "stdio", "name": "n", "server_parameters": {"command": "x"}})


def test_get_input_and_remove_input_boundaries() -> None:
    comp = Computer(name="test")
    # get_input 空字符串 -> None
    assert comp.get_input("") is None
    # remove_input 空字符串 -> False
    assert comp.remove_input("") is False
