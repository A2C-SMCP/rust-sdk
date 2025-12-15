"""
文件名: test_cli_history.py
作者: JQQ
创建日期: 2025/10/02
最后修改日期: 2025/10/02
版权: 2023 JQQ. All rights reserved.
依赖: pytest, pexpect
描述:
  中文:
    - 端到端验证交互式 CLI 的 `history` 命令：先通过 `tc` 触发一次工具调用，再读取历史并断言包含该请求ID。
    - 采用与其他 e2e 用例一致的真实进程 + pexpect 流程与夹具。

  English:
    - E2E test for CLI `history`: trigger one tool call via `tc`, then read history and assert it contains the req_id.
    - Reuse the same real-process + pexpect fixtures as other e2e tests.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

from tests.e2e.computer.utils import expect_prompt_stable, strip_ansi

pexpect = pytest.importorskip("pexpect", reason="e2e tests require pexpect; install with `pip install pexpect`.")


@pytest.mark.e2e
def test_history_after_tc(cli_proc: pexpect.spawn, tmp_path: Path) -> None:
    """
    中文: 通过 `tc` 调用 `hello`，随后 `history` 应包含相同的 req_id。
    English: After calling `hello` via `tc`, `history` should contain the same req_id.
    """
    child = cli_proc

    # 1) server 配置：开启 auto_apply
    server_cfg = {
        "name": "e2e-hist",
        "type": "stdio",
        "disabled": False,
        "forbidden_tools": [],
        "tool_meta": {"hello": {"auto_apply": True}},
        "server_parameters": {
            "command": sys.executable,  # 使用当前 Python 解释器 / Use current Python interpreter
            "args": [
                "tests/integration_tests/computer/mcp_servers/direct_execution.py",
            ],
            "env": None,
            "cwd": None,
            "encoding": "utf-8",
            "encoding_error_handler": "strict",
        },
    }
    cfg_path = tmp_path / "server_e2e_hist.json"
    cfg_path.write_text(json.dumps(server_cfg, ensure_ascii=False), encoding="utf-8")

    # 2) add + start
    child.sendline(f"server add @{cfg_path}")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)
    child.sendline("start e2e-hist")
    expect_prompt_stable(child, quiet=0.6, max_wait=15.0)

    # 3) tools 确认一下 hello 可用
    child.sendline("tools")
    tools_out = expect_prompt_stable(child, quiet=0.6, max_wait=12.0)
    assert "hello" in strip_ansi(tools_out)

    # 4) 通过 tc 触发一次调用（固定 req_id 以便断言）
    req_id = "req-e2e-history-1"
    tc_payload = {
        "agent": "bot-e2e",
        "req_id": req_id,
        "computer": "ignored",
        "tool_name": "hello",
        "params": {"name": "Hist"},
        "timeout": 10,
    }
    child.sendline(f"tc {json.dumps(tc_payload, ensure_ascii=False)}")
    expect_prompt_stable(child, quiet=0.8, max_wait=20.0)

    # 5) 读取 history 并断言包含 req_id
    child.sendline("history")
    hist_out = expect_prompt_stable(child, quiet=0.6, max_wait=12.0)
    hist_out = strip_ansi(hist_out)
    assert req_id in hist_out, f"history does not contain req_id {req_id}. Output:\n{hist_out}"
