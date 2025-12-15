# -*- coding: utf-8 -*-
# filename: test_computer_exceptions.py
from __future__ import annotations

import pytest
from mcp import StdioServerParameters

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import StdioServerConfig


@pytest.mark.asyncio
async def test_arender_and_validate_server_missing_input_keeps_original() -> None:
    """
    当出现未定义的 ${input:NOT_DEFINED} 时，渲染器按设计返回原值字符串而非抛错，
    因此应成功返回校验后的模型，且 env 中仍保留占位符字符串。
    覆盖 computer.py 中对未定义输入的 warning 日志路径。
    """
    comp = Computer(name="test", inputs=[], mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    params = StdioServerParameters(
        command="echo",
        args=["hello"],
        env={"FOO": "${input:NOT_DEFINED}"},
        cwd=None,
        encoding="utf-8",
        encoding_error_handler="strict",
    )
    cfg = StdioServerConfig(name="bad", server_parameters=params)

    validated = await comp._arender_and_validate_server(cfg)  # type: ignore[attr-defined]
    assert validated.server_parameters.env is not None
    assert validated.server_parameters.env.get("FOO") == "${input:NOT_DEFINED}"


@pytest.mark.asyncio
async def test_boot_up_render_error_keeps_original_config(monkeypatch: pytest.MonkeyPatch) -> None:
    """
    让渲染器抛出异常，覆盖 boot_up 中的异常分支（115-118），应当保留原配置继续初始化。
    """
    comp = Computer(name="test", inputs=[], mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    params = StdioServerParameters(
        command="echo",
        args=["hello"],
        env=None,
        cwd=None,
        encoding="utf-8",
        encoding_error_handler="strict",
    )
    cfg = StdioServerConfig(name="keep", server_parameters=params)

    # 往内部 mcp_servers 放一个配置
    comp._mcp_servers = {cfg}  # type: ignore[attr-defined]

    # 让 arender 抛出异常
    async def boom(*args, **kwargs):  # noqa: ANN001, ANN003
        raise RuntimeError("render failed")

    comp._config_render.arender = boom  # type: ignore[assignment]

    await comp.boot_up()
    # 成功则说明吞掉异常并保留了原配置，manager 初始化完毕
    assert comp.mcp_manager is not None
