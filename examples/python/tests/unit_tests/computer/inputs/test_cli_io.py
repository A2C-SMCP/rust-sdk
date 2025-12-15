# -*- coding: utf-8 -*-
# filename: test_cli_io.py
# @Time    : 2025/9/21 17:09
# @Author  : JQQ
# @Email   : jiaqia@qknode.com
# @Software: PyCharm

import json

import pytest

from a2c_smcp.computer.inputs.cli_io import ainput_pick, ainput_prompt, arun_command


@pytest.mark.asyncio
async def test_ainput_prompt_returns_default_on_interrupt(monkeypatch):
    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            raise EOFError()

    # patch PromptSession constructor to return dummy
    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())

    value = await ainput_prompt("Enter:", default="def", password=True)
    assert value == "def"


@pytest.mark.asyncio
async def test_ainput_prompt_uses_default_on_empty(monkeypatch):
    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return ""

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())

    value = await ainput_prompt("Enter:", default="hello")
    assert value == "hello"


@pytest.mark.asyncio
async def test_ainput_prompt_returns_value(monkeypatch):
    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return "world"

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())

    value = await ainput_prompt("Enter:")
    assert value == "world"


@pytest.mark.asyncio
async def test_ainput_pick_single_happy_path(monkeypatch):
    inputs = iter(["1"])  # pick index 1

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return next(inputs)

    import a2c_smcp.computer.inputs.cli_io as cli_io

    # silence console printing by replacing console.print with no-op
    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick one", ["a", "b", "c"], multi=False)
    assert picked == "b"


@pytest.mark.asyncio
async def test_ainput_pick_multi_with_dedup_and_order(monkeypatch):
    inputs = iter(["2,0,2,0"])  # picks ["c","a","c","a"] -> dedup to ["c","a"]

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return next(inputs)

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick multi", ["a", "b", "c"], multi=True)
    assert picked == ["c", "a"]


@pytest.mark.asyncio
async def test_ainput_pick_default_on_interrupt_and_empty(monkeypatch):
    # first raise KeyboardInterrupt then return empty; both should yield default
    calls = {"n": 0}

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            calls["n"] += 1
            if calls["n"] == 1:
                raise KeyboardInterrupt()
            return ""

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick one", ["a", "b"], default_index=1, multi=False)
    assert picked == "b"

    # empty input path
    calls["n"] = 0
    picked2 = await ainput_pick("Pick one", ["a", "b"], default_index=0, multi=True)
    assert picked2 == ["a"]


@pytest.mark.asyncio
async def test_arun_command_raw_and_lines_and_json():
    # raw
    out = await arun_command("echo hello", parse="raw")
    # macOS echo appends newline; function strips
    assert out == "hello"

    # lines
    out_lines = await arun_command("printf 'a\\nb\\n'", parse="lines")
    assert out_lines == ["a", "b"]

    # json success
    js = {"x": 1, "y": [1, 2]}
    cmd = f"python - <<'PY'\nimport json,sys; print(json.dumps({json.dumps(js)}))\nPY"
    out_json = await arun_command(cmd, parse="json")
    assert out_json == js


@pytest.mark.asyncio
async def test_arun_command_timeout_and_error():
    # timeout
    with pytest.raises(TimeoutError):
        await arun_command("sleep 2", timeout=0.1)

    # non-zero exit -> RuntimeError
    with pytest.raises(RuntimeError):
        await arun_command("bash -c 'exit 7'")


@pytest.mark.asyncio
async def test_ainput_prompt_with_provided_session_branches(monkeypatch):
    """覆盖 ainput_prompt 在提供 session 时的异常与返回分支。"""

    # 1) 提供 session 且抛出 KeyboardInterrupt -> 返回默认值
    class Sess1:
        async def prompt_async(self, *_, **__):
            raise KeyboardInterrupt()

    v1 = await ainput_prompt("Q:", default="D", session=Sess1())
    assert v1 == "D"

    # 2) 提供 session 返回非空值
    class Sess2:
        async def prompt_async(self, *_, **__):
            return "val"

    v2 = await ainput_prompt("Q:", session=Sess2())
    assert v2 == "val"


@pytest.mark.asyncio
async def test_ainput_pick_invalid_then_out_of_range_then_ok_single(monkeypatch):
    """单选：先触发 ValueError，再触发越界，再成功。"""
    # 输入顺序：无效 -> 越界 -> 正确
    inputs = iter(["x", "9", "1"])  # x 触发 ValueError；9 越界；1 有效

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return next(inputs)

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick one", ["a", "b", "c"], multi=False)
    assert picked == "b"


@pytest.mark.asyncio
async def test_ainput_pick_invalid_then_out_of_range_then_ok_multi(monkeypatch):
    """多选：先触发 ValueError，再触发越界，再成功。"""
    inputs = iter(["bad", "5", "0,2"])  # bad -> ValueError；5 越界；0,2 有效

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            return next(inputs)

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick multi", ["a", "b", "c"], multi=True)
    assert picked == ["a", "c"]


@pytest.mark.asyncio
async def test_arun_command_json_fallback_and_exec_mode(tmp_path):
    """覆盖 json 解析失败的回退与 shell=False 执行分支。"""
    # 1) json 解析失败 -> 返回原始字符串
    out = await arun_command("echo not-json", parse="json")
    assert out == "not-json"

    # 2) shell=False 使用可执行路径与 cwd（实现约束：不接受参数，command 仅作可执行名）
    subdir = tmp_path / "d"
    subdir.mkdir()
    # 使用 /bin/echo（无参数，输出仅为换行，strip 后为空字符串）
    res = await arun_command("/bin/echo", shell=False, cwd=str(subdir), parse="raw")
    assert res == ""


@pytest.mark.asyncio
async def test_ainput_pick_interrupt_with_invalid_default_multi(monkeypatch):
    """multi=True 且 default_index 越界时，KeyboardInterrupt 返回空列表。"""

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            raise KeyboardInterrupt()

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick multi", ["a", "b"], default_index=10, multi=True)
    assert picked == []


@pytest.mark.asyncio
async def test_ainput_pick_interrupt_with_invalid_default_single(monkeypatch):
    """multi=False 且 default_index 越界时，KeyboardInterrupt 返回空字符串。"""

    class DummySession:
        async def prompt_async(self, *args, **kwargs):
            raise EOFError()

    import a2c_smcp.computer.inputs.cli_io as cli_io

    monkeypatch.setattr(cli_io, "PromptSession", lambda: DummySession())
    monkeypatch.setattr(cli_io.console_util.console, "print", lambda *a, **k: None)

    picked = await ainput_pick("Pick one", ["a", "b"], default_index=999, multi=False)
    assert picked == ""


@pytest.mark.asyncio
async def test_arun_command_shell_false_missing_exec():
    """shell=False 且可执行不存在时应抛出 FileNotFoundError。"""
    with pytest.raises(FileNotFoundError):
        await arun_command("/bin/does-not-exist-xyz", shell=False)
