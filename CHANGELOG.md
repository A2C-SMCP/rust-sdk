# Changelog

All notable changes to this project will be documented in this file.

## [0.2.3] - 2026-06-18

### Bug Fixes

- *(computer)* Forbidden_tools 跨 server 暴露面对账，禁用先于 alias/冲突收集（对标 Python #106/#107）
- *(skills)* SKILL watcher 回调 marshal 回 Tokio 运行时再 mark_dirty（修热重载 panic）

### Features

- *(auth)* 全 SDK Socket.IO auth-dict-only 连接面鉴权（退役 HTTP-header 鉴权）

### Testing

- *(e2e)* 补 get_desktop 指定 window 过滤 + desktop_size 截断端到端测试

### Ci

- *(publish)* 装 libdbus-1-dev/pkg-config 供 Linux keyring 构建（解 v0.2.2 发布 validate 失败）
- *(publish)* 补发布 smcp-client-transport（v0.2.2 新增 crate #22，漏配致下游发布失败）
- *(test)* 装 libdbus-1-dev/pkg-config 供 Linux keyring(libdbus-sys) 构建

## [0.2.2] - 2026-06-13

### Bug Fixes

- *(smcp-server-core)* 恢复 handler.rs 误删的 serde_json::Value 导入
- *(smcp-computer)* Code-review 跟进——blob golden + keyring 缓存 + command parity + value 0600（#62 #65 #73）
- *(smcp-computer)* Code-review 跟进——bundled 空省略 + install skill 计数 + uninstall/disable doc（#69 #71）
- *(smcp-computer)* Code-review 跟进——boot blob init 失败隔离 + 测试 FS 隔离 + Clone watcher（#68）
- *(smcp-computer)* Fix-review 跟进——4015 结构化分流 + restage 持锁标注 + glue 测试（#68）
- *(smcp-computer)* Fix-review 跟进——空 annotations 折叠 None + last_modified 测试覆盖（#30）
- *(smcp)* SKILL/blob 文本 MIME 判定收敛 §6.4 单一权威 smcp::utils::mime（#81）
- *(smcp-agent)* 拆 socket.io ack 外层 args 数组，修高层查询方法端到端 "Missing req_id"（#82）
- *(smcp-computer)* Run 补 boot_up()，CLI 部署模式装配 SKILL/blob/watcher 子系统（#83）

### Features

- *(smcp)* 协议版本常量 + 全量错误码对齐 (0.2.x)
- *(smcp)* [**breaking**] Flat ErrorPayload 重构，移除嵌套 ErrorResponse (#33)
- *(smcp)* [**breaking**] Flat ErrorPayload 重构，移除嵌套 ErrorResponse (#33)
- *(smcp)* Protocol-foundation 根节点 #45/#42/#39/#37/#60 一并落地
- *(handshake)* HS-01 服务端版本握手中间件 + UTIL-01 握手工具 + 4008 ErrorPayload（#21 #58）
- *(handshake)* HS-02 客户端版本握手 + 升级 tf-rust-socketio 0.8.1（#22）
- *(handshake)* SMCP-08 SessionInfo.a2c_version 落库 + list_room 透出（#15）
- *(smcp)* SMCP-02 get_resources + SMCP-04 SKILL 数据结构与事件常量（#26 #35）
- *(smcp)* UTIL-02 drain_blob 通用二进制拉取例程（#59）
- *(smcp)* ErrorPayload 4014/4015 顶层字段 + from_error_code + 未知字段兜底捕获
- *(smcp-computer)* SET-01 治理层 settings schema 与字段级容错校验（#61）
- *(smcp-server-core)* SRV-01 统一 relay_client_call + flat ErrorPayload ack（#47）
- *(smcp-agent)* AGT-01 协议错误解析 + 请求构造器（#34）
- *(smcp-computer)* SET-02 settings 5 级 scope 合并引擎 + DELETE sentinel（#63）
- *(smcp-server-core)* SRV-04 在途断连容错 + 隔离硬化（#56）
- *(smcp-agent)* AGT-02 SKILL 消费 API + notify 刷新（#36）
- *(smcp-computer)* SKL-01 SKILL naming 模块——桥接协议 lexer + source→name 合成链（#40）
- *(smcp-computer)* SKL-03 SKILL home + sources 解析（#46）
- *(smcp-computer)* SKL-02 SKILL registry 状态机 + 共享 frontmatter（#43）
- *(smcp-computer)* SKL-05 SKILL sandbox + resource view + manifest 全量（#52）
- *(smcp-computer)* SKL-04 SKILL staging 三源物化 + 安全解包 + git（#49）
- *(smcp-computer)* SKL-06 SKILL watcher + debouncer（#55）
- *(smcp-computer)* SKL-07 SKILL reconciler——additive 对账 + gc/prune + 孤儿（#57）
- *(smcp-computer)* SET-04 settings store 原子持久化 + 损坏恢复（#67）
- *(smcp-computer)* SET-03 settings policy 层 first-source-wins（#65）
- *(smcp-computer)* INP-01 inputs 解析链 + keyring/value/render（#73）
- *(smcp-computer)* BLB-01 blob_handle 无状态编解码（#62）
- *(smcp-computer)* BLB-02 blob resolver + thresholds（#64）
- *(smcp-computer)* BLB-03 toolspool blob store（#66）
- *(smcp-computer)* SET-05 plugin installer install/uninstall/enable/disable（#69）
- *(smcp-computer)* SET-06 MCP config 层 + 批准门控（#71）
- *(smcp-computer)* INT-01 Computer skill/blob 编排底座（#68）
- *(smcp-computer)* INT-04 mcp_clients 扩展——envFile/分页/skill 资源（#74）
- *(smcp-computer)* INT-01 收尾——get_resources/restage_mcp_skills/envFile 合并（#68）
- *(smcp-computer)* [**breaking**] WIN-02 organize_desktop 元数据下沉 + priority f32[0,1]（#18）
- *(smcp-computer)* [**breaking**] WIN-01 WindowURI 纯标识符化（移除 query 元数据）（#16）
- *(smcp-computer)* MCP-01 host 唯一性 SHOULD-WARN lint（聚合处非阻塞）（#20）
- *(smcp-computer)* RES-01 Computer on_get_resources 透明转发（4014/4015 flat ErrorPayload）（#30）
- *(smcp-agent)* RES-02 Agent get_resources 客户端方法（async+sync）（#31）
- *(smcp-computer)* AUTH-01 MCP 工具调用 4006/4007 授权透传 + auth_hint 脱敏（#23）
- *(smcp-computer)* INT-02 tool_call 取消最后一公里——可取消在途任务 + best-effort MCP notifications/cancelled（#70）
- *(smcp-agent)* AGT-05 tool_call_cancel 发送 + 取消/超时/失败三态区分 + in-flight 断连容错（#44）
- *(smcp-agent)* AGT-03/AGT-04 二进制旁路消费 — get_blob/drain + tool_call sideband（#38 #41）
- *(smcp-computer)* INT-03 socketio blob/skill/cancel 接线 + oversize 铸造（#72）
- *(smcp-computer)* CLI-01 治理层 marketplace + plugin 子命令（#48）
- *(smcp-computer)* CLI-02 治理层 settings + skill 子命令（#51）
- *(smcp-computer)* CLI-03 治理层 REPL UX + 全局 flag 透传（#54）
- *(smcp-server-core)* SRV-02 server:update_skills 广播 + 四 client:* SKILL 通道 handler（#50）
- *(smcp-computer)* INT-04 sse/http list_resources_page 补 4015 能力预检（对齐 Python/stdio）（#78）

### Miscellaneous Tasks

- *(deps)* Bump libredox 0.1.15 → 0.1.17 in Cargo.lock
- Release v0.2.2
- Merge develop-v0.2.2 for v0.2.2 release

### Performance

- *(smcp-computer)* INT stage_mcp_skills 两阶段化——写锁不跨 materialize 网络 fetch（#77）

### Refactor

- *(handshake)* 抽取 smcp-client-transport 共享 crate + 修复 fetch_4008 Ok 臂泄漏（fix-review f3a75bd）
- *(smcp-agent)* Fix-review 跟进——接入 builders/protocol_error + payload 保留（#34）
- *(smcp-computer)* SKL-03 fix-review 跟进——相对 override 锚定 CWD + 协议引用消歧 + ref 注入前瞻（#46）
- *(smcp-computer)* SKL fix-review 跟进——archive 流式下载+超时 + size-cap DRY（#49 #52）
- *(smcp)* Code-review 跟进——digest→hex + 切片边界下沉 smcp::utils 统一（#64 #66）
- *(smcp-computer)* Code-review 跟进——list_windows 过滤排序抽取共享 helper + 比较器单源化（#16 #18）
- *(smcp-computer,smcp-agent)* Fix-review 跟进——取消纵切 #70/#44 收尾（无阻塞项）

### Styling

- *(fmt)* Cargo fmt-all 收敛遗留格式（发布前 fmt-clean）

### Testing

- *(smcp)* 钉死 flat ErrorPayload 序列化 + 错误码命名空间文档
- *(smcp)* Fix-review 跟进——补 #42/#60 测试与契约 doc（来自 1c9db63）
- *(smcp)* Fix-review 跟进——drain_blob 并发分支补测 + sync fatal>recoverable 收紧 + max_retries 加固（#59）
- *(smcp-computer)* SKL-07 fix-review 跟进——补分支测试 + safe_rmtree 符号链接硬化（#57）
- *(smcp-computer)* Fix-review 跟进——mcp_config resolve 层补测 + wire 单源钉死 + 未知键宽容回归（#71）
- *(smcp-server-core)* SRV-03 取消广播 req_id 透传 + AgentCallData 无 computer 字段验收（#53）
- *(smcp-computer)* WIN 修复 desktop_integration 断言已移除的 window URI query 元数据（#79）
- *(workspace)* REL-01 workspace v0.2.2 集成测试矩阵（真实 socket.io + stdio MCP，e2e gated）（#24）
- *(uat)* A2C-SMCP Rust SDK UAT 体系 + 三场景端到端迁移 + tmux 正式编排

### Merge

- *(smcp-03)* Flat ErrorPayload 重构（#33）

## [0.1.15] - 2026-04-23

### Bug Fixes

- *(clippy)* Replace sort_by with sort_by_key for clippy 1.95

### Documentation

- Update CHANGELOG for v0.1.15

### Features

- *(handshake)* [**breaking**] Make Socket.IO auth header and namespace configurable

### Miscellaneous Tasks

- Release v0.1.15

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

