# -*- coding: utf-8 -*-
# filename: mock_sync_smcp_server.py
# @Time    : 2025/9/30 22:50
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
中文：同步 SMCP 服务器 Mock 实现，用于同步客户端集成测试。
English: Synchronous SMCP server Mock implementation for sync client integration tests.
"""

from typing import Any

from mcp.types import CallToolResult, TextContent
from socketio import Namespace, Server

from a2c_smcp.smcp import (
    ENTER_OFFICE_NOTIFICATION,
    SMCP_NAMESPACE,
    UPDATE_CONFIG_NOTIFICATION,
    UPDATE_DESKTOP_NOTIFICATION,
    EnterOfficeNotification,
    EnterOfficeReq,
    GetDeskTopReq,
    GetDeskTopRet,
    GetToolsReq,
    GetToolsRet,
    ListRoomReq,
    ListRoomRet,
    SessionInfo,
    SMCPTool,
    ToolCallReq,
    UpdateMCPConfigNotification,
)
from a2c_smcp.utils.logger import logger


class MockSyncSMCPNamespace(Namespace):
    """
    中文：同步 SMCP 命名空间 Mock 实现。
    English: Synchronous SMCP namespace Mock implementation.
    """

    def __init__(self) -> None:
        super().__init__(namespace=SMCP_NAMESPACE)
        # 存储会话信息：sid -> {role, name, office_id}
        # Store session info: sid -> {role, name, office_id}
        self.sessions: dict[str, dict[str, Any]] = {}

    def trigger_event(self, event: str, *args: Any) -> Any:
        """触发事件，重写触发逻辑，将冒号转换为下划线"""
        return super().trigger_event(event.replace(":", "_"), *args)

    def on_connect(self, sid: str, environ: dict, auth: dict | None = None) -> bool:
        logger.info(f"SocketIO Client {sid} connecting...")
        return True

    def on_disconnect(self, sid: str) -> None:
        logger.info(f"SocketIO Client {sid} disconnected")
        # 清理会话信息 / Clean up session info
        if sid in self.sessions:
            del self.sessions[sid]

    def on_server_join_office(self, sid: str, data: EnterOfficeReq) -> tuple[bool, str | None]:
        """处理加入办公室请求"""
        logger.info(f"Computer/Agent {sid} 加入房间 {data['office_id']}")

        # 存储会话信息 / Store session info
        self.sessions[sid] = {
            "sid": sid,
            "role": data["role"],
            "name": data["name"],
            "office_id": data["office_id"],
        }

        self.enter_room(sid, data["office_id"])

        # 广播进入办公室通知
        notification = EnterOfficeNotification(
            office_id=data["office_id"],
            computer=sid if data["role"] == "computer" else None,
            agent=sid if data["role"] == "agent" else None,
        )

        self.emit(
            ENTER_OFFICE_NOTIFICATION,
            notification,
            skip_sid=sid,
            room=data["office_id"],
        )
        return True, "加入成功"

    def on_server_update_config(self, sid: str, data: dict) -> tuple[bool, str | None]:
        """处理更新配置请求"""
        logger.info(f"Computer {sid} 更新配置")
        computer = data.get("computer", sid)

        # 广播配置更新通知
        notification = UpdateMCPConfigNotification(computer=computer)
        self.emit(UPDATE_CONFIG_NOTIFICATION, notification, skip_sid=sid)
        return True, "配置更新成功"

    def on_client_tool_call(self, sid: str, data: ToolCallReq) -> dict:
        """处理工具调用请求"""
        logger.info(f"Agent {sid} 调用工具 {data['tool_name']}")

        # 返回模拟的工具调用结果
        result = CallToolResult(
            isError=False,
            content=[TextContent(type="text", text="mock tool result")],
        )
        return result.model_dump(mode="json")

    def on_client_get_tools(self, sid: str, data: GetToolsReq) -> GetToolsRet:
        """处理获取工具列表请求"""
        logger.info(f"Agent {sid} 拉取工具列表")

        # 返回模拟的工具列表
        tools = [
            SMCPTool(
                name="echo",
                description="echo text",
                params_schema={"type": "object", "properties": {"text": {"type": "string"}}},
                return_schema=None,
            ),
            SMCPTool(
                name="test_tool",
                description="test tool",
                params_schema={},
                return_schema=None,
            ),
        ]

        return GetToolsRet(tools=tools, req_id=data["req_id"])

    def on_client_get_desktop(self, sid: str, data: GetDeskTopReq) -> GetDeskTopRet:
        """处理获取桌面请求（返回固定桌面数据）。"""
        logger.info(f"Agent {sid} 拉取桌面数据 size={data.get('desktop_size')}")
        desktops = ["window://mock\n\nhello world"]
        return GetDeskTopRet(desktops=desktops, req_id=data["req_id"])

    def on_server_update_desktop(self, sid: str, data: dict) -> tuple[bool, str | None]:
        """处理桌面更新请求并广播通知。"""
        logger.info(f"Computer {sid} 请求广播桌面更新")
        computer = data.get("computer", sid)
        self.emit(UPDATE_DESKTOP_NOTIFICATION, {"computer": computer}, skip_sid=sid)
        return True, None

    def on_server_list_room(self, sid: str, data: ListRoomReq) -> ListRoomRet:
        """
        列出指定房间内的所有会话信息
        List all sessions in the specified room

        Args:
            sid (str): 发起者ID，一般是Agent / Initiator ID, usually Agent
            data (ListRoomReq): 列出房间请求数据 / List room request data

        Returns:
            ListRoomRet: 房间内所有会话信息列表 / List of all session info in the room
        """
        office_id = data["office_id"]
        req_id = data["req_id"]

        logger.info(f"Agent {sid} 查询房间 {office_id} 的会话列表")

        # 过滤出指定房间内的所有会话 / Filter all sessions in the specified room
        sessions: list[SessionInfo] = []
        for _session_sid, session_data in self.sessions.items():
            if session_data.get("office_id") == office_id:
                session_info: SessionInfo = {
                    "sid": session_data["sid"],
                    "name": session_data["name"],
                    "role": session_data["role"],
                    "office_id": session_data["office_id"],
                }
                sessions.append(session_info)

        logger.info(f"房间 {office_id} 中找到 {len(sessions)} 个会话")
        return ListRoomRet(sessions=sessions, req_id=req_id)


def create_sync_smcp_socketio() -> Server:
    """
    创建同步 SMCP Socket.IO 服务器
    Create synchronous SMCP Socket.IO server

    Returns:
        Server: Socket.IO 服务器实例
    """
    sio = Server(
        cors_allowed_origins="*",
        ping_timeout=60,
        ping_interval=25,
        async_handlers=False,  # 重要：使用同步处理器
        always_connect=True,
    )

    # 注册 SMCP 命名空间
    sio.register_namespace(MockSyncSMCPNamespace())

    return sio
