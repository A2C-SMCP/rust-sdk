"""
文件名: main.py
作者: JQQ
创建日期: 2025/9/18
最后修改日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: typer, rich, prompt_toolkit
描述:
  中文: A2C 计算机客户端的命令行入口，提供持续运行模式与基础交互命令。
  English: CLI entry for A2C Computer client. Provides persistent run mode and basic interactive commands.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import typer
from prompt_toolkit import PromptSession
from prompt_toolkit.patch_stdout import patch_stdout
from pydantic import TypeAdapter

from a2c_smcp.computer.cli.interactive_impl import interactive_loop as _interactive_loop_impl
from a2c_smcp.computer.cli.utils import (
    parse_kv_pairs,
    resolve_import_target,
)
from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.inputs.resolver import InputResolver
from a2c_smcp.computer.mcp_clients.model import (
    MCPServerInput as MCPServerInputModel,
)
from a2c_smcp.computer.socketio.client import SMCPComputerClient
from a2c_smcp.computer.utils import console as console_util
from a2c_smcp.smcp import SMCP_NAMESPACE
from a2c_smcp.smcp import MCPServerConfig as SMCPServerConfigDict
from a2c_smcp.smcp import MCPServerInput as SMCPServerInputDict

app = typer.Typer(add_completion=False, help="A2C Computer CLI")
# 使用全局 Console（引用模块属性，便于后续动态切换）
console = console_util.console


# ------------------------------
# Computer 工厂函数类型标注
# ------------------------------
# 中文:
#  - 该类型表示一个可调用对象（函数或类构造器），用于创建 Computer 或其子类的实例。
#  - 参数签名需与 Computer.__init__ 兼容；你可以据此在你自己的工厂函数上添加类型注释。
# English:
#  - This type denotes a callable (function or class constructor) used to create a Computer or subclass instance.
#  - The parameter signature must be compatible with Computer.__init__; use it for your own factory annotations.
ComputerFactory = Callable[
    [
        set["MCPServerInputModel"] | None,
        bool,
        bool,
        Callable[[str, str, str, dict], bool] | None,
        InputResolver | None,
    ],
    Computer,
]


@app.callback(invoke_without_command=True)
def _root(
    ctx: typer.Context,
    auto_connect: bool = typer.Option(True, help="是否自动连接 / Auto connect"),
    auto_reconnect: bool = typer.Option(True, help="是否自动重连 / Auto reconnect"),
    url: str | None = typer.Option(None, help="Socket.IO 服务器URL，例如 https://host:port"),
    namespace: str = typer.Option(
        SMCP_NAMESPACE,
        "--namespace",
        help="Socket.IO 命名空间（默认: /smcp）/ Namespace to connect (default: /smcp)",
    ),
    auth: str | None = typer.Option(None, help="认证参数，形如 key:value,foo:bar"),
    headers: str | None = typer.Option(None, help="请求头参数，形如 key:value,foo:bar"),
    computer_factory: str | None = typer.Option(
        None,
        "--computer-factory",
        help=(
            "指定用于构建 Computer 的导入路径 (模块:属性 或 模块.属性)。\n"
            "例如 my_pkg.my_mod:build_computer 或 my_pkg.my_mod.MySubComputer。\n"
            "不支持以 '.' 开头的相对导入；模块解析相对于运行 a2c-computer 时的工作目录可导入包环境。"
        ),
    ),
    no_color: bool = typer.Option(
        False,
        "--no-color",
        help="关闭彩色输出（PyCharm控制台不渲染ANSI时可使用） / Disable ANSI colors",
    ),
) -> None:
    """
    根级入口：
    - 若未指定子命令，则等价于执行 `run`，保持 `a2c-computer` 和 `a2c-computer run` 两种用法都可用。
    - 若指定了子命令，则不做处理，交给子命令。
    """
    # 根据 no_color 动态调整全局 Console
    if no_color:
        global console
        console_util.set_no_color(True)
        # 重新绑定本地引用
        console = console_util.console

    if ctx.invoked_subcommand is None:
        # 注意：不要直接调用被 @app.command 装饰的 run()，否则未传入的参数会保留 OptionInfo 默认值
        # 这里改为调用纯实现函数 _run_impl，并显式传入 config=None 与 inputs=None。
        _run_impl(
            auto_connect=auto_connect,
            auto_reconnect=auto_reconnect,
            url=url,
            namespace=namespace,
            auth=auth,
            headers=headers,
            computer_factory=computer_factory,
            config=None,
            inputs=None,
        )


async def _interactive_loop(comp: Computer, init_client: SMCPComputerClient | None = None) -> None:
    """
    中文: 兼容外部引用的包装器，委托到 interactive_impl，并注入依赖。
    English: Backward-compatible wrapper that delegates to interactive_impl with dependencies injected.
    """
    await _interactive_loop_impl(
        comp,
        session_factory=PromptSession,
        patch_stdout_ctx=patch_stdout,
        smcp_client_cls=SMCPComputerClient,
        init_client=init_client,
    )


def _run_impl(
    *,
    auto_connect: bool,
    auto_reconnect: bool,
    url: str | None,
    namespace: str | None,
    auth: str | None,
    headers: str | None,
    computer_factory: str | None,
    config: str | None,
    inputs: str | None,
) -> None:
    """
    纯实现函数：不要在此处使用 Typer 的 Option 默认值，避免 OptionInfo 泄露到运行时。
    Both CLI (@app.command) 与回调 (@app.callback) 在需要时调用本函数。
    """

    async def _amain() -> None:
        # 初始化空配置，后续通过交互动态维护 / init with empty config, then manage dynamically
        # 解析工厂：默认使用 Computer 构造函数；若提供 --computer-factory，则动态导入。
        comp_factory_obj: Any = Computer
        if computer_factory:
            try:
                comp_factory_obj = resolve_import_target(computer_factory)
            except Exception as e:  # pragma: no cover
                console.print(f"[red]解析 --computer-factory 失败: {e} / Failed to resolve computer factory: {e}[/red]")
                comp_factory_obj = Computer

        # 类型提示：comp_factory_obj 应满足 ComputerFactory，可是运行时仅作 best-effort 校验
        if not callable(comp_factory_obj):  # pragma: no cover
            console.print("[red]计算机构造目标不可调用，将回退到默认 Computer[/red]")
            comp_factory_obj = Computer

        comp = comp_factory_obj(
            name="friday_hands",
            inputs=set(),
            mcp_servers=set(),
            auto_connect=auto_connect,
            auto_reconnect=auto_reconnect,
        )
        async with comp:
            init_client: SMCPComputerClient | None = None
            if url:
                try:
                    auth_dict = parse_kv_pairs(auth)
                    headers_dict = parse_kv_pairs(headers)
                except Exception as e:
                    console.print(f"[red]启动参数解析失败 / Failed to parse CLI params: {e}[/red]")
                    auth_dict = None
                    headers_dict = None
                init_client = SMCPComputerClient(computer=comp)
                # 通过 CLI 指定命名空间，确保连接时建立对应 namespace 会话
                await init_client.connect(url, auth=auth_dict, headers=headers_dict, namespaces=[namespace])
                console.print("[green]已通过启动参数连接到 Socket.IO / Connected via CLI options[/green]")

            # 启动参数加载 inputs 与 servers 配置
            # Load inputs first (so that servers config rendering can use them if needed later via interactive commands)
            if inputs:
                try:
                    ipath = inputs[1:] if inputs.startswith("@") else inputs
                    data = json.loads(Path(ipath).read_text(encoding="utf-8"))
                    # 允许单个对象或数组
                    if isinstance(data, list):
                        raw_items = TypeAdapter(list[SMCPServerInputDict]).validate_python(data)
                        models: set[MCPServerInputModel] = {TypeAdapter(MCPServerInputModel).validate_python(item) for item in raw_items}
                        comp.update_inputs(models)
                    else:
                        item: SMCPServerInputDict = TypeAdapter(SMCPServerInputDict).validate_python(data)
                        comp.add_or_update_input(TypeAdapter(MCPServerInputModel).validate_python(item))
                    console.print("[green]已加载 Inputs 配置 / Inputs loaded[/green]")
                except Exception as e:  # pragma: no cover
                    console.print(f"[red]加载 Inputs 失败 / Failed to load inputs: {e}[/red]")

            if config:
                try:
                    spath = config[1:] if config.startswith("@") else config
                    data = json.loads(Path(spath).read_text(encoding="utf-8"))
                    # 允许单个对象或数组

                    async def _add_server(cfg_obj: dict[str, Any]) -> None:
                        validated: dict[str, Any] = TypeAdapter(SMCPServerConfigDict).validate_python(cfg_obj)
                        await comp.aadd_or_aupdate_server(validated)

                    if isinstance(data, list):
                        for cfg in data:
                            await _add_server(cfg)
                    else:
                        await _add_server(data)
                    console.print("[green]已加载 Servers 配置 / Servers loaded[/green]")
                except Exception as e:  # pragma: no cover
                    console.print(f"[red]加载 Servers 失败 / Failed to load servers: {e}[/red]")

            await _interactive_loop(comp, init_client=init_client)

    asyncio.run(_amain())


@app.command()
def run(
    auto_connect: bool = typer.Option(True, help="是否自动连接 / Auto connect"),
    auto_reconnect: bool = typer.Option(True, help="是否自动重连 / Auto reconnect"),
    url: str | None = typer.Option(None, help="Socket.IO 服务器URL，例如 https://host:port"),
    namespace: str = typer.Option(
        SMCP_NAMESPACE,
        "--namespace",
        help="Socket.IO 命名空间（默认: /smcp）/ Namespace to connect (default: /smcp)",
    ),
    auth: str | None = typer.Option(None, help="认证参数，形如 key:value,foo:bar"),
    headers: str | None = typer.Option(None, help="请求头参数，形如 key:value,foo:bar"),
    computer_factory: str | None = typer.Option(
        None,
        "--computer-factory",
        help=(
            "指定用于构建 Computer 的导入路径 (模块:属性 或 模块.属性)。\n"
            "例如 my_pkg.my_mod:build_computer 或 my_pkg.my_mod.MySubComputer。\n"
            "不支持以 '.' 开头的相对导入；模块解析相对于运行 a2c-computer 时的工作目录可导入包环境。"
        ),
    ),
    config: str | None = typer.Option(
        None,
        "--config",
        "-c",
        help="在启动时从文件加载 MCP Servers 配置（支持 @file 语法或直接文件路径） / Load MCP Servers from file at startup",
    ),
    inputs: str | None = typer.Option(
        None,
        "--inputs",
        "-i",
        help="在启动时从文件加载 Inputs 定义（支持 @file 语法或直接文件路径） / Load Inputs from file at startup",
    ),
) -> None:
    """
    中文: 启动计算机并进入持续运行模式。后续将支持从配置文件加载 servers 与 inputs。
    English: Boot the computer and enter persistent loop. Config-file loading will be added later.
    """
    _run_impl(
        auto_connect=auto_connect,
        auto_reconnect=auto_reconnect,
        url=url,
        namespace=namespace,
        auth=auth,
        headers=headers,
        computer_factory=computer_factory,
        config=config,
        inputs=inputs,
    )


# 为 console_scripts 兼容提供入口
def main() -> None:  # pragma: no cover
    # 使用 Typer 应用入口，而不是直接调用命令函数
    # 直接调用被 @app.command 装饰的函数会传入 OptionInfo 默认值，导致参数类型错误
    app()
