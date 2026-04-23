# Changelog

All notable changes to this project will be documented in this file.

## [0.1.15] - 2026-04-23

### Features

- *(handshake)* [**breaking**] Make Socket.IO auth header and namespace configurable

## [0.1.14] - 2026-04-08

### Bug Fixes

- *(computer)* Default stdio child cwd to ~/.a2c-smcp when unset (#11)

### Documentation

- Update CHANGELOG for v0.1.14

### Features

- *(skills)* Add rust-learn skill and enhance fix-issue with online issue reply

### Miscellaneous Tasks

- Add claude code plugin settings
- Release v0.1.14

## [0.1.13] - 2026-03-27

### Bug Fixes

- *(ci)* Increase crates.io index wait time and tolerate already-published crates
- *(ci)* Replace fixed sleep with polling for crates.io index readiness
- *(computer)* Drain child stderr pipe to prevent deadlock on heavy output

### Documentation

- Update CHANGELOG for v0.1.12
- Update CHANGELOG for v0.1.13

### Miscellaneous Tasks

- Release v0.1.13

## [0.1.12] - 2026-03-18

### Bug Fixes

- *(computer)* Wire up handle_get_desktop_with_ack to return actual window data

### Miscellaneous Tasks

- Release v0.1.12

## [0.1.11] - 2026-03-13

### Features

- *(computer)* Support custom HTTP headers in SmcpComputerClient

### Miscellaneous Tasks

- Release v0.1.11

## [0.1.10] - 2026-03-11

### Features

- *(computer)* Add window resource aggregation and detail retrieval

### Miscellaneous Tasks

- Release v0.1.10

## [0.1.9] - 2026-03-06

### Bug Fixes

- *(computer)* Add connect timeout to StdioMCPClient and fix echo server framing

### Miscellaneous Tasks

- Release v0.1.9

## [0.1.8] - 2026-03-05

### Miscellaneous Tasks

- Add code-review skill
- Release v0.1.8

### Refactor

- *(computer)* 委托 rmcp SDK 重构 + code review 修复

## [0.1.7] - 2026-03-04

### Bug Fixes

- *(stdio)* 捕获子进程 stderr 输出用于诊断启动失败

### Miscellaneous Tasks

- Release v0.1.7

### Styling

- 修复 cargo fmt 格式化

## [0.1.6] - 2026-03-04

### Bug Fixes

- *(computer)* Tool→SMCPTool 转换时保留 meta 和 annotations

### Miscellaneous Tasks

- Release v0.1.6

## [0.1.5] - 2026-03-04

### Bug Fixes

- *(computer)* List_available_tools() 合并 tool_meta 配置

### Miscellaneous Tasks

- Release v0.1.5

## [0.1.4] - 2026-03-04

### Bug Fixes

- *(sse-client)* 完善 SSE 客户端错误处理并升级 eventsource-client

### Miscellaneous Tasks

- Release v0.1.4

### Styling

- 修复格式化问题

## [0.1.3] - 2026-03-03

### Bug Fixes

- *(mcp-clients)* 修复 notification 消息携带 id 及 params 为 None 的问题

### Miscellaneous Tasks

- Release v0.1.3

### Styling

- 修复格式化问题

## [0.1.2] - 2026-03-03

### Bug Fixes

- *(ci)* 调整 crates.io 发布顺序，smcp-server-core 前置

### Miscellaneous Tasks

- Release v0.1.2

## [0.1.1] - 2026-03-03

### Bug Fixes

- 修复 clippy unwrap_used 警告
- CI 配置排除 e2e feature
- 增加测试超时时间以适应 CI 环境
- 修复 CI 环境中 Socket.IO 集成测试超时问题

### Features

- *(smcp-computer)* 默认启用 vrl，并支持对工具调用结果做 vrl 转换
- *(smcp-computer)* CLI 支持运行时配置并增强 Socket.IO 状态展示
- 实现完整的订阅管理和资源缓存系统
- 实现完整的订阅管理和资源缓存系统
- 添加E2E测试服务器组件依赖
- 完成生产就绪优化 (P0/P1)
- *(computer)* 改进 HTTP/SSE MCP 客户端协议兼容性

### Miscellaneous Tasks

- 添加 crates.io 发布元数据
- Release v0.1.1
- 更新 release skill 和 cargo-release 配置
- Release v0.1.1
- 补充 e2e 测试 dev-dependencies，清理空行

### Refactor

- 移除 examples/python，添加协议规范引用
- 迁移至 tf-rust-socketio v0.7.0
- 统一 workspace 版本管理，引入 cargo-release 和 git-cliff

### Styling

- 修复代码格式化

### Ci

- 添加 GitHub Actions 流水线配置

