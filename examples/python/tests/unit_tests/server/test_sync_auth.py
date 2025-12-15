# -*- coding: utf-8 -*-
"""
测试 a2c_smcp/server/sync_auth.py
"""

from __future__ import annotations

from unittest.mock import MagicMock

from a2c_smcp.server.sync_auth import DefaultSyncAuthenticationProvider

# get_agent_id 方法已被移除，不再需要测试
# get_agent_id method has been removed, no longer need to test


def test_sync_authenticate_variants():
    prov = DefaultSyncAuthenticationProvider("adm")
    sio = MagicMock()
    environ = {}

    ok = prov.authenticate(sio, environ, None, [(b"x-api-key", b"adm")])
    assert ok is True

    assert prov.authenticate(sio, environ, None, []) is False
    assert prov.authenticate(sio, environ, None, [(b"x-api-key", b"wrong")]) is False


# has_admin_permission 方法已被移除，管理员权限检查已集成到 authenticate 方法中
# has_admin_permission method has been removed, admin permission check is now integrated into authenticate method


def test_sync_admin_permission_integrated_in_authenticate():
    """测试管理员权限检查已集成到认证方法中 / Test admin permission check integrated in authenticate method"""
    sio = MagicMock()
    environ = {}

    # 无管理员密钥配置的提供者
    # Provider without admin secret configured
    prov1 = DefaultSyncAuthenticationProvider(None)
    assert prov1.authenticate(sio, environ, None, [(b"x-api-key", b"any_key")]) is False

    # 有管理员密钥配置的提供者
    # Provider with admin secret configured
    prov2 = DefaultSyncAuthenticationProvider("adm")
    assert prov2.authenticate(sio, environ, None, [(b"x-api-key", b"adm")]) is True
