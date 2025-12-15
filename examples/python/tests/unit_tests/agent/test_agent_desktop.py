# -*- coding: utf-8 -*-
# filename: test_agent_desktop.py
# @Time    : 2025/10/05 13:26
# @Author  : A2C-SMCP
# @Email   : qa@a2c-smcp.local
# @Software: PyTest
"""
中文：Agent端桌面协议与事件 单元测试。
English: Unit tests for Agent-side desktop protocol and events.
"""

from typing import Any

import pytest

from a2c_smcp.agent.auth import DefaultAgentAuthProvider
from a2c_smcp.agent.base import BaseAgentClient
from a2c_smcp.agent.client import AsyncSMCPAgentClient
from a2c_smcp.agent.sync_client import SMCPAgentClient
from a2c_smcp.smcp import GET_DESKTOP_EVENT, GetDeskTopReq, GetDeskTopRet


class _DummyAuth(DefaultAgentAuthProvider):
    def __init__(self) -> None:
        super().__init__(agent_id="agent-1", office_id="office-1", api_key=None)


def test_create_get_desktop_request_fields() -> None:
    auth = _DummyAuth()
    base = _make_base_like(auth)
    req = base.create_get_desktop_request("comp-1", size=3, window="window://x")
    assert req["computer"] == "comp-1"
    assert req["agent"] == "agent-1"
    assert isinstance(req["req_id"], str)
    assert req["desktop_size"] == 3
    assert req["window"] == "window://x"


def _make_base_like(auth: _DummyAuth) -> BaseAgentClient:  # type: ignore[return-type]
    class _B(BaseAgentClient):
        def emit(self, event: str, data: Any = None, namespace: str | None = None, callback: Any = None) -> Any:
            return None

        def call(self, event: str, data: Any = None, namespace: str | None = None, timeout: int = 60) -> Any:
            return None

        def register_event_handlers(self) -> None:  # pragma: no cover
            pass

    return _B(auth)


def test_sync_agent_get_desktop_invokes_call(monkeypatch: pytest.MonkeyPatch) -> None:
    auth = _DummyAuth()
    client = SMCPAgentClient(auth_provider=auth)

    called: dict[str, Any] = {}

    def fake_call(event: str, data: GetDeskTopReq, namespace: str | None = None, timeout: int = 60) -> dict:
        called["event"] = event
        called["data"] = data
        return {"desktops": ["window://a"], "req_id": data["req_id"]}

    monkeypatch.setattr(client, "call", fake_call)

    ret = client.get_desktop_from_computer("comp-1", size=2)
    assert called["event"] == GET_DESKTOP_EVENT
    assert called["data"]["computer"] == "comp-1"
    assert called["data"]["agent"] == "agent-1"
    assert ret["desktops"] == ["window://a"]


def test_sync_agent_handle_update_notification(monkeypatch: pytest.MonkeyPatch) -> None:
    auth = _DummyAuth()
    client = SMCPAgentClient(auth_provider=auth)

    fetched: dict[str, Any] = {}

    def fake_get(computer: str, **_: Any) -> GetDeskTopRet:
        fetched["computer"] = computer
        return {"desktops": ["window://b"], "req_id": "x"}

    monkeypatch.setattr(client, "get_desktop_from_computer", fake_get)

    # 直接调用内部回调
    client._on_desktop_updated({"computer": "comp-2"})
    assert fetched["computer"] == "comp-2"


@pytest.mark.asyncio
async def test_async_agent_get_desktop_invokes_call(monkeypatch: pytest.MonkeyPatch) -> None:
    auth = _DummyAuth()
    client = AsyncSMCPAgentClient(auth_provider=auth)

    called: dict[str, Any] = {}

    async def fake_call(event: str, data: GetDeskTopReq, namespace: str | None = None, timeout: int = 60) -> dict:
        called["event"] = event
        called["data"] = data
        return {"desktops": ["window://c"], "req_id": data["req_id"]}

    monkeypatch.setattr(client, "call", fake_call)

    ret = await client.get_desktop_from_computer("comp-3", size=1)
    assert called["event"] == GET_DESKTOP_EVENT
    assert called["data"]["computer"] == "comp-3"
    assert ret["desktops"] == ["window://c"]


@pytest.mark.asyncio
async def test_async_agent_handle_update_notification(monkeypatch: pytest.MonkeyPatch) -> None:
    auth = _DummyAuth()
    client = AsyncSMCPAgentClient(auth_provider=auth)

    fetched: dict[str, Any] = {}

    async def fake_get(computer: str, **_: Any) -> GetDeskTopRet:
        fetched["computer"] = computer
        return {"desktops": ["window://d"], "req_id": "y"}

    monkeypatch.setattr(client, "get_desktop_from_computer", fake_get)

    await client._on_desktop_updated({"computer": "comp-4"})
    assert fetched["computer"] == "comp-4"
