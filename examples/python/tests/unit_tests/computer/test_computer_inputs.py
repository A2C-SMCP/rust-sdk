# -*- coding: utf-8 -*-
# filename: test_computer_inputs.py
from __future__ import annotations

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import (
    MCPServerPickStringInput,
    MCPServerPromptStringInput,
)


def test_inputs_crud_and_set_uniqueness() -> None:
    comp = Computer(name="test")

    # 初始为空
    assert comp.inputs == ()

    i1 = MCPServerPromptStringInput(id="USER", description="user name", default="alice", password=False)
    i2 = MCPServerPickStringInput(id="REGION", description="region", options=["us", "eu"], default="us")

    # add
    comp.add_or_update_input(i1)
    comp.add_or_update_input(i2)
    ids = {i.id for i in comp.inputs}
    assert ids == {"USER", "REGION"}

    # update (相同id替换)
    i1b = MCPServerPromptStringInput(id="USER", description="user name2", default="bob", password=False)
    comp.add_or_update_input(i1b)
    got = comp.get_input("USER")
    assert got is not None and got.description == "user name2" and got.default == "bob"

    # list 不可变视图
    items = comp.list_inputs()
    assert isinstance(items, tuple)

    # remove
    ok = comp.remove_input("USER")
    assert ok is True
    assert comp.get_input("USER") is None

    # update_inputs(set) 覆盖并去重
    comp.update_inputs({i1, i1b, i2})  # i1与i1b相同id，最终只应保留一个
    ids = sorted([i.id for i in comp.inputs])
    assert ids == ["REGION", "USER"]
