"""
文件名: test_cli_inputs_resolve.py
作者: JQQ
创建日期: 2025/9/24
最后修改日期: 2025/9/24
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文: 通过 inputs load + server add 的方式，验证配置中的 ${input:xxx} 能被正确解析并用于启动 MCP Server。
  English: Verify that ${input:xxx} placeholders are correctly resolved after `inputs load` and used to start MCP Server.
"""

from __future__ import annotations

import time

import pytest

from tests.e2e.computer.utils import expect_prompt_stable

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@pytest.mark.e2e
def test_inputs_resolve_then_server_start(cli_proc: pexpect.spawn) -> None:
    """
    中文: 加载 inputs_basic.json 后，添加引用 ${input:SCRIPT} 的 server 配置，应能正确渲染并启动。
    English: After loading inputs_basic.json, adding a server config that references ${input:SCRIPT} should render
             correctly and start.

    步骤 Steps:
      1) inputs load @tests/e2e/computer/configs/inputs_basic.json
      2) server add @tests/e2e/computer/configs/server_using_input.json
      3) start all
      4) status 包含 e2e-inputs-test，tools 包含 hello
    """
    print("[test_start] test_inputs_resolve_then_server_start")
    child = cli_proc

    # 1) 加载 inputs 定义 / load inputs definitions
    child.sendline("inputs load @tests/e2e/computer/configs/inputs_basic.json")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 2) 添加引用 ${input:SCRIPT} 的 server / add server that references ${input:SCRIPT}
    child.sendline("server add @tests/e2e/computer/configs/server_using_input.json")
    # 输入一个回车，表示使用默认值
    child.sendline("\n")
    expect_prompt_stable(child, quiet=0.3, max_wait=10.0)

    # 3) 启动所有服务 / start all servers
    child.sendline("start all")
    # 给渲染/注册留一点时间 / allow a short time for render/registration
    time.sleep(1.0)
    # 中文: 等待“稳定提示符”，保证启动完成且提示符后无残留日志 / wait for stable prompt to ensure no trailing logs
    # English: Wait for a stable prompt so there are no trailing logs after the prompt
    expect_prompt_stable(child, quiet=0.5, max_wait=15.0)

    # 4) 轮询校验 status/tools / poll for status/tools
    def _assert_contains(cmd: str, needle: str, retries: int = 1, delay: float = 1.0) -> None:
        """
        中文: 在 e2e 交互中对输出进行轮询断言。为缓解提示符与表格输出的时序竞态，允许在发送命令后先短暂等待再匹配提示符。
        English: Poll-and-assert helper for e2e. To mitigate prompt vs. output racing, optionally wait a bit after
                 sending the command before expecting the prompt.
        """
        out: str = ""
        for attempt in range(1, retries + 1):
            print(f"a2c>{cmd} [attempt {attempt}/{retries}]")
            child.sendline(cmd)
            # 给渲染/注册留一点时间 / allow a short time for render/registration
            time.sleep(delay)
            out = expect_prompt_stable(child, quiet=0.5, max_wait=15.0)
            print(out)
            if needle in out:
                return
            if attempt < retries:
                time.sleep(delay)
        assert needle in out, f"`{cmd}` 未包含 {needle}. 输出:\n{out}"

    _assert_contains("status", "e2e-inputs-test", retries=1, delay=0.8)
    time.sleep(1.0)
    # 中文: tools 输出涉及列举远端工具，首次枚举易受竞态影响；提高重试次数并在发送命令后预等待以增强稳定性
    # English: tools listing can race on first enumeration; increase retries and add pre-wait for stability
    _assert_contains("tools", "hello", retries=1, delay=1.0)
