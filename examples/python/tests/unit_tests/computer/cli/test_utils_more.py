# -*- coding: utf-8 -*-
"""
文件名: test_utils_more.py
作者: JQQ
创建日期: 2025/9/29
最后修改日期: 2025/9/29
版权: 2023 JQQ. All rights reserved.
依赖: pytest
描述:
  中文: 补充覆盖 `print_mcp_config` 的空与非空场景，确保稳定输出。
  English: Add coverage for `print_mcp_config` empty and non-empty cases to ensure stable printing.
"""

from __future__ import annotations

from a2c_smcp.computer.cli.utils import print_mcp_config


def test_print_mcp_config_empty() -> None:
    # 不应抛出异常 / Should not raise
    print_mcp_config({})


def test_print_mcp_config_non_empty() -> None:
    cfg = {
        "servers": {
            "s1": {"type": "stdio", "disabled": False},
            "s2": {"type": "http", "disabled": True},
        },
        "inputs": [
            {"id": "A", "type": "promptString", "description": "alpha"},
            {"id": "B", "type": "pickString", "description": "beta"},
        ],
    }
    print_mcp_config(cfg)
