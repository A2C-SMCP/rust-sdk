# -*- coding: utf-8 -*-
# filename: test_inputs.py
# @Time    : 2025/11/24 16:49
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm

"""
集成测试：Computer inputs 定义加载与值缓存行为
Integration test: Computer inputs definition loading and value cache behavior
"""

import pytest

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import MCPServerPromptStringInput


@pytest.mark.anyio
async def test_update_inputs_and_list_values() -> None:
    """
    中文: 测试 update_inputs 加载定义后，list_input_values 为空是正常行为。
    English: Test that after update_inputs loads definitions, list_input_values is empty (expected behavior).

    验证点 / Verification points:
    1. update_inputs 成功加载 inputs 定义
    2. inputs 定义包含 default 值
    3. list_input_values 返回空字典（因为没有实际解析）
    4. inputs 属性能正确返回所有定义
    """
    # 创建 Computer 实例 / Create Computer instance
    computer = Computer(name="test_computer")

    # 准备测试数据：与用户提供的示例一致 / Prepare test data: same as user's example
    test_inputs = {
        MCPServerPromptStringInput(
            id="CLAUDE_CWD",
            type="promptString",
            description="Claude MCP server working directory",
            default="/Users/huruize/PycharmProjects/TfrobotSceneTests",
            password=False,
        ),
        MCPServerPromptStringInput(
            id="FEISHU_APP_ID",
            type="promptString",
            description="Feishu MCP App ID",
            default="cli_a86666c7f478500e",
            password=False,
        ),
        MCPServerPromptStringInput(
            id="FEISHU_APP_SECRET",
            type="promptString",
            description="Feishu MCP App Secret",
            default="nxlKxbeKGcPGe1kNDa11EboJwUkYxFn6",
            password=True,
        ),
    }

    # 执行 update_inputs / Execute update_inputs
    computer.update_inputs(test_inputs)

    # 验证 1: inputs 定义已加载 / Verify 1: inputs definitions are loaded
    loaded_inputs = computer.inputs
    assert len(loaded_inputs) == 3, "应该加载 3 个 inputs 定义 / Should load 3 input definitions"

    # 验证 2: 检查每个 input 的定义是否正确 / Verify 2: check each input definition
    input_ids = {inp.id for inp in loaded_inputs}
    assert "CLAUDE_CWD" in input_ids
    assert "FEISHU_APP_ID" in input_ids
    assert "FEISHU_APP_SECRET" in input_ids

    # 验证 3: 检查 default 值存在 / Verify 3: check default values exist
    for inp in loaded_inputs:
        assert inp.default is not None, f"Input {inp.id} 应该有 default 值 / should have default value"

    # 验证 4: list_input_values 应该返回空字典 / Verify 4: list_input_values should return empty dict
    # 这是关键验证点：加载定义后，缓存应该为空
    # This is the key verification: after loading definitions, cache should be empty
    cached_values = computer.list_input_values()
    assert cached_values == {}, (
        "加载 inputs 定义后，缓存应该为空（default 不会自动填充）/ "
        "After loading input definitions, cache should be empty (defaults are not auto-populated)"
    )

    # 验证 5: 单独获取每个 input 的值也应该返回 None / Verify 5: getting individual values should return None
    assert computer.get_input_value("CLAUDE_CWD") is None
    assert computer.get_input_value("FEISHU_APP_ID") is None
    assert computer.get_input_value("FEISHU_APP_SECRET") is None


@pytest.mark.anyio
async def test_inputs_value_after_manual_set() -> None:
    """
    中文: 测试手动设置 input 值后，list_input_values 能正确返回。
    English: Test that after manually setting input values, list_input_values returns correctly.
    """
    computer = Computer(name="test_computer")

    # 加载定义 / Load definitions
    test_inputs = {
        MCPServerPromptStringInput(
            id="TEST_INPUT",
            type="promptString",
            description="Test input",
            default="default_value",
            password=False,
        ),
    }
    computer.update_inputs(test_inputs)

    # 初始缓存为空 / Initial cache is empty
    assert computer.list_input_values() == {}

    # 手动设置值 / Manually set value
    success = computer.set_input_value("TEST_INPUT", "manually_set_value")
    assert success is True, "设置值应该成功 / Setting value should succeed"

    # 现在缓存应该有值 / Now cache should have value
    cached_values = computer.list_input_values()
    assert cached_values == {"TEST_INPUT": "manually_set_value"}

    # 获取单个值 / Get individual value
    value = computer.get_input_value("TEST_INPUT")
    assert value == "manually_set_value"


@pytest.mark.anyio
async def test_inputs_value_after_resolve() -> None:
    """
    中文: 测试通过 aresolve_by_id 解析后，值会被缓存。
    English: Test that after resolving via aresolve_by_id, value is cached.

    注意：此测试需要模拟用户输入，暂时跳过实际解析测试。
    Note: This test requires mocking user input, skipping actual resolve test for now.
    """
    computer = Computer(name="test_computer")

    test_inputs = {
        MCPServerPromptStringInput(
            id="RESOLVE_TEST",
            type="promptString",
            description="Resolve test input",
            default="default_value",
            password=False,
        ),
    }
    computer.update_inputs(test_inputs)

    # 初始缓存为空 / Initial cache is empty
    assert computer.list_input_values() == {}

    # 注意：实际的 aresolve_by_id 需要用户交互，这里仅验证接口存在
    # Note: Actual aresolve_by_id requires user interaction, only verify interface exists
    assert hasattr(computer, "_input_resolver")
    assert hasattr(computer._input_resolver, "aresolve_by_id")


@pytest.mark.anyio
async def test_set_input_value_with_default() -> None:
    """
    中文: 测试通过 get_input 获取定义后，可以手动将 default 值设置到缓存。
    English: Test that after getting input definition, can manually set default value to cache.

    这模拟了 CLI 中 'inputs value set <id>' 不带值参数时的行为。
    This simulates the behavior of 'inputs value set <id>' without value parameter in CLI.
    """
    computer = Computer(name="test_computer")

    # 加载定义 / Load definitions
    test_inputs = {
        MCPServerPromptStringInput(
            id="WITH_DEFAULT",
            type="promptString",
            description="Input with default",
            default="my_default_value",
            password=False,
        ),
        MCPServerPromptStringInput(
            id="WITHOUT_DEFAULT",
            type="promptString",
            description="Input without default",
            default=None,
            password=False,
        ),
    }
    computer.update_inputs(test_inputs)

    # 初始缓存为空 / Initial cache is empty
    assert computer.list_input_values() == {}

    # 场景 1: 获取有 default 值的 input 定义，并手动设置 default 到缓存
    # Scenario 1: Get input with default, manually set default to cache
    input_def = computer.get_input("WITH_DEFAULT")
    assert input_def is not None
    assert input_def.default == "my_default_value"

    # 手动设置 default 值到缓存 / Manually set default to cache
    success = computer.set_input_value("WITH_DEFAULT", input_def.default)
    assert success is True

    # 验证缓存 / Verify cache
    cached_values = computer.list_input_values()
    assert cached_values == {"WITH_DEFAULT": "my_default_value"}

    # 场景 2: 没有 default 值的 input
    # Scenario 2: Input without default
    input_def_no_default = computer.get_input("WITHOUT_DEFAULT")
    assert input_def_no_default is not None
    assert input_def_no_default.default is None

    # 尝试设置 None 也是可以的 / Setting None is also allowed
    success = computer.set_input_value("WITHOUT_DEFAULT", None)
    assert success is True

    # 验证缓存 / Verify cache
    cached_values = computer.list_input_values()
    assert cached_values == {"WITH_DEFAULT": "my_default_value", "WITHOUT_DEFAULT": None}
