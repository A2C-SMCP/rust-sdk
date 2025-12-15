# -*- coding: utf-8 -*-
# filename: test_resolver.py
# @Time    : 2025/9/21 17:09
# @Author  : JQQ
# @Email   : jiaqia@qknode.com
# @Software: PyCharm

import pytest
from prompt_toolkit import PromptSession

from a2c_smcp.computer.inputs.resolver import InputNotFoundError, InputResolver
from a2c_smcp.computer.mcp_clients.model import (
    MCPServerCommandInput,
    MCPServerPickStringInput,
    MCPServerPromptStringInput,
)


@pytest.mark.asyncio
async def test_resolver_prompt_path(monkeypatch):
    inputs = [MCPServerPromptStringInput(id="p1", description="desc", default="d", password=True, type="promptString")]
    r = InputResolver(inputs)

    async def fake_prompt(message: str, *, password: bool = False, default: str | None = None, session: PromptSession | None = None) -> str:
        assert "desc" in message
        assert password is True
        assert default == "d"
        assert session is None
        return "typed"

    import a2c_smcp.computer.inputs.resolver as resolver_mod

    monkeypatch.setattr(resolver_mod, "ainput_prompt", fake_prompt)

    v = await r.aresolve_by_id("p1")
    assert v == "typed"


@pytest.mark.asyncio
async def test_resolver_pick_path_default_fallback(monkeypatch):
    inputs = [MCPServerPickStringInput(id="k", description="pick one", options=["a", "b", "c"], default="b")]
    r = InputResolver(inputs)

    async def fake_pick(message, options, *, default_index=None, multi=False, session: PromptSession | None = None):
        # simulate user returns empty so resolver should use cfg.default
        return ""

    import a2c_smcp.computer.inputs.resolver as resolver_mod

    monkeypatch.setattr(resolver_mod, "ainput_pick", fake_pick)

    v = await r.aresolve_by_id("k")
    assert v == "b"


@pytest.mark.asyncio
async def test_resolver_command_path(monkeypatch):
    inputs = [MCPServerCommandInput(id="cmd", description="run", command="echo 123")]
    r = InputResolver(inputs)

    async def fake_run(command: str, *, shell: bool = True, parse: str = "raw"):
        assert command == "echo 123"
        assert shell is True
        assert parse == "raw"
        return "OK"

    import a2c_smcp.computer.inputs.resolver as resolver_mod

    monkeypatch.setattr(resolver_mod, "arun_command", fake_run)

    v = await r.aresolve_by_id("cmd")
    assert v == "OK"


@pytest.mark.asyncio
async def test_resolver_cache_and_clear(monkeypatch):
    calls = {"n": 0}
    inputs = [MCPServerPromptStringInput(id="p", description="d")]
    r = InputResolver(inputs)

    async def fake_prompt(*args, **kwargs):
        calls["n"] += 1
        return "v" + str(calls["n"])

    import a2c_smcp.computer.inputs.resolver as resolver_mod

    monkeypatch.setattr(resolver_mod, "ainput_prompt", fake_prompt)

    v1 = await r.aresolve_by_id("p")
    v2 = await r.aresolve_by_id("p")
    assert v1 == v2 == "v1"  # cached

    r.clear_cache("p")
    v3 = await r.aresolve_by_id("p")
    assert v3 == "v2"

    r.clear_cache()
    v4 = await r.aresolve_by_id("p")
    assert v4 == "v3"


@pytest.mark.asyncio
async def test_resolver_missing_id_raises():
    r = InputResolver([])
    with pytest.raises(InputNotFoundError):
        await r.aresolve_by_id("nope")
