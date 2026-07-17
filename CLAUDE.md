# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目环境常用命令

### 测试相关别名 / Test aliases

```bash
cargo test-ws          # 测试整个 workspace
cargo test-all         # 测试所有 features
cargo test-e2e         # 运行 e2e 测试（需要 --ignored 标志）
cargo test-computer    # 只测试 smcp-computer
cargo test-agent       # 只测试 smcp-agent
cargo test-server      # 只测试 smcp-server-core

# 运行单个测试
cargo test --package <crate-name> <test_name>

# 运行带输出的测试
cargo test -- --nocapture
```

### 代码质量别名 / Code quality aliases

```bash
cargo fmt-all           # 格式化所有代码
cargo clippy-workspace  # 严格 clippy 检查
cargo clippy-loose      # 宽松 clippy 检查

# rustdoc 零警告门禁（#146，与 CI 一致）
RUSTDOCFLAGS="-D warnings" cargo doc-check
```

### 构建

```bash
cargo build --workspace --all-features
cargo build --release --workspace --all-features
```

## 代码架构概览

这是一个**真实 workspace**（同时有 `[workspace]` 和 `[package]` 段）的 Rust 项目，实现了 A2C-SMCP 协议。

### Workspace 结构

```
rust-sdk/
├── src/              # 主包入口（基于 feature re-export）
├── tests/            # 跨 crate 集成测试
└── crates/
    ├── smcp/               # 核心协议类型定义
    ├── smcp-agent/         # Agent 实现（客户端）
    ├── smcp-computer/      # Computer 实现（MCP 服务器管理）
    ├── smcp-server-core/   # Server 核心逻辑
    └── smcp-server-hyper/  # Hyper HTTP 适配器
```

### 三大核心组件

1. **Agent (`smcp-agent`)**
    - AI 智能体客户端，连接到 SMCP Server
    - 调用 Computer 上的工具
    - 支持异步 (`AsyncSmcpAgent`) 和同步 (`SyncSmcpAgent`) API
    - 基于 `tf-rust-socketio` 实现客户端通信

2. **Computer (`smcp-computer`)**
    - 管理多个 MCP Servers (stdio/SSE/HTTP)
    - 提供桌面资源聚合（window://）
    - 处理来自 Agent 的工具调用请求
    - 内置 CLI 工具（基于 clap + expectrl）

3. **Server (`smcp-server-core` + `smcp-server-hyper`)**
    - Socket.IO 服务器（基于 `socketioxide`）
    - 会话管理、事件转发、广播通知
    - 支持 API Key 认证
    - HTTP 承载层可插拔（默认 Hyper，支持 Tower 兼容框架）

### 通信协议

- **传输层**: Socket.IO (namespace `/smcp`)
- **事件命名**:
    - `client:*`: Agent → Computer 请求（如 `client:tool_call`）
    - `server:*`: 客户端 → Server 管理（如 `server:join_office`）
    - `notify:*`: Server 广播状态变更
- **序列化**: JSON (serde_json)

### MCP 服务器管理

Computer 支持三种 MCP 传输方式：

- **stdio**: 子进程通信
- **SSE**: Server-Sent Events
- **HTTP**: 直接 HTTP 调用

工具注册与去重通过 `tool_registry` 实现，支持 `ToolMeta.alias` 解决名称冲突。

## 关键设计决策

1. **Socket.IO 紧绑定**: 使用 `socketioxide` (Server) 和 `tf-rust-socketio` (Client)
2. **HTTP 承载层可插拔**: 通过 Tower Layer/Service 模式，默认使用 Hyper
3. **仅支持 JSON**: 不支持二进制消息，未来通过独立通道支持资源流
4. **异步优先**: 基于 Tokio 运行时

## Features

- `agent`: 启用 smcp-agent
- `computer`: 启用 smcp-computer
- `server`: 启用 smcp-server-core + smcp-server-hyper
- `full`: 启用所有功能
- `e2e`: E2E 测试服务器组件依赖

## 依赖注意事项

- Socket.IO 客户端使用自研的 `tf-rust-socketio` (crates.io)，基于 `rust_socketio` 增加了 ACK 响应支持
- E2E 测试需要 `e2e` feature 才能运行（`--ignored`）

## 协议规范与参考实现

- **协议规范仓库**: [a2c-smcp-protocol](https://github.com/A2C-SMCP/a2c-smcp-protocol)（当前版本: 0.1.2-rc1）
- **Python 参考实现**: `/Users/jqq/A2C-SMCP/python-sdk`（已添加到工作空间）

### 核心模块对应关系

| Python 模块          | Rust 模块             | 说明                       |
|---------------------|----------------------|----------------------------|
| `a2c_smcp/smcp.py`  | `crates/smcp/`       | 协议定义（事件、数据结构） |
| `a2c_smcp/server/`  | `crates/smcp-server-core/` | Server 端实现        |
| `a2c_smcp/agent/`   | `crates/smcp-agent/` | Agent 客户端               |
| `a2c_smcp/computer/`| `crates/smcp-computer/` | Computer 端实现         |
