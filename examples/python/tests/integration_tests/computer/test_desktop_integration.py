# -*- coding: utf-8 -*-
# filename: test_desktop_integration.py
# @Time    : 2025/10/02 16:24
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
Desktop 集成测试占位
Integration test placeholder for Desktop pipeline

说明 / Notes:
- 由于 organize 策略尚未定稿，本文件先整体跳过；
- 等策略稳定后，将接入真实的 MCPServer mock，贯通 BaseClient -> Manager -> Computer -> organize。
"""

import pytest


@pytest.mark.skip(reason="organize 策略未稳定，集成测试暂时跳过 / organizing policy not stable yet")
class TestDesktopIntegration:
    def test_placeholder(self):
        assert True
