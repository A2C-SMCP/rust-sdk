"""
文件名: test_cli_tc.py
作者: JQQ
创建日期: 2025/10/02
最后修改日期: 2025/10/02
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文:
    - 端到端验证交互式 CLI 的 `tc` 命令，针对真实 stdio MCP server（direct_execution.py）。
    - 通过写入 server 配置文件并使用 `server add @file` + `start <name>` 启动。
    - 为避免二次确认阻断，使用 `tool_meta.hello.auto_apply=true` 开启自动执行。
    - 发送 `tc` 的 JSON 负载（与 Socket.IO 一致的 `ToolCallReq` 结构），期望输出包含工具返回文本。

  English:
    - E2E test for interactive CLI `tc` against real stdio MCP server (direct_execution.py).
    - Start server via config file and `server add @file` + `start <name>`.
    - Enable `tool_meta.hello.auto_apply=true` to bypass confirm gate.
    - Send `tc` JSON payload (Socket.IO-compatible `ToolCallReq`) and expect tool output text.

测试要点与断言:
  1) 成功添加并启动名为 `e2e-tc` 的 stdio 服务器
  2) `tools` 输出包含 `hello`
  3) 发送 `tc`（tool_name 使用 `e2e-tc/hello` 以确保路由）后输出包含 `Hello, E2E!`
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@pytest.mark.e2e
def test_tc_call_hello(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 通过 `tc` 调用 `hello` 工具，并校验结果文本。
    English: Call `hello` tool via `tc` and assert result text.
    """
    child = cli_proc

    # 1) 写入 server 配置文件（打开 auto_apply），指向 direct_execution.py
    server_cfg = {
        "name": "e2e-tc",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"hello": {"auto_apply": True}},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 2) 添加配置并启动
    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 3) 确认工具已可见（tools 列表应包含 hello）
    child.sendline("tools")
    tools_out = expect_prompt_stable(child, quiet=0.6, max_wait=12.0)
    assert "hello" in strip_ansi(tools_out)

    # 4) 构造 tc 负载，工具名使用原始 MCP 工具名（不加前缀）。
    #    中文: Manager 会在所有已启动 server 中解析该工具名；我们前一步已确认 tools 中包含 hello。
    #    English: Manager resolves plain tool name across active servers; tools list already contains 'hello'.
    tc_payload = {
        "agent": "bot-e2e",
        "req_id": "req-e2e-hello",
        "computer": "ignored",
        "tool_name": "hello",
        "params": {"name": "E2E"},
        "timeout": 10,
    }

    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    # 5) 断言包含调用结果文本
    assert "Hello, E2E!" in out, f"unexpected tc output:\n{out}"


@pytest.mark.e2e
def test_tc_default_and_tool_both_false_then_error(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 当 `tool_meta.mark_b.auto_apply=False` 且 `default_tool_meta.auto_apply=False` 时，调用需要二次确认，
        但 CLI 未实现回调，应返回错误提示文本。
    English: With both `tool_meta.mark_b.auto_apply=False` and `default_tool_meta.auto_apply=False`, call requires
        confirm; CLI lacks confirm callback, expect error message.
    """
    child = cli_proc

    server_cfg = {
        "name": "e2e-tc-b-both-false",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"mark_b": {"auto_apply": False}},
        "default_tool_meta": {"auto_apply": False},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/resources_subscribe_b_stdio_server.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_b_both_false.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-b-both-false")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 发送 tc（使用 mark_b）
    tc_payload = {
        "agent": "r-e2e",
        "req_id": "req-e2e",
        "computer": "client",
        "tool_name": "mark_b",
        "params": {},
        "timeout": 5,
    }
    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    # 期望错误并给出中文提示
    assert '"isError": true' in out or '"isError":true' in out
    assert "当前工具需要调用前进行二次确认" in out, f"unexpected output:\n{out}"


@pytest.mark.e2e
def test_tc_tool_true_overrides_default(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 如果工具设置了 auto_apply=True，则无论 default_tool_meta 如何，都应直接执行成功。
    English: If tool's auto_apply=True is set, it should override default and execute successfully.
    """
    child = cli_proc

    server_cfg = {
        "name": "e2e-tc-b-tool-true",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        # 工具设置为 True，default 故意设为 False 以验证覆盖
        "tool_meta": {"mark_b": {"auto_apply": True}},
        "default_tool_meta": {"auto_apply": False},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/resources_subscribe_b_stdio_server.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_b_tool_true.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-b-tool-true")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    tc_payload = {
        "agent": "r-e2e",
        "req_id": "req-e2e",
        "computer": "client",
        "tool_name": "mark_b",
        "params": {},
        "timeout": 5,
    }
    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    assert "ok:mark_b" in out and '"isError": false' in out or '"isError":false' in out


@pytest.mark.e2e
def test_tc_tool_unset_uses_default(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 当工具未设置 auto_apply 时，遵循 default_tool_meta；这里 default 设置为 True，应执行成功。
    English: When tool auto_apply is unset, follow default_tool_meta; with default=True, expect success.
    """
    child = cli_proc

    server_cfg = {
        "name": "e2e-tc-b-default-true",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        # 不设置 tool_meta.mark_b
        "tool_meta": {},
        "default_tool_meta": {"auto_apply": True},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/resources_subscribe_b_stdio_server.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_b_default_true.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-b-default-true")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    tc_payload = {
        "agent": "r-e2e",
        "req_id": "req-e2e",
        "computer": "client",
        "tool_name": "mark_b",
        "params": {},
        "timeout": 5,
    }
    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    assert "ok:mark_b" in out and ('"isError": false' in out or '"isError":false' in out)
