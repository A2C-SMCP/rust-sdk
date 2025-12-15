# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/utils.py
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from a2c_smcp.server.utils import (
    aget_all_sessions_in_office,
    aget_computers_in_office,
    get_all_sessions_in_office,
    get_computers_in_office,
)


@pytest.mark.asyncio
async def test_aget_get_computers_in_office_async_paths():
    sio = AsyncMock()
    sio.manager = MagicMock()
    # 无参与者
    sio.manager.get_participants.return_value = []
    assert await aget_computers_in_office("room1", sio) == []

    # 有参与者（包含 Agent 自身 与 一个 Computer 与 一个异常）
    sio.manager.get_participants.return_value = [("room1", "eio_room1"), ("c1", "eio_c1"), ("bad", "eio_bao")]

    async def _get_session(sid, namespace=None):  # noqa: ANN001
        if sid == "c1":
            return {"sid": "c1", "name": "C1", "role": "computer", "office_id": "room1"}
        raise RuntimeError("boom")

    sio.get_session.side_effect = _get_session

    res = await aget_computers_in_office("room1", sio)
    assert len(res) == 1 and res[0]["sid"] == "c1"


def test_get_computers_in_office_sync_paths():
    sio = MagicMock()
    # 无参与者
    sio.manager.get_participants.return_value = []
    assert get_computers_in_office("room1", sio) == []

    # 有参与者
    sio.manager.get_participants.return_value = [("room1", "eio_room1"), ("c1", "eio_c1"), ("x", "eio_x")]

    def _get_session(sid, namespace=None):  # noqa: ANN001
        if sid == "c1":
            return {"sid": "c1", "name": "C1", "role": "computer", "office_id": "room1"}
        raise RuntimeError("bad")

    sio.get_session.side_effect = _get_session

    res = get_computers_in_office("room1", sio)
    assert len(res) == 1 and res[0]["sid"] == "c1"


@pytest.mark.asyncio
async def test_aget_and_get_all_sessions_in_office():
    # async
    sio = AsyncMock()
    sio.manager = MagicMock()
    sio.manager.get_participants.return_value = []
    assert await aget_all_sessions_in_office("r", sio) == []

    sio.manager.get_participants.return_value = [("r", "eio_r"), ("a", "eio_a"), ("b", "eio_b")]

    async def _get_session(sid, namespace=None):  # noqa: ANN001
        if sid == "a":
            return {"sid": "a", "name": "A", "role": "agent", "office_id": "r"}
        if sid == "b":
            return {"sid": "b", "name": "B", "role": "computer", "office_id": "r"}
        return None

    sio.get_session.side_effect = _get_session
    out = await aget_all_sessions_in_office("r", sio)
    assert {s["sid"] for s in out} == {"a", "b"}

    # sync
    sio2 = MagicMock()
    sio2.manager.get_participants.return_value = []
    assert get_all_sessions_in_office("r", sio2) == []

    sio2.manager.get_participants.return_value = [("r", "eio_r"), ("a", "eio_a"), ("b", "eio_b")]

    def _get_session2(sid, namespace=None):  # noqa: ANN001
        if sid == "a":
            return {"sid": "a", "name": "A", "role": "agent", "office_id": "r"}
        if sid == "b":
            return {"sid": "b", "name": "B", "role": "computer", "office_id": "r"}
        return None

    sio2.get_session.side_effect = _get_session2
    out2 = get_all_sessions_in_office("r", sio2)
    assert {s["sid"] for s in out2} == {"a", "b"}
