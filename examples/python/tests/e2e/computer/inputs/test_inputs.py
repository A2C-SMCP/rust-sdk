# -*- coding: utf-8 -*-
# filename: test_inputs.py
# @Time    : 2025/11/24 17:01
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

"""
文件名: test_inputs.py
作者: JQQ
创建日期: 2025/11/24
最后修改日期: 2025/11/24
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: E2E 测试 inputs 的加载、查询、设置默认值等功能。
  English: E2E tests for inputs loading, querying, and setting default values.
"""

from __future__ import annotations

import time

import pytest

from tests.e2e.computer.utils import expect_prompt_stable

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@pytest.mark.e2e
def test_inputs_load_and_list_definitions(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs load 加载定义后，inputs list 能正确显示所有定义。
    English: Test that after inputs load, inputs list correctly shows all definitions.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_with_default.json
      2) inputs list 应该显示 3 个 inputs 定义
      3) 验证输出包含所有 id
    """
    print("[test_start] test_inputs_load_and_list_definitions")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 列出 inputs 定义 / List inputs definitions
    child.sendline("inputs list")
    time.sleep(0.5)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs list output:\n{out}")

    # 3) 验证输出包含所有 id / Verify output contains all ids
    assert "CLAUDE_CWD" in out, "应该包含 CLAUDE_CWD / Should contain CLAUDE_CWD"
    assert "FEISHU_APP_ID" in out, "应该包含 FEISHU_APP_ID / Should contain FEISHU_APP_ID"
    assert "FEISHU_APP_SECRET" in out, "应该包含 FEISHU_APP_SECRET / Should contain FEISHU_APP_SECRET"


@pytest.mark.e2e
def test_inputs_value_list_empty_after_load(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs load 后，inputs value list 为空（因为没有实际解析）。
    English: Test that after inputs load, inputs value list is empty (no actual resolution).

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_with_default.json
      2) inputs value list 应该返回空字典 {}
    """
    print("[test_start] test_inputs_value_list_empty_after_load")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 列出 inputs 值缓存 / List inputs value cache
    child.sendline("inputs value list")
    time.sleep(0.5)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value list output:\n{out}")

    # 3) 验证输出为空字典 / Verify output is empty dict
    # 中文: 输出应该是 {} 或包含 "empty" 等提示
    # English: Output should be {} or contain hints like "empty"
    assert "{}" in out or "empty" in out.lower(), "缓存应该为空 / Cache should be empty"


@pytest.mark.e2e
def test_inputs_value_set_with_explicit_value(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs value set <id> <value> 手动设置值后，inputs value list 能正确显示。
    English: Test that after inputs value set <id> <value>, inputs value list shows the value.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_with_default.json
      2) inputs value set FEISHU_APP_ID "custom_app_id"
      3) inputs value list 应该包含 FEISHU_APP_ID: "custom_app_id"
      4) inputs value get FEISHU_APP_ID 应该返回 "custom_app_id"
    """
    print("[test_start] test_inputs_value_set_with_explicit_value")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 手动设置值 / Manually set value
    child.sendline('inputs value set FEISHU_APP_ID "custom_app_id"')
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value set output:\n{out}")
    assert "已设置" in out or "Set" in out, "应该提示设置成功 / Should indicate success"

    # 3) 列出 inputs 值缓存 / List inputs value cache
    child.sendline("inputs value list")
    time.sleep(0.5)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value list output:\n{out}")
    assert "FEISHU_APP_ID" in out, "应该包含 FEISHU_APP_ID / Should contain FEISHU_APP_ID"
    assert "custom_app_id" in out, "应该包含设置的值 / Should contain the set value"

    # 4) 获取单个值 / Get individual value
    child.sendline("inputs value get FEISHU_APP_ID")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value get output:\n{out}")
    assert "custom_app_id" in out, "应该返回设置的值 / Should return the set value"


@pytest.mark.e2e
def test_inputs_value_set_with_default(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs value set <id> 不带值参数时，自动使用 default 值。
    English: Test that inputs value set <id> without value parameter uses default value.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_with_default.json
      2) inputs value set CLAUDE_CWD (不带值参数)
      3) 应该提示使用 default 值
      4) inputs value get CLAUDE_CWD 应该返回 default 值
    """
    print("[test_start] test_inputs_value_set_with_default")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 不带值参数设置，应该使用 default / Set without value, should use default
    child.sendline("inputs value set CLAUDE_CWD")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value set (no value) output:\n{out}")
    # 中文: 应该提示使用了 default 值
    # English: Should indicate using default value
    assert "default" in out.lower() or "TfrobotSceneTests" in out, "应该提示使用 default 值 / Should indicate using default"

    # 3) 获取值验证 / Get value to verify
    child.sendline("inputs value get CLAUDE_CWD")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value get output:\n{out}")
    assert "TfrobotSceneTests" in out, "应该返回 default 值 / Should return default value"


@pytest.mark.e2e
def test_inputs_value_set_no_default_error(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试对没有 default 值的 input 执行 inputs value set <id> 时，应该提示错误。
    English: Test that inputs value set <id> on input without default shows error.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_basic.json (SCRIPT 有 default)
      2) inputs add 一个没有 default 的 input
      3) inputs value set <no-default-id> 应该提示错误
    """
    print("[test_start] test_inputs_value_set_no_default_error")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 添加一个没有 default 的 input / Add input without default
    child.sendline('inputs add {"id": "NO_DEFAULT", "type": "promptString", "description": "No default", "default": null}')
    time.sleep(0.3)
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 3) 尝试不带值设置，应该提示错误 / Try to set without value, should show error
    child.sendline("inputs value set NO_DEFAULT")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value set (no default) output:\n{out}")
    # 中文: 应该提示没有 default 值
    # English: Should indicate no default value
    assert "没有 default" in out or "no default" in out.lower(), "应该提示没有 default 值 / Should indicate no default"


@pytest.mark.e2e
def test_inputs_value_set_command_type_error(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试对 command 类型的 input 执行 inputs value set <id> 时，应该提示不支持 default。
    English: Test that inputs value set <id> on command type input shows no default support error.

    步骤 Steps:
      1) inputs add 一个 command 类型的 input
      2) inputs value set <command-id> 应该提示 command 类型不支持 default
    """
    print("[test_start] test_inputs_value_set_command_type_error")
    child = cli_proc

    # 1) 添加一个 command 类型的 input / Add command type input
    child.sendline('inputs add {"id": "CMD_INPUT", "type": "command", "description": "Command input", "command": "echo test"}')
    time.sleep(0.3)
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 尝试不带值设置，应该提示 command 类型不支持 default / Try to set without value, should show error
    child.sendline("inputs value set CMD_INPUT")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs value set (command type) output:\n{out}")
    # 中文: 应该提示 command 类型不支持 default
    # English: Should indicate command type has no default support
    assert ("command" in out.lower() and ("不支持" in out or "no" in out.lower())) or "command type" in out.lower(), (
        "应该提示 command 类型不支持 default / Should indicate command type no default"
    )


@pytest.mark.e2e
def test_inputs_value_rm_and_clear(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs value rm 和 clear 命令。
    English: Test inputs value rm and clear commands.

    步骤 Steps:
      1) inputs load 并设置多个值
      2) inputs value rm <id> 删除单个值
      3) inputs value clear 清空所有值
    """
    print("[test_start] test_inputs_value_rm_and_clear")
    child = cli_proc

    # 1) 加载并设置值 / Load and set values
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    child.sendline("inputs value set CLAUDE_CWD")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    child.sendline("inputs value set FEISHU_APP_ID")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 验证有 2 个值 / Verify 2 values
    child.sendline("inputs value list")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    assert "CLAUDE_CWD" in out and "FEISHU_APP_ID" in out

    # 2) 删除单个值 / Remove single value
    child.sendline("inputs value rm CLAUDE_CWD")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    assert "已删除" in out or "Removed" in out

    # 验证只剩 1 个值 / Verify only 1 value left
    child.sendline("inputs value list")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    assert "CLAUDE_CWD" not in out
    assert "FEISHU_APP_ID" in out

    # 3) 清空所有值 / Clear all values
    child.sendline("inputs value clear")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    assert "已清理" in out or "cleared" in out.lower()

    # 验证缓存为空 / Verify cache is empty
    child.sendline("inputs value list")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    assert "{}" in out or "empty" in out.lower()


@pytest.mark.e2e
def test_inputs_get_definition(cli_proc: pexpect.spawn) -> None:
    """
    中文: 测试 inputs get <id> 获取单个 input 定义。
    English: Test inputs get <id> to retrieve single input definition.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_with_default.json
      2) inputs get FEISHU_APP_SECRET 应该显示完整定义
      3) 验证包含 password: true
    """
    print("[test_start] test_inputs_get_definition")
    child = cli_proc

    # 1) 加载 inputs 定义 / Load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_with_default.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 获取单个定义 / Get single definition
    child.sendline("inputs get FEISHU_APP_SECRET")
    time.sleep(0.3)
    out = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)
    print(f"inputs get output:\n{out}")

    # 3) 验证输出包含关键字段 / Verify output contains key fields
    assert "FEISHU_APP_SECRET" in out
    assert "password" in out.lower()
    assert "true" in out.lower() or '"password": true' in out
    assert "Feishu MCP App Secret" in out
