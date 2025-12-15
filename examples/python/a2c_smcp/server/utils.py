"""
* 文件名: utils
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: socketio, pydantic
* 描述: Server端工具函数 / Server-side utility functions
"""

from pydantic import TypeAdapter
from socketio import AsyncServer, Server

from a2c_smcp.server.types import OFFICE_ID, ComputerSession
from a2c_smcp.smcp import SMCP_NAMESPACE


async def aget_computers_in_office(office_id: OFFICE_ID, sio: AsyncServer) -> list[ComputerSession]:
    """
    从sio的 /smcp 命名空间中获取Computer所在房间的计算机列表
    Get list of computers in the room from the /smcp namespace of sio

    Args:
        office_id (OFFICE_ID): 房间号 / Room ID
        sio (AsyncServer): SocketIO实例 / SocketIO instance

    Returns:
        list[ComputerSession]: 计算机列表 / Computer list
    """
    computers = []
    # 这里office_id是房间号，但根据 SMCP 协议的设计，房间号也是AgentID，而Agent在SMCP_NAMESPACE仅可以同时存在于单一房间。
    # 因此可以直接用office_id获取rooms
    # Here office_id is the room number, but according to SMCP protocol design, room number is also AgentID,
    # and Agent can only exist in a single room in SMCP_NAMESPACE. Therefore, office_id can be used directly to get rooms
    # 排除OFFICE_ID实际上就是排除Agent，进而获取到的是Computers
    # Excluding OFFICE_ID actually excludes Agent, thus getting Computers
    for sid, _eio_sid in sio.manager.get_participants(SMCP_NAMESPACE, office_id):
        if sid != office_id:  # 排除Agent自身 / Exclude Agent itself
            try:
                session = await sio.get_session(sid, namespace=SMCP_NAMESPACE)
                if session.get("role") == "computer":
                    computer_session = TypeAdapter(ComputerSession).validate_python(session)
                    computers.append(computer_session)
            except Exception:
                # 忽略无效的会话 / Ignore invalid sessions
                continue

    return computers


def get_computers_in_office(office_id: OFFICE_ID, sio: Server) -> list[ComputerSession]:
    """
    从sio的 /smcp 命名空间中获取Computer所在房间的计算机列表（同步版本）
    Get list of computers in the room from the /smcp namespace of sio (synchronous version)

    Args:
        office_id (OFFICE_ID): 房间号 / Room ID
        sio (Server): SocketIO实例 / SocketIO instance

    Returns:
        list[ComputerSession]: 计算机列表 / Computer list
    """
    computers = []
    # 这里office_id是房间号，但根据 SMCP 协议的设计，房间号也是AgentID，而Agent在SMCP_NAMESPACE仅可以同时存在于单一房间。
    # 因此可以直接用office_id获取rooms
    # Here office_id is the room number, but according to SMCP protocol design, room number is also AgentID,
    # and Agent can only exist in a single room in SMCP_NAMESPACE. Therefore, office_id can be used directly to get rooms
    # 排除OFFICE_ID实际上就是排除Agent，进而获取到的是Computers
    # Excluding OFFICE_ID actually excludes Agent, thus getting Computers
    for sid, _eio_sid in sio.manager.get_participants(SMCP_NAMESPACE, office_id):
        if sid != office_id:  # 排除Agent自身 / Exclude Agent itself
            try:
                session = sio.get_session(sid, namespace=SMCP_NAMESPACE)
                if session.get("role") == "computer":
                    computer_session = TypeAdapter(ComputerSession).validate_python(session)
                    computers.append(computer_session)
            except Exception:
                # 忽略无效的会话 / Ignore invalid sessions
                continue

    return computers


async def aget_all_sessions_in_office(office_id: OFFICE_ID, sio: AsyncServer) -> list[dict]:
    """
    获取房间内所有会话信息
    Get all session information in the room

    Args:
        office_id (OFFICE_ID): 房间号 / Room ID
        sio (AsyncServer): SocketIO实例 / SocketIO instance

    Returns:
        list[dict]: 所有会话列表 / All session list
    """
    sessions = []
    for sid, _eio_sid in sio.manager.get_participants(SMCP_NAMESPACE, office_id):
        try:
            session = await sio.get_session(sid, namespace=SMCP_NAMESPACE)
            if session:
                sessions.append(session)
        except Exception:
            # 忽略无效的会话 / Ignore invalid sessions
            continue

    return sessions


def get_all_sessions_in_office(office_id: OFFICE_ID, sio: Server) -> list[dict]:
    """
    获取房间内所有会话信息（同步版本）
    Get all session information in the room (synchronous version)

    Args:
        office_id (OFFICE_ID): 房间号 / Room ID
        sio (Server): SocketIO实例 / SocketIO instance

    Returns:
        list[dict]: 所有会话列表 / All session list
    """
    sessions = []
    for sid, _eio_sid in sio.manager.get_participants(SMCP_NAMESPACE, office_id):
        try:
            session = sio.get_session(sid, namespace=SMCP_NAMESPACE)
            if session:
                sessions.append(session)
        except Exception:
            # 忽略无效的会话 / Ignore invalid sessions
            continue

    return sessions
