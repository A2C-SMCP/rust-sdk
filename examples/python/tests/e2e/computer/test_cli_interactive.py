"""
文件名: test_cli_interactive.py
作者: JQQ
创建日期: 2025/9/22
最后修改日期: 2025/9/22
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: 通过 pexpect 驱动真实进程的交互式 CLI e2e 测试用例。
  English: End-to-end tests for interactive CLI driven by pexpect against a real process.
"""

from __future__ import annotations

import re

import pytest

from tests.e2e.computer.utils import ANSI, PROMPT_RE, expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")

# 中文: 帮助标题的匹配，沿用 ANSI 感知的模式以避免误匹配 re.S(或re.DOTALL) 模式下 . 将匹配包括换行模式符在内的任意字符。因此必须强制关闭
# re.S 否则会拦截正常输出
# English: Help title regex using ANSI-aware pattern to avoid mismatches
HELP_TITLE_RE = re.compile(ANSI + r".*可用命令 / Commands.*")


@pytest.mark.e2e
def test_enter_does_nothing(cli_proc: pexpect.spawn) -> None:
    """
    中文: 在 a2c> 下按回车，应该不打印帮助，直接返回到下一次提示符。
    English: Pressing Enter at a2c> should do nothing and return to the next prompt without printing help.
    """
    child = cli_proc

    # 发送空回车并等待稳定提示符 / send empty Enter then wait for stable prompt
    child.sendline("")
    output = expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 不应出现帮助标题 / should not contain help title
    assert "可用命令 / Commands" not in output
    assert re.search(r"\bCommands\b", output) is None


@pytest.mark.e2e
def test_help_shows_table(cli_proc: pexpect.spawn) -> None:
    """
    中文: 输入 help 或 ? 能展示帮助表格。
    English: Typing help or ? should display the help table.
    """
    child = cli_proc

    # 请求帮助 / ask for help
    child.sendline("help")
    # 先等待帮助标题出现，再等待稳定提示符，避免抓空 / wait help title then stable prompt
    child.expect(HELP_TITLE_RE, timeout=5)
    child.expect(PROMPT_RE, timeout=5)
    output = strip_ansi((child.before or "").strip())

    assert "server add <json|@file>" in output
    assert "socket connect" in output

    # 再用 ? 验证一次 / verify with ? again
    child.sendline("?")
    child.expect(HELP_TITLE_RE)
    child.expect(PROMPT_RE, timeout=5)
    output2 = strip_ansi((child.before or "").strip())
    assert "server add <json|@file>" in output2
