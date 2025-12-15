# -*- coding: utf-8 -*-
# filename: test_cli_tc_vrl.py
# @Time    : 2025/10/06 14:55
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文:
  - 端到端验证交互式 CLI 的 `tc` 命令配合 VRL 转换功能。
  - 通过配置 VRL 脚本，验证工具返回值被正确转换并存储在元数据中。
  - 验证 VRL 转换结果以 JSON 格式存储在 `a2c_vrl_transformed` 字段中。

English:
  - E2E test for interactive CLI `tc` command with VRL transformation.
  - Verify tool return values are correctly transformed via VRL script.
  - Verify VRL transformation results are stored in `a2c_vrl_transformed` metadata field as JSON.

测试要点与断言:
  1) 配置 MCP Server 时带上 VRL 脚本
  2) 成功添加并启动配置了 VRL 的服务器
  3) 发送 `tc` 调用工具后，输出包含 VRL 转换结果标识
  4) 验证元数据中包含 `a2c_vrl_transformed` 字段
  5) 验证 VRL 转换后的数据结构符合预期
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@pytest.mark.e2e
def test_tc_with_vrl_transformation_basic(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 验证配置 VRL 脚本后，tc 调用工具返回的元数据中包含 VRL 转换结果。
    English: Verify tc tool call returns VRL transformation result in metadata when VRL is configured.
    """
    child = cli_proc

    # 1) 配置 VRL 脚本：添加一个新字段 transformed_by_vrl = true
    # Configure VRL script: add a new field transformed_by_vrl = true
    vrl_script = '.transformed_by_vrl = true\n.vrl_test_field = "e2e_test"'

    # 2) 写入 server 配置文件，包含 VRL 脚本
    # Write server config file with VRL script
    server_cfg = {
        "name": "e2e-tc-vrl-basic",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"hello": {"auto_apply": True}},
        "vrl": vrl_script,  # 添加 VRL 配置 / Add VRL config
        "server_parameters": {
            "command": sys.executable,
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_vrl_basic.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 3) 添加配置并启动
    # Add config and start server
    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-vrl-basic")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 4) 确认工具已可见
    # Verify tool is visible
    child.sendline("tools")
    tools_out = expect_prompt_stable(child, quiet=0.6, max_wait=12.0)
    assert "hello" in strip_ansi(tools_out)

    # 5) 构造 tc 负载并调用
    # Construct tc payload and call
    tc_payload = {
        "agent": "bot-e2e-vrl",
        "req_id": "req-e2e-vrl-hello",
        "computer": "ignored",
        "tool_name": "hello",
        "params": {"name": "VRL-Test"},
        "timeout": 10,
    }

    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    # 6) 断言包含工具调用结果
    # Assert tool call result is present
    assert "Hello, VRL-Test!" in out, f"unexpected tc output:\n{out}"

    # 7) 断言包含 VRL 转换标识
    # Assert VRL transformation marker is present
    assert "a2c_vrl_transformed" in out, f"VRL transformation marker not found in output:\n{out}"

    # 8) 验证 VRL 转换后的字段存在
    # Verify VRL transformed fields exist
    assert "transformed_by_vrl" in out or "vrl_test_field" in out, f"VRL transformed fields not found in output:\n{out}"


@pytest.mark.e2e
def test_tc_with_vrl_field_mapping(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 验证 VRL 可以对工具返回值进行字段映射和数据转换。
    English: Verify VRL can perform field mapping and data transformation on tool return values.
    """
    child = cli_proc

    # 1) 配置 VRL 脚本：提取工具返回的文本内容，并重新组织结构
    # Configure VRL script: extract text content and reorganize structure
    vrl_script = """
.status = "success"
.original_content = .content[0].text
.metadata = {
    "processed": true,
    "processor": "vrl-e2e-test"
}
"""

    # 2) 写入 server 配置文件
    # Write server config file
    server_cfg = {
        "name": "e2e-tc-vrl-mapping",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"mark_a": {"auto_apply": True}},
        "vrl": vrl_script,
        "server_parameters": {
            "command": sys.executable,
            "args": [
                "tests/integration_tests/computer/mcp_servers/resources_subscribe_stdio_server.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_vrl_mapping.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 3) 添加配置并启动
    # Add config and start server
    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-vrl-mapping")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 4) 确认工具已可见
    # Verify tool is visible
    child.sendline("tools")
    tools_out = expect_prompt_stable(child, quiet=0.6, max_wait=12.0)
    assert "mark_a" in strip_ansi(tools_out)

    # 5) 构造 tc 负载并调用
    # Construct tc payload and call
    tc_payload = {
        "agent": "bot-e2e-vrl-map",
        "req_id": "req-e2e-vrl-map",
        "computer": "ignored",
        "tool_name": "mark_a",
        "params": {},
        "timeout": 10,
    }

    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    # 6) 断言包含工具调用结果
    # Assert tool call result is present
    assert "ok:mark_a" in out, f"unexpected tc output:\n{out}"

    # 7) 断言包含 VRL 转换标识和转换后的字段
    # Assert VRL transformation marker and transformed fields are present
    assert "a2c_vrl_transformed" in out, f"VRL transformation marker not found in output:\n{out}"
    # VRL转换结果在meta中是JSON字符串，可能被转义，所以检查转义后的形式
    # VRL transformation result in meta is JSON string, may be escaped, so check escaped form
    assert '"status"' in out or "'status'" in out or '\\"status\\"' in out, f"VRL transformed 'status' field not found in output:\n{out}"
    assert '"success"' in out or "'success'" in out or '\\"success\\"' in out, (
        f"VRL transformed 'success' value not found in output:\n{out}"
    )


@pytest.mark.e2e
def test_tc_with_invalid_vrl_syntax_rejected(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 验证配置了无效 VRL 语法的服务器会在添加时被拒绝。
    English: Verify server with invalid VRL syntax is rejected during configuration.
    """
    child = cli_proc

    # 1) 配置无效的 VRL 脚本（语法错误）
    # Configure invalid VRL script (syntax error)
    invalid_vrl_script = ".invalid syntax here @@@ this will fail"

    # 2) 写入 server 配置文件
    # Write server config file
    server_cfg = {
        "name": "e2e-tc-vrl-invalid",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"hello": {"auto_apply": True}},
        "vrl": invalid_vrl_script,  # 无效的 VRL 脚本 / Invalid VRL script
        "server_parameters": {
            "command": sys.executable,
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_vrl_invalid.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 3) 尝试添加配置，应该失败
    # Try to add config, should fail
    child.sendline(f"server add @{cfg_path}")
    out = expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    out = strip_ansi(out)

    # 4) 断言包含错误信息
    # Assert error message is present
    assert "VRL" in out or "语法错误" in out or "syntax error" in out or "error" in out.lower(), (
        f"Expected VRL syntax error message in output:\n{out}"
    )


@pytest.mark.e2e
def test_tc_vrl_transformation_preserves_original_content(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 验证 VRL 转换不影响原始工具返回内容，转换结果仅存储在元数据中。
    English: Verify VRL transformation doesn't affect original tool return content,
              transformation result is only stored in metadata.
    """
    child = cli_proc

    # 1) 配置 VRL 脚本
    # Configure VRL script
    vrl_script = '.vrl_marker = "transformed"\n.extra_field = 42'

    # 2) 写入 server 配置文件
    # Write server config file
    server_cfg = {
        "name": "e2e-tc-vrl-preserve",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"hello": {"auto_apply": True}},
        "vrl": vrl_script,
        "server_parameters": {
            "command": sys.executable,
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_tc_vrl_preserve.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 3) 添加配置并启动
    # Add config and start server
    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-tc-vrl-preserve")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 4) 构造 tc 负载并调用
    # Construct tc payload and call
    tc_payload = {
        "agent": "bot-e2e-vrl-preserve",
        "req_id": "req-e2e-vrl-preserve",
        "computer": "ignored",
        "tool_name": "hello",
        "params": {"name": "Preserve-Test"},
        "timeout": 10,
    }

    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    out = expect_prompt_stable(child, quiet=0.8, max_wait=20.0)
    out = strip_ansi(out)

    # 5) 断言原始内容仍然存在
    # Assert original content is still present
    assert "Hello, Preserve-Test!" in out, f"Original content missing in output:\n{out}"

    # 6) 断言 VRL 转换结果也存在于元数据中
    # Assert VRL transformation result exists in metadata
    assert "a2c_vrl_transformed" in out, f"VRL transformation marker not found in output:\n{out}"
    assert "vrl_marker" in out or "extra_field" in out, f"VRL transformed fields not found in output:\n{out}"
