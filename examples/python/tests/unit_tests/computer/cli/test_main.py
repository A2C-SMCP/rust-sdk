"""
合并版 CLI 单测，包含基础与扩展用例
"""

from __future__ import annotations

import json
from collections.abc import Callable
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest

import a2c_smcp.computer.cli.main as cli_main
from a2c_smcp.computer.cli.main import _interactive_loop
from a2c_smcp.computer.computer import Computer


class DummyInteractive:
    called: bool = False
    last_comp: Any | None = None
    last_init_client: Any | None = None

    @classmethod
    async def coro(cls, comp: Any, init_client: Any | None = None) -> None:  # matches _interactive_loop signature
        cls.called = True
        cls.last_comp = comp
        cls.last_init_client = init_client


class FakeComputer:
    """A lightweight fake that matches Computer's init signature and async context manager."""

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
            "inputs": inputs,
            "mcp_servers": mcp_servers,
            "auto_connect": auto_connect,
            "auto_reconnect": auto_reconnect,
            "confirm_callback": confirm_callback,
            "input_resolver": input_resolver,
        }

    async def __aenter__(self) -> FakeComputer:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:  # noqa: ANN001
        return None


def test_run_impl_uses_default_computer_when_no_factory(monkeypatch: pytest.MonkeyPatch) -> None:
    # Patch Computer to our fake and _interactive_loop to a dummy coro
    monkeypatch.setattr(cli_main, "Computer", FakeComputer, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", DummyInteractive.coro, raising=True)

    # Call implementation with no factory and no side-effect options
    cli_main._run_impl(
        auto_connect=True,
        auto_reconnect=True,
        url=None,
        namespace=None,
        auth=None,
        headers=None,
        computer_factory=None,
        config=None,
        inputs=None,
    )

    assert DummyInteractive.called is True
    assert isinstance(DummyInteractive.last_comp, FakeComputer)
    assert DummyInteractive.last_comp.init_args["auto_connect"] is True
    assert DummyInteractive.last_comp.init_args["auto_reconnect"] is True


def test_run_impl_uses_resolved_factory(monkeypatch: pytest.MonkeyPatch) -> None:
    # Prepare a factory that returns our FakeComputer
    calls: dict[str, Any] = {"count": 0}

    def factory(**kwargs: Any) -> FakeComputer:
        calls["count"] += 1
        return FakeComputer(**kwargs)

    # Patch resolver to return our factory; patch interactive loop to avoid blocking
    monkeypatch.setattr(cli_main, "resolve_import_target", lambda s: factory, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", DummyInteractive.coro, raising=True)

    cli_main._run_impl(
        auto_connect=False,
        auto_reconnect=False,
        url=None,
        namespace=None,
        auth=None,
        headers=None,
        computer_factory="some.module:factory",
        config=None,
        inputs=None,
    )

    assert calls["count"] == 1
    assert isinstance(DummyInteractive.last_comp, FakeComputer)
    assert DummyInteractive.last_comp.init_args["auto_connect"] is False
    assert DummyInteractive.last_comp.init_args["auto_reconnect"] is False


def test_run_impl_factory_not_callable_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    # Make resolve_import_target return a non-callable
    monkeypatch.setattr(cli_main, "resolve_import_target", lambda s: object(), raising=True)
    # Patch Computer fallback to our FakeComputer
    monkeypatch.setattr(cli_main, "Computer", FakeComputer, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", DummyInteractive.coro, raising=True)

    cli_main._run_impl(
        auto_connect=True,
        auto_reconnect=True,
        url=None,
        namespace=None,
        auth=None,
        headers=None,
        computer_factory="x.y:bad",
        config=None,
        inputs=None,
    )

    assert isinstance(DummyInteractive.last_comp, FakeComputer)


def test_run_impl_resolve_error_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    def _raise(_: str) -> Any:
        raise ValueError("boom")

    monkeypatch.setattr(cli_main, "resolve_import_target", _raise, raising=True)
    monkeypatch.setattr(cli_main, "Computer", FakeComputer, raising=True)
    monkeypatch.setattr(cli_main, "_interactive_loop", DummyInteractive.coro, raising=True)

    cli_main._run_impl(
        auto_connect=True,
        auto_reconnect=True,
        url=None,
        namespace=None,
        auth=None,
        headers=None,
        computer_factory="x.y:z",
        config=None,
        inputs=None,
    )

    assert isinstance(DummyInteractive.last_comp, FakeComputer)


class FakePromptSession:
    """Feed scripted inputs to the interactive loop."""

    def __init__(self, commands: list[str]) -> None:
        self._commands = commands

    async def prompt_async(self, *_: str, **__: Any) -> str:  # noqa: D401
        if not self._commands:
            raise EOFError
        return self._commands.pop(0)


@contextmanager
def no_patch_stdout():
    """No-op context manager to replace patch_stdout() in tests."""
    yield


@pytest.mark.asyncio
async def test_interactive_help_and_exit(monkeypatch: pytest.MonkeyPatch) -> None:
    commands = [
        "help",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_server_add_exception_and_rm_with_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 server add 的异常打印分支，以及 rm 时已连接触发 emit 分支。"""

    # server 配置文件
    server_file = tmp_path / "server.json"
    server_file.write_text(
        json.dumps(
            {
                "name": "s2",
                "type": "stdio",
                "disabled": True,
                "forbidden_tools": [],
                "tool_meta": {},
                "server_parameters": {
                    "command": "echo",
                    "args": [],
                    "env": None,
                    "cwd": None,
                    "encoding": "utf-8",
                    "encoding_error_handler": "strict",
                },
            },
        ),
        encoding="utf-8",
    )

    # 指令：先连接，再尝试 add 触发异常，再 rm 触发已连接 emit
    commands = [
        "socket connect http://localhost:9001",
        f"server add @{server_file}",
        "server rm s2",
        "exit",
    ]

    # 准备 comp 与补丁
    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    async def _raise_add(*args: Any, **kwargs: Any) -> None:  # noqa: ANN001
        raise RuntimeError("boom")

    monkeypatch.setattr(comp, "aadd_or_aupdate_server", _raise_add)
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_inputs_load_usage_and_success_with_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 inputs load 的用法提示与成功路径（含 emit）。"""
    inputs_file = tmp_path / "inputs.json"
    inputs_file.write_text(
        json.dumps(
            [
                {"id": "J1", "type": "promptString", "description": "d", "default": "v"},
            ],
        ),
        encoding="utf-8",
    )

    commands = [
        "inputs load",  # 触发用法提示
        "socket connect http://localhost:9002",
        f"inputs load @{inputs_file}",  # 成功并触发 emit
        "exit",
    ]

    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_socket_connect_guided_parse_error(monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖交互式 socket connect 的参数解析失败分支。"""
    commands = [
        "socket connect",
        "http://localhost:9003",
        "bad_auth_kv",  # 无效，触发 parse_kv_pairs 异常
        "exit",
    ]

    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_inputs_value_print_json_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    """通过让 console.print_json 抛异常覆盖 repr 回退分支。"""
    import a2c_smcp.computer.cli.utils as cli_utils

    commands = [
        'inputs add {"id":"Z","type":"promptString","description":"d"}',
        'inputs value set Z {"x":1}',  # 设置为字典
        "inputs value get Z",  # 获取时让 print_json 抛错
        "exit",
    ]

    def _raise_print_json(*args: Any, **kwargs: Any) -> None:  # noqa: ANN001
        raise ValueError("no json")

    monkeypatch.setattr(cli_utils.console, "print_json", _raise_print_json, raising=True)
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


def test_run_impl_inputs_and_servers_single_object(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 _run_impl 的 inputs/config 单对象路径。"""
    # 立即退出的交互
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(["exit"]))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    inputs_file = tmp_path / "i.json"
    inputs_file.write_text(
        json.dumps({"id": "SO", "type": "promptString", "description": "d", "default": "a"}),
        encoding="utf-8",
    )

    server_file = tmp_path / "s.json"
    server_file.write_text(
        json.dumps(
            {
                "name": "solo",
                "type": "stdio",
                "disabled": True,
                "forbidden_tools": [],
                "tool_meta": {},
                "server_parameters": {
                    "command": "echo",
                    "args": [],
                    "env": None,
                    "cwd": None,
                    "encoding": "utf-8",
                    "encoding_error_handler": "strict",
                },
            },
        ),
        encoding="utf-8",
    )

    cli_main._run_impl(
        auto_connect=False,
        auto_reconnect=False,
        url=None,
        namespace=cli_main.SMCP_NAMESPACE,
        auth=None,
        headers=None,
        computer_factory=None,
        config=str(server_file),  # 单对象
        inputs=str(inputs_file),  # 单对象
    )


@pytest.mark.asyncio
async def test_cover_remaining_branches(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 interactive_impl.py 中剩余未命中的分支。"""
    # 为 inputs update @file 准备文件（列表）
    upd_file = tmp_path / "upd.json"
    upd_file.write_text(
        json.dumps(
            [
                {"id": "U1", "type": "promptString", "description": "d"},
                {"id": "U2", "type": "promptString", "description": "d2"},
            ],
        ),
        encoding="utf-8",
    )

    # 命令序列
    commands = [
        # 添加 server 后立刻 mcp，覆盖 servers 循环
        '{"cmd":"server add inline"}',  # 占位，下一行是真正的 add
        'server add {"name":"m1","type":"stdio","disabled":true,"forbidden_tools":[],"tool_meta":{},'
        '"server_parameters":{"command":"echo","args":[],"env":null,"cwd":null,"encoding":"utf-8","encoding_error_handler":"strict"}}',
        "mcp",
        # start/stop 时 manager 未初始化
        "start one",
        "stop one",
        # inputs add 用法
        "inputs add",
        # inputs update 用法 + @file 列表
        "inputs update",
        f"inputs update @{upd_file}",
        # inputs rm 用法 + rm 不存在
        "inputs rm",
        "inputs rm NOPE",
        # inputs get 用法
        "inputs get",
        # inputs value 顶层用法 + set 缺少参数 + set 不存在 id + get 不存在值
        "inputs value",
        "inputs value set",
        "inputs value set NOPE 1",
        "inputs value get NOPE",
        # inputs value 未知子命令
        "inputs value what",
        # socket connect 引导但 URL 为空，触发 URL required
        "socket connect",
        "",
        # socket join 带参数但尚未连接
        "socket join o1 c1",
        # socket leave 在未连接
        "socket leave",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_interactive_misc_and_file_paths(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖更多 interactive_impl 分支：
    - 空输入跳过
    - tools/mcp 打印
    - server add 使用 @file + 随后 rm
    - inputs add 使用 @file（数组）与 update 使用单对象
    - inputs value 边界：缺少参数、指定 id 清理、JSON 载荷
    - socket 再次 connect 走 already-connected 分支
    - socket join/leave 的未连接/未加入分支
    - 未知子命令（server/socket/notify）与 render 内联 JSON
    - start/stop 单个名称（manager 初始化后触发路径）
    """

    # 预备文件：server 与 inputs
    server_file = tmp_path / "server.json"
    server_file.write_text(
        json.dumps(
            {
                "name": "s1",
                "type": "stdio",
                "disabled": True,  # 避免真实启动
                "forbidden_tools": [],
                "tool_meta": {},
                "server_parameters": {
                    "command": "echo",
                    "args": [],
                    "env": None,
                    "cwd": None,
                    "encoding": "utf-8",
                    "encoding_error_handler": "strict",
                },
            },
        ),
        encoding="utf-8",
    )

    inputs_file = tmp_path / "inputs.json"
    inputs_file.write_text(
        json.dumps(
            [
                {"id": "I1", "type": "promptString", "description": "d1", "default": "x"},
                {"id": "I2", "type": "pickString", "description": "d2", "options": ["a", "b"], "default": "a"},
            ],
        ),
        encoding="utf-8",
    )

    # 指令脚本
    commands = [
        "",  # 空输入
        "tools",
        "mcp",
        f"server add @{server_file}",
        "server rm s1",
        f"inputs add @{inputs_file}",  # 数组 add
        'inputs update {"id":"I1","type":"promptString","description":"d1u","default":"y"}',  # 单对象 update
        "inputs value get",  # 缺失 id
        "inputs value rm",  # 缺失 id
        'inputs value set I1 {"k":1}',  # JSON 载荷
        "inputs value clear I1",  # 指定 id 清理
        "socket connect http://localhost:9000",  # 连接一次
        "socket connect http://localhost:9000",  # 已连接分支
        "socket join",  # 缺少参数
        "socket leave",  # 未加入房间
        "server unknownsub",
        "socket unknown",
        "notify unknown",
        'render {"a":1}',  # 内联 JSON 渲染
        # 初始化 manager 后测试 start/stop 单个名称分支
        "exit",
    ]

    # 打补丁：Session/patch_stdout/SMCP 客户端与 tools 列表
    class LocalFakeClient(FakeSMCPClient):
        pass

    # 我们需要在交互开始前让 comp.manager 初始化，以便稍后可以测试 start/stop 单个名称
    # 这里分两段会话：第一段跑上述命令到 exit，然后第二段在 manager 初始化后再跑 start/stop name

    monkeypatch.setattr(cli_main, "SMCPComputerClient", LocalFakeClient)
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)

    # stub 工具列表
    async def _fake_tools() -> list[dict[str, Any]]:
        return [{"name": "t1", "description": "d", "return_schema": {}}]

    monkeypatch.setattr(comp, "aget_available_tools", _fake_tools)

    await _interactive_loop(comp)

    # 第二段：初始化 manager 后测试 start/stop <name> 分支（即使失败也能走异常打印分支）
    await comp.boot_up()
    commands2 = [
        "start xxx",
        "stop xxx",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands2))
    await _interactive_loop(comp)


def test_root_no_color_triggers_console_switch(monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 _root 的 no_color 分支，并确保调用 _run_impl。"""
    called: dict[str, Any] = {"ok": False}

    def _stub_run_impl(**kwargs: Any) -> None:  # noqa: ANN003
        called["ok"] = True

    class Ctx:
        invoked_subcommand = None

    monkeypatch.setattr(cli_main, "_run_impl", _stub_run_impl, raising=True)

    # 验证不会抛异常，且 _run_impl 被调用
    cli_main._root(Ctx(), no_color=True)  # 其它参数用默认值
    assert called["ok"] is True


def test_run_impl_loads_inputs_and_servers_from_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 _run_impl 的 inputs/config 文件加载成功路径。"""
    # 提供立即退出的交互
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(["exit"]))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    inputs_file = tmp_path / "inputs.json"
    inputs_file.write_text(
        json.dumps(
            [
                {"id": "VA", "type": "promptString", "description": "d", "default": "1"},
                {"id": "VB", "type": "pickString", "description": "d", "options": ["x", "y"], "default": "x"},
            ],
        ),
        encoding="utf-8",
    )

    server_file = tmp_path / "servers.json"
    server_file.write_text(
        json.dumps(
            [
                {
                    "name": "s1",
                    "type": "stdio",
                    "disabled": True,
                    "forbidden_tools": [],
                    "tool_meta": {},
                    "server_parameters": {
                        "command": "echo",
                        "args": [],
                        "env": None,
                        "cwd": None,
                        "encoding": "utf-8",
                        "encoding_error_handler": "strict",
                    },
                },
            ],
        ),
        encoding="utf-8",
    )

    # 运行：不提供 url，避免网络；仅加载文件
    cli_main._run_impl(
        auto_connect=False,
        auto_reconnect=False,
        url=None,
        namespace=cli_main.SMCP_NAMESPACE,
        auth=None,
        headers=None,
        computer_factory=None,
        config=str(server_file),
        inputs=str(inputs_file),
    )


def test_run_impl_cli_params_parse_error(monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 _run_impl 在解析 auth/headers 失败时的异常分支。"""
    # 立即退出
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(["exit"]))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())
    # 使用假的 Socket 客户端避免真实连接
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)

    # 传入无效的 kv 字符串（缺少冒号），触发 parse_kv_pairs 抛错，从而走 except 分支
    cli_main._run_impl(
        auto_connect=False,
        auto_reconnect=False,
        url="http://localhost:7777",
        namespace=cli_main.SMCP_NAMESPACE,
        auth="invalid",  # 无效
        headers="also_invalid",  # 无效
        computer_factory=None,
        config=None,
        inputs=None,
    )


@pytest.mark.asyncio
async def test_inputs_cli_crud_commands(monkeypatch: pytest.MonkeyPatch) -> None:
    """覆盖 inputs 子命令：add/update/rm/get/list，并在连接状态下触发配置更新通知。"""
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)

    # 使用 socket connect 建立连接，随后执行 inputs 的 CRUD 命令
    commands = [
        "socket connect http://localhost:7000",
        # add 单条
        'inputs add {"id":"USER","type":"promptString","description":"d","default":"a"}',
        # get + list
        "inputs get USER",
        "inputs list",
        # update 批量（数组）
        'inputs update [{"id":"USER","type":"promptString","description":"d2","default":"b"},'
        ' {"id":"REG","type":"pickString","description":"r","options":["us","eu"],"default":"us"}]',
        "inputs list",
        # rm
        "inputs rm USER",
        "inputs list",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)

    last: FakeSMCPClient = FakeSMCPClient.last  # type: ignore[assignment]
    # 至少在 add/update/rm 期间触发了多次更新通知
    assert last.updated >= 3


@pytest.mark.asyncio
async def test_socket_connect_guided_inputs_parsing(monkeypatch: pytest.MonkeyPatch) -> None:
    """
    验证在未提供 URL 的情况下，交互式引导输入 URL/Auth/Headers，并正确解析传给 connect(auth=..., headers=...).
    """
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)

    # 触发引导式：先输入命令，再依次回应 URL、Auth、Headers，然后退出
    commands = [
        "socket connect",
        "http://localhost:8000",
        "token:abc123",
        "app:demo,ver:1.0",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)

    # 断言 FakeSMCPClient 收到了期望的参数
    last: FakeSMCPClient = FakeSMCPClient.last  # type: ignore[assignment]
    assert last.connected is True
    assert last.connect_args is not None
    assert last.connect_args["url"] == "http://localhost:8000"
    assert last.connect_args["auth"] == {"token": "abc123"}
    assert last.connect_args["headers"] == {"app": "demo", "ver": "1.0"}


def test_run_with_cli_url_auth_headers(monkeypatch: pytest.MonkeyPatch) -> None:
    """
    验证通过 run(url=..., auth=..., headers=...) 启动时，会自动连接并传入解析后的参数，随后进入交互并退出。
    """
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)

    # 进入交互后立即退出
    commands = [
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    # 调用同步的 run()，其内部使用 asyncio.run() 执行
    cli_main.run(
        auto_connect=False,
        auto_reconnect=False,
        url="http://service:1234",
        namespace=cli_main.SMCP_NAMESPACE,
        auth="token:abc",
        headers="h1:v1,h2:v2",
    )

    last: FakeSMCPClient = FakeSMCPClient.last  # type: ignore[assignment]
    assert last.connected is True
    assert last.connect_args == {
        "url": "http://service:1234",
        "auth": {"token": "abc"},
        "headers": {"h1": "v1", "h2": "v2"},
        "namespaces": [cli_main.SMCP_NAMESPACE],
    }


@pytest.mark.asyncio
async def test_server_add_and_status_without_auto_connect(monkeypatch: pytest.MonkeyPatch) -> None:
    # Minimal stdio server config (disabled=true to avoid start operations later)
    stdio_cfg = {
        "name": "test-stdio",
        "type": "stdio",
        "disabled": True,
        "forbidden_tools": [],
        "tool_meta": {},
        "server_parameters": {
            "command": "echo",
            "args": [],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }

    commands = [
        f"server add {stdio_cfg}",
        "mcp",
        "status",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_unknown_and_status_manager_uninitialized(monkeypatch: pytest.MonkeyPatch) -> None:
    commands = [
        "unknown",
        "status",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_server_rm_without_name_and_add_invalid_json(monkeypatch: pytest.MonkeyPatch) -> None:
    commands = [
        "server rm",
        "server add {invalid}",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_start_stop_all_with_manager_initialized(monkeypatch: pytest.MonkeyPatch) -> None:
    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await comp.boot_up()

    commands = [
        "start all",
        "stop all",
        "exit",
    ]
    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    await _interactive_loop(comp)


@pytest.mark.asyncio
async def test_inputs_load_and_render(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    inputs_file = tmp_path / "inputs.json"
    inputs_file.write_text(
        json.dumps(
            [
                {"id": "VAR1", "type": "promptString", "description": "v", "default": "abc"},
                {"id": "CHOICE", "type": "pickString", "description": "d", "options": ["x", "y"], "default": "x"},
            ],
        ),
        encoding="utf-8",
    )

    any_file = tmp_path / "any.json"
    any_file.write_text(json.dumps({"k": "${input:VAR1}", "c": "${input:CHOICE}"}), encoding="utf-8")

    commands = [
        f"inputs load @{inputs_file}",
        f"render @{any_file}",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)


class FakeSMCPClient:
    def __init__(self, *args: Any, **kwargs: Any) -> None:  # noqa: D401
        self.connected = False
        self.office_id: str | None = None
        self.joined_args: tuple[str, str] | None = None
        self.updated = 0
        # 记录最后一个实例，便于断言
        FakeSMCPClient.last = self  # type: ignore[attr-defined]
        self.connect_args: dict[str, Any] | None = None

    async def connect(
        self,
        url: str,
        auth: dict[str, Any] | None = None,
        headers: dict[str, Any] | None = None,
        namespaces: list[str] | None = None,
    ) -> None:
        self.connected = True
        args: dict[str, Any] = {"url": url, "auth": auth, "headers": headers}
        if namespaces is not None:
            args["namespaces"] = namespaces
        self.connect_args = args

    async def join_office(self, office_id: str, computer_name: str) -> None:
        assert self.connected
        self.office_id = office_id
        self.joined_args = (office_id, computer_name)

    async def leave_office(self, office_id: str) -> None:
        assert self.connected
        self.office_id = None

    async def emit_update_config(self) -> None:
        self.updated += 1


@pytest.mark.asyncio
async def test_socket_and_notify_branches(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(cli_main, "SMCPComputerClient", FakeSMCPClient)

    commands = [
        "notify update",
        "socket connect http://localhost:7000",
        "socket join office-1 compA",
        "notify update",
        "socket leave",
        "exit",
    ]

    monkeypatch.setattr(cli_main, "PromptSession", lambda: FakePromptSession(commands))
    monkeypatch.setattr(cli_main, "patch_stdout", lambda raw: no_patch_stdout())

    comp = Computer(name="test_main_c", inputs=set(), mcp_servers=set(), auto_connect=False, auto_reconnect=False)
    await _interactive_loop(comp)
