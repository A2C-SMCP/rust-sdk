"""
全局 Console 工具。
- 统一提供一个可被全局引用的 rich.Console 实例。
- 支持在运行时切换 no_color 以适配非 TTY/IDE 控制台。
"""

from __future__ import annotations

from rich.console import Console

# 注意：不要在其他模块中使用 "from ... import console"，
# 应使用 "from a2c_smcp_cc.utils import console as console_util" 然后访问 console_util.console。
# 这样当我们在此模块内替换 console 实例时，引用方总能拿到最新对象。
console: Console = Console()


def set_no_color(flag: bool) -> None:
    """在运行时切换全局 Console 的 no_color 配置。

    为了确保各处引用到的对象保持一致，我们直接在本模块内重建并替换实例。
    调用方需通过模块属性访问：console_util.console，而不是将对象拷贝到局部变量。
    """
    global console
    console = Console(no_color=flag)
