# filename: logger.py
# @Time    : 2025/8/15 14:39
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
a2c_sMCP 日志模块
Logger module for a2c_sMCP project.

通过环境变量控制日志：
- A2C_SMCP_LOG_LEVEL: 控制日志等级（debug, info, warning, error, critical）
- A2C_SMCP_LOG_SILENT: 设为 1/true/yes 时禁用所有日志输出
- A2C_SMCP_LOG_FILE: 设置日志文件路径，启用文件输出
"""

import logging
import os
import sys
from pathlib import Path

# 直接从环境变量获取配置
LOG_LEVEL = os.environ.get("A2C_SMCP_LOG_LEVEL", "info").lower()
SILENT_MODE = os.environ.get("A2C_SMCP_LOG_SILENT", "0").lower() in ("1", "true", "yes")
LOG_FILE = os.environ.get("A2C_SMCP_LOG_FILE")

# 创建全局 logger 实例
logger = logging.getLogger("a2c_smcp")
logger.propagate = False  # 防止日志向上传播到根logger

if not SILENT_MODE:
    # 配置日志级别
    level_map = {
        "debug": logging.DEBUG,
        "info": logging.INFO,
        "warning": logging.WARNING,
        "error": logging.ERROR,
        "critical": logging.CRITICAL,
    }
    logger.setLevel(level_map.get(LOG_LEVEL, logging.INFO))

    # 创建通用日志格式
    formatter = logging.Formatter("%(asctime)s - %(name)s - %(levelname)s - %(message)s")

    # 配置控制台输出
    console_handler = logging.StreamHandler(sys.stdout)
    console_handler.setFormatter(formatter)
    logger.addHandler(console_handler)

    # 配置文件输出（如果指定了日志文件）
    if LOG_FILE:
        try:
            # 确保日志目录存在
            log_path = Path(LOG_FILE)
            log_path.parent.mkdir(parents=True, exist_ok=True)

            file_handler = logging.FileHandler(LOG_FILE)
            file_handler.setFormatter(formatter)
            logger.addHandler(file_handler)
        except Exception as e:
            logger.error(f"创建日志文件失败: {str(e)}. 回退到仅控制台输出")

    logger.info("日志系统已初始化 - 级别: %s, 文件: %s", LOG_LEVEL.upper(), LOG_FILE if LOG_FILE else "N/A")

else:
    # 静默模式：禁用日志
    logger.disabled = True
    logger.addHandler(logging.NullHandler())  # 防止无handler的警告
