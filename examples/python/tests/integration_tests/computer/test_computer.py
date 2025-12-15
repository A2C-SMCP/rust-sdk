# -*- coding: utf-8 -*-
# filename: test_computer.py
# @Time    : 2025/8/26
# @Author  : JQQ
# @Email   : jiaqia@qknode.com
# @Software: PyCharm
"""
集成测试：Computer.aexecute_tool 工具调用
Integration test: Computer.aexecute_tool tool execution
"""

from unittest.mock import MagicMock

import pytest
from prompt_toolkit import PromptSession

from a2c_smcp.computer.computer import Computer
from a2c_smcp.computer.mcp_clients.model import SseServerConfig, StdioServerConfig, ToolMeta


@pytest.mark.anyio
async def test_computer_aexecute_tool_success(stdio_params, sse_params, sse_server) -> None:
    """
    测试通过Computer.aexecute_tool调用工具（自动通过）
    Test Computer.aexecute_tool with auto_apply tool

    StdioServer中只有一个可用工具 hello 参数为name 返回值 f"Hello, {name}!"
    """
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params, tool_meta={"hello": ToolMeta(auto_apply=True)})
    sse_cfg = SseServerConfig(name="sse_server", server_parameters=sse_params)
    computer = Computer(name="test", mcp_servers={stdio_cfg, sse_cfg})
    await computer.boot_up()
    # 获取可用工具/Get available tools
    tools = await computer.aget_available_tools()
    assert tools, "No tools available"
    tool_name = "hello"
    # 调用工具/Call tool
    result = await computer.aexecute_tool("reqid", tool_name, {"name": "China"})
    assert hasattr(result, "content")
    assert result.content, "Tool call result should have content"
    assert result.content[0].text == "Hello, China!"


@pytest.mark.anyio
async def test_computer_aexecute_tool_confirm_callback_called(stdio_params):
    """
    测试tool_meta为空时会触发二次确认回调，并保证回调被调用
    Test that when tool_meta is empty, confirm_callback is triggered and called
    """
    from a2c_smcp.computer.mcp_clients.model import StdioServerConfig

    # tool_meta为空/No auto_apply
    confirm_mock = MagicMock(return_value=True)
    stdio_cfg = StdioServerConfig(name="stdio_server", server_parameters=stdio_params, tool_meta={})
    computer = Computer(name="test", mcp_servers={stdio_cfg}, confirm_callback=confirm_mock)
    await computer.boot_up()
    await computer.aget_available_tools()
    tool_name = "hello"
    result = await computer.aexecute_tool("reqid", tool_name, {"name": "China"})
    assert hasattr(result, "content")
    assert confirm_mock.called, "confirm_callback should be called"
    assert result.content[0].text == "Hello, China!"


# -------------------- 以下为动态管理相关集成用例 --------------------


class _DictResolver:
    """简单的 resolver：根据映射返回值，模拟 inputs 解析。"""

    def __init__(self, mapping: dict[str, object]):
        self._m = mapping

    async def aresolve_by_id(self, input_id: str, *, session: PromptSession | None = None):  # noqa: D401
        return self._m[input_id]

    def clear_cache(self, key: str | None = None):  # noqa: D401
        return None


@pytest.mark.anyio
async def test_dynamic_add_with_inputs_and_tool_call(stdio_params) -> None:
    """
    动态添加：使用带有 ${input:...} 的原始 dict 配置，确保渲染后能连接 stdio 测试服务并调用 hello 工具。
    """
    # 将fixture提供的参数拆出值，作为 inputs 渲染的来源
    cmd = stdio_params.command
    args = list(stdio_params.args or [])
    cwd = stdio_params.cwd
    env = stdio_params.env

    # 原始字典配置，包含占位符
    cfg_dict: dict = {
        "type": "stdio",
        "name": "dyn_stdio",
        "disabled": False,
        "server_parameters": {
            "command": "${input:cmd}",
            "args": ["${input:arg0}"] + (["${input:arg1}"] if len(args) > 1 else []),
            "env": env,
            "cwd": cwd,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
        "forbidden_tools": [],
        "tool_meta": {"hello": ToolMeta(auto_apply=True).model_dump(mode="json")},
    }

    # 提供解析器，返回与 fixture 相符的值
    resolver = _DictResolver(
        {
            "cmd": cmd,
            "arg0": args[0] if args else "",
            "arg1": args[1] if len(args) > 1 else "",
        },
    )

    # 初始化 Computer（auto_connect=true 便于动态添加后立即连接）
    computer = Computer(name="test", auto_connect=True, input_resolver=resolver)

    # 动态添加
    await computer.aadd_or_aupdate_server(cfg_dict)

    # 获取工具并调用
    tools = await computer.aget_available_tools()
    assert any(t["name"] == "hello" for t in tools)

    ret = await computer.aexecute_tool("reqid", "hello", {"name": "China"})
    assert ret.content and ret.content[0].text == "Hello, China!"


@pytest.mark.anyio
async def test_dynamic_update_forbid_tool_then_call_fails(stdio_params) -> None:
    """
    动态更新：先添加服务，后通过 forbidden_tools 禁用 hello 工具，随后调用应失败。
    """
    computer = Computer(name="test", auto_connect=True, confirm_callback=lambda *_: True)
    # 先添加（直接用模型，不涉及 inputs）
    cfg = StdioServerConfig(name="dyn_stdio2", server_parameters=stdio_params)
    await computer.aadd_or_aupdate_server(cfg)

    # 确认可调用
    ret1 = await computer.aexecute_tool("reqid", "hello", {"name": "China"})
    assert ret1.content and ret1.content[0].text == "Hello, China!"

    # 更新：禁用工具
    cfg2 = StdioServerConfig(name="dyn_stdio2", server_parameters=stdio_params, forbidden_tools=["hello"])
    await computer.aadd_or_aupdate_server(cfg2)

    # 调用应被 Manager 拒绝
    with pytest.raises(PermissionError):
        await computer.aexecute_tool("reqid", "hello", {"name": "China"})


@pytest.mark.anyio
async def test_dynamic_remove_then_tool_not_found(stdio_params) -> None:
    """
    动态移除：移除后再调用工具应失败（找不到工具）。
    """
    computer = Computer(name="test", auto_connect=True, confirm_callback=lambda *_: True)
    cfg = StdioServerConfig(name="dyn_stdio3", server_parameters=stdio_params)
    await computer.aadd_or_aupdate_server(cfg)

    # 调用成功一次
    ret1 = await computer.aexecute_tool("reqid", "hello", {"name": "China"})
    assert ret1.content and ret1.content[0].text == "Hello, China!"

    # 移除
    await computer.aremove_server("dyn_stdio3")

    # 再次调用应报错：工具不存在
    with pytest.raises(ValueError):
        await computer.aexecute_tool("reqid", "hello", {"name": "China"})


# -------------------- 新增：工具调用历史集成用例 --------------------


@pytest.mark.anyio
async def test_tool_call_history_records_success_and_order(stdio_params) -> None:
    """
    中文：验证 aexecute_tool 成功调用后，历史中包含记录，且字段正确、顺序正确。
    English: Verify history has records after successful calls with correct fields and order.
    """
    cfg = StdioServerConfig(name="hist_stdio", server_parameters=stdio_params, tool_meta={"hello": ToolMeta(auto_apply=True)})
    computer = Computer(name="test", mcp_servers={cfg})
    await computer.boot_up()

    # 进行两次调用/Two calls
    r1 = await computer.aexecute_tool("req-1", "hello", {"name": "A"})
    r2 = await computer.aexecute_tool("req-2", "hello", {"name": "B"})
    assert r1.content and r2.content

    hist = await computer.aget_tool_call_history()
    assert len(hist) >= 2

    # 校验最后一条为第二次调用/Last is second call
    last = hist[-1]
    assert last["req_id"] == "req-2"
    assert last["tool"] == "hello"
    assert last["server"] == "hist_stdio"
    assert last["parameters"] == {"name": "B"}
    assert last["timeout"] is None
    assert last["success"] is True
    assert isinstance(last["timestamp"], str) and last["timestamp"]


@pytest.mark.anyio
async def test_tool_call_history_maxlen(stdio_params) -> None:
    """
    中文：验证历史仅保留最近10条。
    English: Verify history keeps only last 10 records.
    """
    cfg = StdioServerConfig(name="hist_maxlen", server_parameters=stdio_params, tool_meta={"hello": ToolMeta(auto_apply=True)})
    computer = Computer(name="test", mcp_servers={cfg})
    await computer.boot_up()

    # 执行12次，maxlen=10后应只保留后10次/Execute 12 times, keep last 10
    for i in range(12):
        rid = f"req-{i + 1}"
        await computer.aexecute_tool(rid, "hello", {"name": str(i + 1)})

    hist = await computer.aget_tool_call_history()
    assert len(hist) == 10
    # 保留的是 3..12 的 req_id，其中最后一条是 12
    assert hist[0]["req_id"] == "req-3"
    assert hist[-1]["req_id"] == "req-12"


@pytest.mark.anyio
async def test_tool_call_history_confirm_callback_exception(stdio_params) -> None:
    """
    中文：当确认回调抛出异常时，应记录一条失败的历史记录。
    English: When confirm callback raises, a failed history record should be stored.
    """

    def bad_confirm(*_args, **_kwargs):
        raise RuntimeError("confirm failed")

    cfg = StdioServerConfig(name="hist_confirm_err", server_parameters=stdio_params, tool_meta={})
    computer = Computer(name="test", mcp_servers={cfg}, confirm_callback=bad_confirm)
    await computer.boot_up()

    result = await computer.aexecute_tool("req-X", "hello", {"name": "Err"})
    assert result.isError is True

    hist = await computer.aget_tool_call_history()
    assert len(hist) >= 1
    last = hist[-1]
    assert last["req_id"] == "req-X"
    assert last["tool"] == "hello"
    assert last["server"] == "hist_confirm_err"
    assert last["success"] is False
    assert last["error"] is not None
