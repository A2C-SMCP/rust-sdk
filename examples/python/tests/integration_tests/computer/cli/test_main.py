# -*- coding: utf-8 -*-
# filename: test_main_integration.py
# 基于真实 stdio MCP Server 的 CLI 集成测试
from __future__ import annotations

import json
from collections.abc import Callable
from contextlib import contextmanager
from typing import Any

import pytest
from mcp import StdioServerParameters
from typer.testing import CliRunner

import a2c_smcp.computer.cli.main as cli_main
from a2c_smcp.computer.cli.main import _interactive_loop
from a2c_smcp.computer.computer import Computer


class FakePromptSession:
    def __init__(self, commands: list[str]) -> None:
        self._commands = commands

    async def prompt_async(self, *_: str, **__: Any) -> str:
        if not self._commands:
            raise EOFError
        return self._commands.pop(0)


@contextmanager
def no_patch_stdout():
    yield


@pytest.mark.asyncio
async def test_cli_with_real_stdio(stdio_params: StdioServerParameters) -> None:
    """
    集成测试：通过 CLI 交互完成以下流程（使用真实 stdio MCP server 参数）：
    1) 添加 server 配置（disabled=false）
    2) 启动该 server
    3) 列出工具与状态
    4) 停止该 server
    5) 退出
    期望：流程执行无异常。
    """
    server_cfg = {
        "name": "it-stdio",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {},
        "server_parameters": json.loads(stdio_params.model_dump_json()),
    }

    commands = [
        f"server add {json.dumps(server_cfg)}",
        "start it-stdio",
        "tools",
        "status",
        "stop it-stdio",
        "exit",
    ]

    # Patch interactive IO
    cli_main.PromptSession = lambda: FakePromptSession(commands)  # type: ignore
    cli_main.patch_stdout = lambda raw: no_patch_stdout()  # type: ignore

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    await _interactive_loop(comp)


class FakeSMCPClient:
    def __init__(self, *args: Any, **kwargs: Any) -> None:  # noqa: D401
        self.connected = False
        self.connect_args: dict[str, Any] | None = None
        FakeSMCPClient.last = self  # type: ignore[attr-defined]

    async def connect(self, url: str, auth: dict[str, Any] | None = None, headers: dict[str, Any] | None = None) -> None:
        self.connected = True
        self.connect_args = {"url": url, "auth": auth, "headers": headers}


@pytest.mark.asyncio
async def test_cli_socket_connect_guided_inputs_without_real_network(monkeypatch: pytest.MonkeyPatch) -> None:
    """
    集成层面验证 CLI 的交互式引导输入 URL/Auth/Headers 的行为，但不依赖真实网络。
    """
    # Patch client to fake
    cli_main.SMCPComputerClient = FakeSMCPClient  # type: ignore

    commands = [
        "socket connect",
        "http://127.0.0.1:9000",
        "apikey:xyz",
        "app:demo,build:42",
        "exit",
    ]

    # Patch interactive IO
    cli_main.PromptSession = lambda: FakePromptSession(commands)  # type: ignore
    cli_main.patch_stdout = lambda raw: no_patch_stdout()  # type: ignore

    comp = Computer(name="test", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)

    last: FakeSMCPClient = FakeSMCPClient.last  # type: ignore[assignment]
    assert last.connected is True
    assert last.connect_args == {
        "url": "http://127.0.0.1:9000",
        "auth": {"apikey": "xyz"},
        "headers": {"app": "demo", "build": "42"},
    }


# ------------------------------
# 测试 --computer-factory 集成路径（通过 Typer CLI）
# ------------------------------


class _DummyInteractive:
    called: bool = False
    last_comp: Any | None = None

    @classmethod
    async def coro(cls, comp: Any, init_client: Any | None = None) -> None:  # noqa: ARG003
        cls.called = True
        cls.last_comp = comp


class _FakeComputer:
    """轻量 Computer 替身，匹配构造参数与异步上下文协议。"""

    def __init__(
        self,
        name: str,
        inputs: set[Any] | None = None,
        mcp_servers: set[Any] | None = None,
        auto_connect: bool = True,
        auto_reconnect: bool = True,
        confirm_callback: Callable[[str, str, str, dict], bool] | None = None,
        input_resolver: Any | None = None,
    ) -> None:
        self.init_args = {
            "name": name,
            "inputs": inputs,
            "mcp_servers": mcp_servers,
            "auto_connect": auto_connect,
            "auto_reconnect": auto_reconnect,
            "confirm_callback": confirm_callback,
            "input_resolver": input_resolver,
        }

    async def __aenter__(self) -> _FakeComputer:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:  # noqa: ANN001
        return None


@pytest.fixture(autouse=True)
def _reset_dummy_interactive() -> None:
    _DummyInteractive.called = False
    _DummyInteractive.last_comp = None


def test_cli_root_with_computer_factory(monkeypatch: pytest.MonkeyPatch) -> None:
    """根路径（无子命令）携带 --computer-factory 时应调用解析的工厂。"""
    runner = CliRunner()

    # 工厂: 返回 _FakeComputer，并计数
    calls: dict[str, Any] = {"count": 0}

    def factory(**kwargs: Any) -> _FakeComputer:
        calls["count"] += 1
        return _FakeComputer(**kwargs)

    monkeypatch.setattr(cli_main, "resolve_import_target", lambda s: factory, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", _DummyInteractive.coro, raising=True)

    result = runner.invoke(cli_main.app, ["--computer-factory", "pkg.mod:factory"])  # noqa: S603

    assert result.exit_code == 0
    assert calls["count"] == 1
    assert _DummyInteractive.called is True
    assert isinstance(_DummyInteractive.last_comp, _FakeComputer)


def test_cli_run_with_computer_factory_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    """当解析到的目标不可调用时，CLI 应回退到默认 Computer；为便于断言，替换为 _FakeComputer。"""
    runner = CliRunner()

    monkeypatch.setattr(cli_main, "resolve_import_target", lambda s: object(), raising=True)
    monkeypatch.setattr(cli_main, "Computer", _FakeComputer, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", _DummyInteractive.coro, raising=True)

    result = runner.invoke(
        cli_main.app,
        [
            "run",
            "--computer-factory",
            "x.y:bad",
            "--auto-connect",
            "--auto-reconnect",
        ],
    )

    assert result.exit_code == 0
    assert _DummyInteractive.called is True
    assert isinstance(_DummyInteractive.last_comp, _FakeComputer)
    assert _DummyInteractive.last_comp.init_args["auto_connect"] is True
    assert _DummyInteractive.last_comp.init_args["auto_reconnect"] is True
