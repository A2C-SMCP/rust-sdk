"""
* 文件名: types
* 作者: JQQ
* 创建日期: 2025/9/29
* 最后修改日期: 2025/9/29
* 版权: 2023 JQQ. All rights reserved.
* 依赖: None
* 描述: Server端类型定义 / Server-side type definitions
"""

from typing import Literal, TypeAlias

from typing_extensions import TypedDict

# 类型别名定义 / Type aliases
OFFICE_ID: TypeAlias = str
SID: TypeAlias = str


class BaseSession(TypedDict):
    """
    公共会话基类，包含sid和name属性
    Base session class, includes sid and name attributes
    """

    sid: str  # 会话ID / Session ID
    name: str  # 会话名称 / Session name


class ComputerSession(BaseSession):
    """
    Computer会话类型
    Computer session type
    """

    role: Literal["computer"]
    office_id: str


class AgentSession(BaseSession):
    """
    Agent会话类型
    Agent session type
    """

    role: Literal["agent"]
    office_id: str


# 联合类型定义 / Union type definition
Session = ComputerSession | AgentSession
