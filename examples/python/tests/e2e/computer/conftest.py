"""
文件名: conftest.py
作者: JQQ
创建日期: 2025/9/22
最后修改日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: e2e 测试公共夹具，负责启动与关闭 CLI 交互进程。
  English: Common fixtures for e2e tests to spawn and cleanup CLI interactive process.
"""

from __future__ import annotations

import os
import shutil
import signal
import sys
from collections.abc import Iterator
from contextlib import contextmanager

import pytest

from tests.e2e.computer.utils import PROMPT_RE, expect_prompt_stable

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@contextmanager
def _spawn_cli(*extra_args: str, cwd: str | None = None) -> Iterator[pexpect.spawn]:
    """
    中文: 启动 CLI 交互进程，返回 pexpect child；确保在退出时清理。可通过 `cwd` 指定子进程工作目录，默认使用项目根目录。
    English: Spawn the CLI interactive process and ensure cleanup on exit. You can specify child process working directory via `cwd`;
        defaults to project root.
    """
    print("spawn cli...")
    env = os.environ.copy()
    # 确保 Python 输出不被缓冲，便于 pexpect 捕获 / Unbuffered Python output for stable pexpect reads
    env["PYTHONUNBUFFERED"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    # 降低 prompt_toolkit 的控制序列噪音（如 CPR），提升匹配稳定性
    # Reduce prompt_toolkit control sequences to stabilize matching
    env["PROMPT_TOOLKIT_NO_CPR"] = "1"
    env["PROMPT_TOOLKIT_DISABLE_BRACKETED_PASTE"] = "1"
    env["PROMPT_TOOLKIT_COMPLETE_STYLE"] = "column"
    env["PROMPT_TOOLKIT_EDITING_MODE"] = "emacs"
    env["PROMPT_TOOLKIT_MOUSE_SUPPORT"] = "0"
    env["PROMPT_TOOLKIT_ENABLE_SUSPEND"] = "0"
    # 使用最简终端，促使 prompt_toolkit 降级，减少 ANSI 控制序列
    # Use dumb TERM to reduce advanced terminal features
    env["TERM"] = "dumb"  # 使用最简单的终端类型
    # 禁用分页与固定列宽，进一步减少输出差异 / Disable pager and fix width to reduce variability
    env["PAGER"] = "cat"
    env["COLUMNS"] = "120"
    env["LINES"] = "24"
    env["NO_COLOR"] = "1"
    # 强制使用UTF-8编码 / Force UTF-8 encoding
    env["LC_ALL"] = "en_US.UTF-8"
    env["LANG"] = "en_US.UTF-8"
    # 优先使用已安装的 console script；否则回退到 python -c 调用 main()
    # Prefer console script if available; fallback to python -c main()
    console_script = shutil.which("a2c-computer")
    if console_script:
        args = [console_script, "--no-color", "run"]
    else:
        print("a2c-computer not found in shell")
        args = [
            sys.executable,
            "-c",
            "from a2c_smcp.computer.cli.main import main; main()",
            "--no-color",
            "run",
        ]
    if extra_args:
        args.extend(extra_args)

    # 计算与设置工作目录 / Compute and set working directory
    # 默认将工作目录设置为项目根目录（本文件位于 tests/e2e/conftest.py，向上两级即为项目根）
    # By default, set cwd to project root (this file lives at tests/e2e/conftest.py; go up two levels)
    project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
    spawn_cwd = cwd or project_root

    print("a2c-computer starting...")
    # 保证每次发送前有一个时延保持稳定
    child = pexpect.spawn(args[0], args[1:], env=env, encoding="utf-8", timeout=60, cwd=spawn_cwd)
    # 控制窗口大小，减少 CPR 请求 / Set winsize to reduce CPR
    child.delaybeforesend = 0.1
    try:
        child.setwinsize(24, 120)
    except Exception:
        pass
    try:
        yield child
    finally:
        # 优雅退出; 若仍存活则强杀 / Try graceful exit then hard kill if needed
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


@pytest.fixture()
def cli_proc() -> Iterator[pexpect.spawn]:
    """
    中文: 提供一个已启动并等待在 `a2c>` 提示符的 CLI 进程。
    English: Provide a CLI process ready at `a2c>` prompt.
    """
    with _spawn_cli() as child:
        print("a2c-computer started up")
        child.expect([r"Enter interactive mode, type 'help' for commands", PROMPT_RE])
        # 若匹配到横幅，则继续等待提示符 / If banner matched, then wait for prompt
        if (
            child.match
            and hasattr(child.match, "re")
            and child.match.re
            and getattr(child.match.re, "pattern", "").startswith("Enter interactive")
        ):
            pass  # fall through to wait prompt below
        # 等待提示符，并在必要时发送空回车触发刷新 / Wait for prompt, poke with empty enter if needed
        for _ in range(5):
            try:
                print("waiting for [a2c>]...")
                expect_prompt_stable(child, quiet=0.5, max_wait=5.0)
                break
            except pexpect.TIMEOUT:
                child.sendline("")
        else:
            child.expect(PROMPT_RE)
        child.sendline("")
        expect_prompt_stable(child, quiet=0.5, max_wait=12.0)
        yield child
