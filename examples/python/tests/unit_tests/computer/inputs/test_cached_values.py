# -*- coding: utf-8 -*-
# 文件名: test_cached_values.py
# 作者: JQQ
# 创建日期: 2025/9/24
# 最后修改日期: 2025/9/24
# 版权: 2023 JQQ. All rights reserved.
# 依赖: pytest
# 描述:
#   中文: 覆盖 InputResolver 的当前值缓存增删改查能力。
#   English: Cover CRUD capabilities for current values cache in InputResolver.

import pytest

from a2c_smcp.computer.inputs.resolver import InputResolver
from a2c_smcp.computer.mcp_clients.model import MCPServerPromptStringInput


@pytest.mark.asyncio
async def test_inputresolver_cached_values_crud() -> None:
    # 中文: 准备一个包含一个输入定义的解析器
    # English: Prepare a resolver with a single input definition
    inputs = [MCPServerPromptStringInput(id="token", description="desc", default="D", password=False, type="promptString")]
    resolver = InputResolver(inputs, session=None)

    # 初始应无缓存 / no cache initially
    assert resolver.list_cached_values() == {}
    assert resolver.get_cached_value("token") is None

    # 设置缓存 / set cache
    ok = resolver.set_cached_value("token", "ABC")
    assert ok is True
    assert resolver.get_cached_value("token") == "ABC"

    # 列出缓存 / list cache
    snap = resolver.list_cached_values()
    assert snap == {"token": "ABC"}

    # 删除缓存 / delete cache
    removed = resolver.delete_cached_value("token")
    assert removed is True
    assert resolver.get_cached_value("token") is None

    # 清空缓存 / clear
    resolver.set_cached_value("token", "XYZ")
    resolver.clear_cache()
    assert resolver.list_cached_values() == {}

    # 删除不存在键返回 False / delete non-exist returns False
    assert resolver.delete_cached_value("nope") is False

    # 设置不存在 id 返回 False / set for unknown id returns False
    assert resolver.set_cached_value("nope", "x") is False
