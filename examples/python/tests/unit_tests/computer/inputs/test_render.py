# -*- coding: utf-8 -*-
# filename: test_render.py
# @Time    : 2025/9/21 17:09
# @Author  : JQQ
# @Email   : jiaqia@qknode.com
# @Software: PyCharm

import logging

import pytest

from a2c_smcp.computer.inputs.render import ConfigRender


@pytest.fixture(autouse=True)
def attach_project_logger_to_caplog(caplog):
    """
    Ensure logs from the project logger "a2c_smcp" are captured by pytest's caplog.
    The project logger disables propagation and has its own handlers, so caplog would
    not capture by default. We attach caplog.handler for the duration of each test.
    """
    logger = logging.getLogger("a2c_smcp")
    # Remember state
    prev_level = logger.level
    prev_propagate = logger.propagate

    # Set a permissive level for capturing and attach caplog's handler
    logger.setLevel(logging.DEBUG)
    logger.addHandler(caplog.handler)

    try:
        yield
    finally:
        # Detach and restore
        try:
            logger.removeHandler(caplog.handler)
        except Exception:
            pass
        logger.setLevel(prev_level)
        logger.propagate = prev_propagate


@pytest.mark.asyncio
async def test_arender_str_no_placeholders_returns_same():
    s = "no placeholders here"
    out = await ConfigRender.arender_str(s, resolve_input=lambda _id: None)
    assert out == s


@pytest.mark.asyncio
async def test_arender_str_single_placeholder_non_string_value():
    async def resolver(_id: str):
        return {"a": 1}

    out = await ConfigRender.arender_str("${input:x}", resolve_input=resolver)
    # when the string is exactly a single placeholder, return the non-string value directly
    assert out == {"a": 1}


@pytest.mark.asyncio
async def test_arender_str_multi_placeholders_coerced_to_string():
    async def resolver(_id: str):
        return 42 if _id == "x" else "ok"

    out = await ConfigRender.arender_str("A=${input:x},B=${input:y}", resolve_input=resolver)
    assert out == "A=42,B=ok"


@pytest.mark.asyncio
async def test_arender_str_missing_input_keeps_original_and_logs(caplog):
    caplog.set_level("WARNING", logger="a2c_smcp")

    async def resolver(_id: str):
        raise KeyError(_id)

    out = await ConfigRender.arender_str("Hello ${input:id}", resolve_input=resolver)
    assert out == "Hello ${input:id}"
    assert any("未找到输入项" in rec.message for rec in caplog.records)


@pytest.mark.asyncio
async def test_arender_recursive_dict_and_list_and_depth_limit(caplog):
    caplog.set_level("ERROR", logger="a2c_smcp")

    cr = ConfigRender(max_depth=2)

    async def resolver(_id: str):
        return "X"

    data = {"k": ["a", "${input:foo}"]}
    # depth should be fine here
    out = await cr.arender(data, resolver)
    assert out == {"k": ["a", "X"]}

    # For depth limit behavior, construct nested levels exceeding max_depth
    deep = {"a": {"b": {"c": "${input:foo}"}}}
    out2 = await cr.arender(deep, resolver)
    # Since depth limit is hit, inner dict remains unrendered
    assert out2["a"]["b"]["c"] == "${input:foo}"
    assert any("渲染深度超过限制" in rec.message for rec in caplog.records)
