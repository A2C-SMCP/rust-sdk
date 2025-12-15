# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/sync_base.py
"""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from a2c_smcp.server.sync_base import SyncBaseNamespace


class _DummyAuthProv:
    def authenticate(self, sio, environ, auth, headers):
        return True


def _mk_environ_with_headers(headers: list[tuple[bytes, bytes]]):
    return {"asgi": {"scope": {"headers": headers}}}


def test_extract_headers_from_asgi_and_fallback():
    # from asgi.scope.headers
    assert SyncBaseNamespace._extract_headers(_mk_environ_with_headers([(b"x", b"1")])) == [
        (b"x", b"1"),
    ]
    # fallback from HTTP_HEADERS
    env = {"HTTP_HEADERS": [(b"y", b"2")]}
    assert SyncBaseNamespace._extract_headers(env) == [(b"y", b"2")]


def test_on_connect_success_and_failure_paths():
    ns = SyncBaseNamespace("/ns", _DummyAuthProv())

    # 准备 server 与成功认证
    server = MagicMock()
    server.app = MagicMock()
    server.app.state = MagicMock()
    server.app.state.agent_id = "A"
    ns.server = server

    environ = _mk_environ_with_headers([(b"x-api-key", b"ok")])
    ok = ns.on_connect("sid1", environ, None)
    assert ok is True

    # 认证失败
    bad_auth = _DummyAuthProv()
    bad_auth.authenticate = MagicMock(return_value=False)
    ns_bad = SyncBaseNamespace("/ns", bad_auth)
    ns_bad.server = server
    with pytest.raises(ConnectionRefusedError):
        ns_bad.on_connect("sid2", environ, None)

    # 认证过程中抛出异常 -> 包装为 ConnectionRefusedError
    err_auth = _DummyAuthProv()

    def boom(*_a, **_k):
        raise RuntimeError("boom")

    err_auth.authenticate = boom  # type: ignore[assignment]
    ns_err = SyncBaseNamespace("/ns", err_auth)
    ns_err.server = server
    with pytest.raises(ConnectionRefusedError):
        ns_err.on_connect("sid3", environ, None)


def test_on_disconnect_leaves_all_non_self_rooms():
    ns = SyncBaseNamespace("/ns", _DummyAuthProv())

    # rooms 返回多个，跳过与 sid 相同的房间
    ns.rooms = MagicMock(return_value=["sidX", "roomA", "roomB"])  # type: ignore[attr-defined]
    ns.leave_room = MagicMock()  # type: ignore[attr-defined]
    ns.server = MagicMock()

    ns.on_disconnect("sidX")

    ns.leave_room.assert_any_call("sidX", "roomA")
    ns.leave_room.assert_any_call("sidX", "roomB")
    assert ns.leave_room.call_count == 2


def test_trigger_event_replaces_colon_with_underscore(monkeypatch):
    ns = SyncBaseNamespace("/ns", _DummyAuthProv())

    called = {}

    def fake_trigger(self, event, *args, **kwargs):  # socketio.Namespace.trigger_event
        called["event"] = event
        called["args"] = args
        called["kwargs"] = kwargs
        return "ok"

    # 直接在类上 monkeypatch 父类 Namespace.trigger_event
    from socketio import Namespace as _SNamespace

    monkeypatch.setattr(_SNamespace, "trigger_event", fake_trigger)
    ret = ns.trigger_event("a:b:c", 1, 2)

    assert ret == "ok"
    assert called["event"] == "a_b_c"
    assert called["args"] == (1, 2)
    assert called["kwargs"] == {}
