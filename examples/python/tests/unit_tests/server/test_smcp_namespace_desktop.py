# -*- coding: utf-8 -*-
# filename: test_smcp_namespace_desktop.py
# @Time    : 2025/10/05 13:28
# @Author  : A2C-SMCP
# @Email   : qa@a2c-smcp.local
# @Software: PyTest
"""
中文：SMCPNamespace 桌面协议相关的单元测试。
English: Unit tests for SMCPNamespace desktop-related behaviors.
"""

from unittest.mock import AsyncMock

import pytest

from a2c_smcp.server import SMCPNamespace
from a2c_smcp.smcp import GET_DESKTOP_EVENT, GetDeskTopReq


@pytest.mark.asyncio
async def test_on_client_get_desktop_and_update_broadcast(monkeypatch: pytest.MonkeyPatch):
    ns = SMCPNamespace(auth_provider=AsyncMock())
    ns.server = AsyncMock()

    agent_sid = "a2"
    agent_name = "an2"
    comp_sid = "c2"
    comp_name = "cn2"
    sess_agent = {"role": "agent", "office_id": "room2"}
    sess_comp = {"role": "computer", "office_id": "room2"}

    # get_session: 查询 computer_sid 与 agent_sid
    async def _get_session(sid):
        return sess_comp if sid == comp_sid else sess_agent

    ns.get_session = AsyncMock(side_effect=_get_session)
    ns._name_to_sid_map = {agent_name: agent_sid, comp_name: comp_sid}
    # 转发到 client:get_desktop
    ns.call = AsyncMock(return_value={"desktops": ["window://m"], "req_id": "rid"})

    req: GetDeskTopReq = {"computer": comp_name, "robot_id": agent_name, "req_id": "rid", "desktop_size": 1}
    ret = await ns.on_client_get_desktop(agent_sid, req)
    assert ret["desktops"] == ["window://m"]
    ns.call.assert_awaited_with(GET_DESKTOP_EVENT, req, to=comp_sid, namespace=ns.namespace)

    # 广播 update_desktop
    ns.get_session = AsyncMock(return_value=sess_comp)
    ns.emit = AsyncMock()
    await ns.on_server_update_desktop(comp_sid, {"computer": comp_name})
    ns.emit.assert_awaited()
