# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/base.py
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from a2c_smcp.server.auth import AuthenticationProvider
from a2c_smcp.server.base import BaseNamespace


class DummyAuth(AuthenticationProvider):
    async def authenticate(self, sio, environ, auth, headers):
        return True


@pytest.fixture
def ns():
    return BaseNamespace("/ns", DummyAuth())


@pytest.mark.asyncio
async def test_on_connect_success_and_extract_headers_paths(ns):
    server = AsyncMock()
    server.app = MagicMock()

    # 路径1：从 asgi.scope.headers
    environ1 = {"asgi": {"scope": {"headers": [(b"k", b"v")]}}}
    ns.server = server
    ok = await ns.on_connect("sid1", environ1, None)
    assert ok is True

    # 路径2：从 HTTP_HEADERS
    class Auth2(DummyAuth):
        async def authenticate(self, sio, environ, auth, headers):  # noqa: D401
            assert headers == [(b"h1", b"v1")]
            return True

    ns2 = BaseNamespace("/ns", Auth2())
    ns2.server = server
    environ2 = {"HTTP_HEADERS": [(b"h1", b"v1")]}
    ok2 = await ns2.on_connect("sid2", environ2, None)
    assert ok2 is True


@pytest.mark.asyncio
async def test_on_connect_auth_failed(ns):
    class BadAuth(DummyAuth):
        async def authenticate(self, *a, **k):  # noqa: ANN001, D401
            return False

    ns_bad = BaseNamespace("/ns", BadAuth())
    ns_bad.server = AsyncMock()
    with pytest.raises(ConnectionRefusedError):
        await ns_bad.on_connect("sidX", {"asgi": {"scope": {"headers": []}}}, None)


@pytest.mark.asyncio
async def test_on_disconnect_and_trigger_event_translate(ns, monkeypatch):
    ns.server = AsyncMock()
    # rooms: 包含自身房间与其它房间
    ns.rooms = MagicMock(return_value=["sid3", "roomA", "roomB"])  # 自身房间名等于 sid
    ns.leave_room = AsyncMock()
    await ns.on_disconnect("sid3")
    ns.leave_room.assert_any_await("sid3", "roomA")
    ns.leave_room.assert_any_await("sid3", "roomB")

    # trigger_event 将 : 替换为 _，并调用父类 AsyncNamespace.trigger_event
    from socketio import AsyncNamespace

    parent = AsyncMock()
    # 使用 monkeypatch 临时替换，避免污染全局，影响后续集成测试的事件分发
    monkeypatch.setattr(AsyncNamespace, "trigger_event", parent, raising=True)
    await ns.trigger_event("server:join_office", 1, 2)
    parent.assert_awaited_with("server_join_office", 1, 2)
