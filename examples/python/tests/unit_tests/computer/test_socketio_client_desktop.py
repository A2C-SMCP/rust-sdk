# -*- coding: utf-8 -*-
# filename: test_socketio_client_desktop.py
# @Time    : 2025/10/05 13:32
# @Author  : A2C-SMCP
# @Email   : qa@a2c-smcp.local
# @Software: PyTest
"""
中文：Computer Socket.IO 客户端的桌面相关单元测试。
English: Unit tests for Computer Socket.IO client's desktop features.
"""

from typing import Any

import pytest

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.socketio.client import SMCPComputerClient
from a2c_smcp.smcp import SMCP_NAMESPACE, UPDATE_DESKTOP_EVENT, GetDeskTopReq, GetDeskTopRet


class _DummyComputer(Computer):
    async def get_desktop(self, size: int | None = None, window_uri: str | None = None) -> list[str]:  # type: ignore[override]
        return ["window://dummy\n\ncontent"]


@pytest.mark.asyncio
async def test_on_get_desktop_returns_expected(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = _DummyComputer(name="comp-sid-1", auto_connect=False, auto_reconnect=False)
    client = SMCPComputerClient(computer=comp)
    # 准备 office 情况
    client.office_id = "office-1"

    req: GetDeskTopReq = {
        "computer": "comp-sid-1",
        "agent": "office-1",
        "req_id": "rid-1",
        "desktop_size": 1,
    }

    ret: GetDeskTopRet = await client.on_get_desktop(req)
    assert ret["req_id"] == "rid-1"
    assert isinstance(ret["desktops"], list) and ret["desktops"]


@pytest.mark.asyncio
async def test_emit_refresh_desktop_emits(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = _DummyComputer(name="test", auto_connect=False, auto_reconnect=False)
    client = SMCPComputerClient(computer=comp)
    client.office_id = "office-1"

    called: dict[str, Any] = {}

    async def fake_emit(self, event: str, data: Any = None, namespace: str | None = None, callback: Any = None) -> None:  # noqa: D401
        called["event"] = event
        called["data"] = data

    monkeypatch.setattr(SMCPComputerClient, "emit", fake_emit)

    await client.emit_refresh_desktop()
    assert called["event"] == UPDATE_DESKTOP_EVENT
    assert called["data"]["computer"] == "test"
