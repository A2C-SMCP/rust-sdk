"""
文件名: test_cli_run_with_config.py
作者: JQQ
创建日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: 启动 CLI 时通过 --config/-c 传入配置文件，验证服务加载与工具可见。
  English: Pass --config at startup to load servers config and verify tools.
"""

from __future__ import annotations

import os
import re
import shutil
import signal
import sys
import time
from contextlib import contextmanager

import pytest

from tests.e2e.computer.utils import strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


# ANSI-aware prompt matching and helper to strip ANSI sequences
# 与 conftest/test_cli_interactive 保持一致，确保在包含控制序列/光标移动的终端下也能稳定匹配提示符
ANSI = r"(?:\x1b\[[0-?]*[ -/]*[@-~])*"
PROMPT_RE = re.compile(ANSI + r"a2c>" + ANSI)


@contextmanager
def _spawn_cli_with_args(*extra_args: str):
    env = os.environ.copy()
    env.setdefault("PYTHONUNBUFFERED", "1")
    env.setdefault("PROMPT_TOOLKIT_NO_CPR", "1")
    env.setdefault("PROMPT_TOOLKIT_DISABLE_BRACKETED_PASTE", "1")
    env.setdefault("TERM", "dumb")
    console_script = shutil.which("a2c-computer")
    if console_script:
        args = [console_script, "--no-color", "run", *extra_args]
    else:
        args = [
            sys.executable,
            "-c",
            "from a2c_smcp_cc.cli.main import main; main()",
            "--no-color",
            "run",
            *extra_args,
        ]
    # 计算与设置工作目录 / Compute and set working directory
    # 默认将工作目录设置为项目根目录（本文件位于 tests/e2e/conftest.py，向上两级即为项目根）
    # By default, set cwd to project root (this file lives at tests/e2e/conftest.py; go up two levels)
    project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
    child = pexpect.spawn(args[0], args[1:], env=env, encoding="utf-8", timeout=25, cwd=project_root)
    try:
        child.setwinsize(24, 120)
    except Exception:
        pass
    try:
        yield child
    finally:
        if child.isalive():
            try:
                child.sendline("exit")
                child.expect([pexpect.EOF, "Bye"], timeout=5)
            except Exception:
                pass
        if child.isalive():
            try:
                child.kill(signal.SIGKILL)
            except Exception:
                pass


def _wait_prompt(child: pexpect.spawn, timeout: float = 15.0) -> None:
    child.expect(PROMPT_RE, timeout=timeout)


def _assert_tools(child: pexpect.spawn, name: str, retries: int = 10, delay: float = 1.0) -> None:
    for _ in range(retries):
        child.sendline("tools")
        _wait_prompt(child)
        out = strip_ansi((child.before or "").strip())
        if name in out:
            return
        time.sleep(delay)
    child.sendline("tools")
    _wait_prompt(child)
    out = strip_ansi((child.before or "").strip())
    assert name in out, f"tools 未包含 {name}. 输出:\n{out}"


def _assert_status(child: pexpect.spawn, server: str, retries: int = 8, delay: float = 0.8) -> None:
    for _ in range(retries):
        child.sendline("status")
        _wait_prompt(child)
        out = strip_ansi((child.before or "").strip())
        if server in out:
            return
        time.sleep(delay)
    child.sendline("status")
    _wait_prompt(child)
    out = strip_ansi((child.before or "").strip())
    assert server in out, f"status 未出现 {server}. 输出:\n{out}"


@pytest.mark.e2e
def test_run_with_config_param_loads_server() -> None:
    """
    启动参数包含 --config @tests/e2e/computer/configs/server_direct_execution.json，应能加载并启动服务：
    - 进入 a2c> 后检查 status 含 e2e-test
    - tools 中包含 hello
    若自动启动存在延迟，调用一次 start all 作为补偿
    """
    cfg_arg = "--config=@tests/e2e/computer/configs/server_direct_execution.json"
    with _spawn_cli_with_args(cfg_arg) as child:
        # 等横幅/提示符
        try:
            child.expect("Enter interactive mode, type 'help' for commands")
        except Exception:
            pass
        # 可能需要轻推以出现提示符
        for _ in range(5):
            try:
                _wait_prompt(child, timeout=5)
                break
            except pexpect.TIMEOUT:
                child.sendline("")
        else:
            _wait_prompt(child, timeout=10)

        # 若 auto-connect 未马上激活，补打一遍 start all
        child.sendline("start all")
        _wait_prompt(child)

        _assert_status(child, "e2e-test", retries=10, delay=0.8)
        _assert_tools(child, "hello", retries=12, delay=1.0)
