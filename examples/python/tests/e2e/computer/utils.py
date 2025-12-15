# -*- coding: utf-8 -*-
# filename: utils.py
# @Time    : 2025/9/28 12:54
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
from __future__ import annotations

import re
import time

import pexpect

# 中文: ANSI 控制序列匹配与去除工具，避免 prompt_toolkit 的控制序列影响断言
# English: ANSI control-sequence helpers to avoid prompt_toolkit artifacts breaking assertions


ANSI = r"(?:\x1b\[[0-?]*[ -/]*[@-~])*"
PROMPT_RE = re.compile(ANSI + r"a2c>" + ANSI)


def strip_ansi(s: str) -> str:
    return re.sub(ANSI, "", s)


def expect_prompt_stable(child: pexpect.spawn, *, quiet: float = 0.3, max_wait: float = 10.0) -> str | None:
    """
    中文: 等待直到捕获到“最后一个稳定的 a2c> 提示符”，即提示符出现后在 quiet 秒内没有任何新增输出。
          返回：该稳定提示符之前的输出（已去除 ANSI），供断言使用。
    English: Wait until we catch the "last stable a2c> prompt" – i.e., after the prompt appears, no new output arrives
             within the quiet window. Returns the output before that stable prompt (ANSI stripped) for assertions.
    """
    deadline = time.time() + max_wait
    last_out: str
    err: bool = False
    while True:
        remaining = max(0.05, deadline - time.time())
        try:
            # 等待下一次提示符 / wait for the next prompt
            child.expect(PROMPT_RE, timeout=remaining)
        except TimeoutError as e:
            print(f"timeout: {remaining}. ")
            err = True
            raise e
        finally:
            last_out = strip_ansi((child.before or "").strip())
            if err:
                print(f"output: {last_out}")
        # 在 quiet 窗口内观察是否还有新输出 / observe if any new output arrives within the quiet window
        try:
            # read_nonblocking 会消费数据；若有数据说明提示符后仍有输出，继续等待下一次提示符
            # read_nonblocking consumes data; if any is received, more output is coming after the prompt – keep waiting
            chunk = child.read_nonblocking(size=4096, timeout=quiet)
            if chunk:
                # 若 quiet 内有输出，则不是稳定提示符，继续下一轮 / not stable yet, continue
                continue
        except Exception:  # 包含 pexpect.TIMEOUT
            # 超时表示 quiet 窗口内没有任何新输出，认为稳定 / timeout => stable prompt
            return last_out or ""
        # 若时间已用尽，返回当前捕获 / deadline reached, return current capture
        if time.time() >= deadline:
            return last_out or ""
