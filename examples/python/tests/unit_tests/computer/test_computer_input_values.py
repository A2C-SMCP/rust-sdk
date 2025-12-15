# -*- coding: utf-8 -*-
# 文件名: test_computer_input_values.py
# 作者: JQQ
# 创建日期: 2025/9/24
# 最后修改日期: 2025/9/24
# 版权: 2023 JQQ. All rights reserved.
# 依赖: pytest
# 描述:
#   中文: 覆盖 Computer 对“当前 inputs 值（缓存）”的封装方法。
#   English: Cover Computer's wrappers for current input values (cache).

import pytest

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import MCPServerPromptStringInput


@pytest.mark.asyncio
async def test_computer_input_values_crud_wrappers() -> None:
    # 中文: 准备一个包含一个输入定义的 Computer
    # English: Prepare a Computer with one input definition
    inputs = {MCPServerPromptStringInput(id="api_key", description="desc", default=None, password=True, type="promptString")}
    comp = Computer(name="test", inputs=inputs)

    # 初始应为空 / empty initially
    assert comp.list_input_values() == {}
    assert comp.get_input_value("api_key") is None

    # set 生效（存在该 id） / set works when id exists
    assert comp.set_input_value("api_key", "K-123") is True
    assert comp.get_input_value("api_key") == "K-123"

    # list 快照 / list snapshot
    snap = comp.list_input_values()
    assert snap == {"api_key": "K-123"}

    # remove 指定键 / remove specific key
    assert comp.remove_input_value("api_key") is True
    assert comp.get_input_value("api_key") is None

    # clear 全部 / clear all
    assert comp.set_input_value("api_key", "Z") is True
    comp.clear_input_values()
    assert comp.list_input_values() == {}

    # set 不存在 id 返回 False / setting unknown id returns False
    assert comp.set_input_value("nope", "x") is False
