# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/auth.py
"""

from __future__ import annotations

from unittest.mock import AsyncMock

import pytest

from a2c_smcp.server.auth import DefaultAuthenticationProvider

# get_agent_id 方法已被移除，不再需要测试
# get_agent_id method has been removed, no longer need to test


@pytest.mark.asyncio
async def test_authenticate_admin_ok_and_missing_or_wrong_key():
    prov = DefaultAuthenticationProvider("admin_secret")
    sio = AsyncMock()
    environ = {}

    # 正确 key
    headers = [(b"x-api-key", b"admin_secret")]
    ok = await prov.authenticate(sio, environ, None, headers)
    assert ok is True

    # 缺失 key
    headers = []
    ok2 = await prov.authenticate(sio, environ, None, headers)
    assert ok2 is False

    # 错误 key
    headers = [(b"x-api-key", b"wrong")]
    ok3 = await prov.authenticate(sio, environ, None, headers)
    assert ok3 is False


# has_admin_permission 方法已被移除，管理员权限检查已集成到 authenticate 方法中
# has_admin_permission method has been removed, admin permission check is now integrated into authenticate method


@pytest.mark.asyncio
async def test_admin_permission_integrated_in_authenticate():
    """测试管理员权限检查已集成到认证方法中 / Test admin permission check integrated in authenticate method"""
    sio = AsyncMock()
    environ = {}

    # 无管理员密钥配置的提供者
    # Provider without admin secret configured
    prov1 = DefaultAuthenticationProvider(None)
    headers = [(b"x-api-key", b"any_key")]
    assert await prov1.authenticate(sio, environ, None, headers) is False

    # 有管理员密钥配置的提供者
    # Provider with admin secret configured
    prov2 = DefaultAuthenticationProvider("admin_secret")
    headers = [(b"x-api-key", b"admin_secret")]
    assert await prov2.authenticate(sio, environ, None, headers) is True
