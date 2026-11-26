# Changelog

All notable changes to this project will be documented in this file.

## [0.5.1] - 2026-11-26

### Bug Fixes

- *(computer)* #271 卸载/GC 不再删除共享 catalog 树，仅删独占外部 clone (#276)
- *(computer)* #273 scp-like 源展示恢复 host+path（#154 严格边界不变） (#277)
- *(computer)* [**breaking**] #275 跨 marketplace 同名 plugin 安装层唯一性 (#281)
- *(computer)* #279 严格展示函数对 opaque scheme 一律 fail-closed (#282)
- *(computer)* #274 enable 内容缺失 fail-fast + 回滚（∀ 声明态判据） (#283)
- *(computer)* #285 拒绝 hostless Git URL，防凭据 slug 进 marketplace 名称 (#287)
- *(computer)* #278 旧版本升级迁移文档 + install 失败恢复提示 (#288)
- *(computer)* #284 多 scope 卸载保留 retained 记录声明的 skills 与 servers (#291)
- *(computer)* #286 enable 失败回滚回收 server + remount existing-wins（§2.4/§2.5-1） (#290)
- *(computer)* #292 executor A+ 对齐：project readRoots 保留 + 目标容器畸形 fail-closed (#293)
- *(computer)* #294 reconcile 声明式换源拒绝而非 wipe+reclone（§5.14） (#296)
- *(computer)* #272 marketplace 注册名改用清单 name（方向 A） (#297)
- *(computer)* #295 boot recovery 声明面换源拒绝与结构化报告（§5.14） (#298)
- *(computer)* #267 marketplace 剩余身份守卫（refresh 改名 / 声明式 re-add / flag-policy 引用层） (#300)
- *(computer)* #267 v0.5.1 剩余小项（谓词统一 / 非法声明可观测 / CLI 测试 / 复审销项） (#302)
- *(computer)* #267 settings 残余：flag 层四点消费 + import 容器预检（#248/#292） (#301)
- *(computer)* #267 S4 MCP client teardown 有界化（stdio/HTTP close + SSE lifecycle） (#303)
- *(computer)* Protocol#78 数据向量 re-vendor（bundle 25 / canonical 56） (#304)
- *(computer)* #286 follow-up enable 失败回滚按 scope pre-state（protocol#83 裁决 C） (#306)

### Documentation

- *(release)* #267 补齐 v0.5.1 release notes 覆盖（9 项 + #286 占位） (#305)

### Features

- *(computer)* #289 marketplace 存量注册名迁移（migrate + journal/备份/回滚） (#299)

### Miscellaneous Tasks

- *(ci)* 固定 Rust 工具链到 1.99.1（避免 1.100 rustfmt 基线漂移） (#280)

## [0.5.0] - 2026-11-03

### Bug Fixes

- *(computer)* Cancel startup wait during shutdown
- Scope session names by office
- *(server)* Trust session room for leave_office
- Box room ack errors for clippy
- Harden stdio initialization cleanup
- *(computer)* 有界等待 stderr drain，去掉 stdio 测试的线程串行化
- *(ci)* 补齐 CI 门禁——rustdoc 私有链接 + hyper 集成测试的空 ack 断言
- *(agent,server,computer)* 落实 PR#225 架构复审（4 项 blocking + 10 项建议）
- *(server)* 无房 client:* 按协议回 flat 4103（Not In Room）
- *(agent)* 自动重连后恢复 Office 成员关系（#219）
- *(agent)* Close replaced connections and serialize office state publication
- *(agent)* Make connection cancellation and office recovery safe
- *(computer)* Honor CLI socket join identity with reconnect (#224)
- *(computer)* Clear retired socket slots after teardown errors
- Preserve Office updates and invalidate calls across reconnects (#231)
- *(computer)* [**breaking**] #234 自动回房 4101 有界退避重试（镜像 python#212） (#239)
- *(smcp)* #245 PR2a 遗留收口（installer raw 保真 + #248 flag 诊断） (#251)
- *(computer)* #256 拒绝集目录/landing 前缀大小写折叠（protocol#76） (#257)
- *(computer)* #254 MCP client teardown 韧性（S1 manager 诚实性 + S2 SSE 任务所有权） (#261)
- *(computer)* #248 复审收尾——flag/mcp-config 信任路径归一与 fail-closed 补齐 (#264)

### Documentation

- *(test)* Diagnose macOS artifact-directory timeout failures (#229)
- *(changelog)* V0.5.0 变更归入

### Features

- *(smcp-agent)* Add cancellable tool calls
- *(computer)* Configure stdio initialization timeout
- *(computer)* Add stdio initialization diagnostics (#194)
- *(smcp-server-core)* Isolate office rooms and validate acks
- Unify room ack and error contracts
- *(server,agent)* 房间 ack 契约闭环 — 落实 PR#225 复审 blocking（#226）
- Complete session contracts and incremental update recovery
- *(room)* [**breaking**] 一房至多一台 Computer——每 role 一席 / 4101 泛化 / 4105 收回为预留码（#232）(#233)
- *(smcp)* #241 PR1 computer_id 类型与 Agent 编译面 (#243)
- *(computer)* #241 PR2 Computer 身份持久化与入房携带 (#244)
- *(smcp)* #245 PR1 client:resolve_path 事件管道与 4020 (#249)
- *(server)* #241 PR3 computer_id Server 强制校验、回显与场景 #10–#21 (#246)
- *(smcp)* #245 PR2a canonical_path 规范化 + readRoots 配置/围栏基础 (#247)
- *(computer)* #245 PR2b path 句柄、九步 resolve 管线与 client:resolve_path（protocol#65） (#250)
- *(computer)* #241 PR4 身份重置 SDK/CLI 与崩溃窗口恢复 (#252)
- *(computer)* #248 flag scope 最小 seam（运行期 readRoots/landingRoot + 撤 #251 诊断） (#260)
- *(computer)* #253 settings 写入 strict raw RMW 与 corrupt fail-closed（PR-A） (#262)

### Miscellaneous Tasks

- *(computer)* #240 PR-A 注释订正与回房测试判别修正 (#242)
- *(ci)* 发版前置——clippy all-features 门禁修复 + publish 失败不再被吞 (#258)
- *(ci)* 清零 clippy 基线红，cargo clippy-workspace 全绿 (#265)
- Release v0.5.0

### Testing

- *(agent)* Assert synchronous office recovery before leaving
- *(computer)* #235 auth_error 4006 三景补传输级 isError 断言（全字段口径）
- *(computer)* #237 env_segment 向量溯源改为协议 vendored 副本
- *(computer)* #245 PR3 resolve_path 集成、真实链路与 python 对拍 (#255)
- *(computer)* #240 PR-B 迟到 ACK 分类单测与 CLI 取消提示 outcome (#259)
- *(computer)* #254 补复审测试缺口（强制断开回退 / 请求饥饿 / lifecycle POST 序列） (#263)
- *(computer)* #253 A+ 跟进——文档/告警可见性与测试补强 (#266)

### Build

- Integrate released transport 0.9.3

### Ci

- Serialize stdio integration tests

## [0.4.1] - 2026-08-28

### Bug Fixes

- *(computer)* Address Socket.IO review findings (#206)
- *(computer)* Upgrade Socket.IO dispatch for long tool calls (#208) (#209)
- *(computer)* #211 旧 transport 迟到 Close 不再清除新会话 membership
- *(computer)* #208 长调用期间 cancel ACK 即时可达——best-effort 剥离 MCP cancelled 通知
- *(tests)* #213 v022 e2e 矩阵适配 BundleID 工具名（exposed_tool 单一来源）

### Documentation

- *(tests)* #212 target 自动清理（launchd cargo-sweep）与 [profile.test] 落地注记
- *(changelog)* V0.4.1 变更归入（#214/#208/#211/#212/#213）

### Features

- *(computer)* #200 ToolMeta Server 声明 tags 三层合并（protocol#51 / PR#57）
- *(skill)* A2CSkillRef.tags 加性字段透传（#198，protocol#50/PR#55，镜像 python#198）
- *(blob)* Client:put_blob 上行写入通道（#195，protocol#12/PR#53，镜像 python#196）
- *(blob)* #195 审查下轮 3 条收口——put_blob 真 e2e + spawn_blocking + flag 分叉注记
- *(tests)* #212 本地全量测试分级提速——nextest 接入 + 测试剖面瘦身 + RA 锁隔离
- *(tests)* #212 补交 [profile.test] debug=line-tables-only（与 #211 提交拆分后补交）
- *(computer)* #214 Computer 级 MCP 有限并发启动——统一门控/批量 API/治理恢复/CLI flag

### Miscellaneous Tasks

- PROTOCOL_VERSION 同步 0.4.0（对齐 protocol develop 0.4.0-dev 周期，镜像 python f8e02ab）
- Restore CI baseline on develop (#210)
- Release v0.4.1

### Refactor

- *(tests)* #213 复审跟进——exposed_tool 直调生产命名函数 + 保留裸名就地注释

## [0.4.0-dev.0] - 2026-08-24

### Bug Fixes

- *(ci)* Publish 依赖序修正——smcp-server-hyper 前置 + wait-for-index 补 User-Agent
- *(ci)* Publish.yml 消除全部 annotations——checkout v5 + 清理 package 暂存区

### Documentation

- *(skill)* Release SKILL 补充发布验收与依赖序教训（v0.3.2 事故沉淀）

### Miscellaneous Tasks

- Release v0.4.0-dev.0

## [0.3.2] - 2026-08-24

### Bug Fixes

- Automate Atlassian OAuth UAT consent
- *(computer)* Address OAuth review findings
- *(release)* Keep workspace version at 0.3.1
- *(computer)* [**breaking**] Project-scope enabledPlugins bundled server 回落审批门 PENDING (#165, Option B)
- *(computer)* [**breaking**] 安全层 Gate 1-3 对 plugin baseline 强制生效 (#169)
- *(computer)* Resolve_target §5.1 步骤序死锁 — 多命中精确 bundle_id 匹配优先（Candidate B）
- *(computer)* Start/restart 对「已声明未挂载」目标增加生命周期守卫 (#175)
- *(computer)* 空 OAuth scope 配置采纳 PRM scopes_supported 进行初始授权 (#176)
- *(computer)* Settings show --scope policy 经 validate_settings 做字段校验 (#166)
- *(computer)* Complete auto OAuth startup in one call (#179)
- *(smcp-computer)* Decouple activation from connection (#182)
- *(smcp-computer)* Propagate OAuth capability revocation
- *(smcp-computer)* #188 staging 根归属——URI 前缀优先于 _meta.source，被覆盖资源不再当独立根物化
- *(smcp-computer)* #189 裸 Bearer challenge 准入放开——well-known 回退链 + RFC 8414 §3.3 issuer 一致性
- *(smcp-computer)* #189 审查跟进——直取形态 §3.3 覆盖 + 文档契约 + reset 行为锚定
- *(smcp-computer)* Publish dynamic tool changes
- *(agent,computer)* 锁跨无界 await 并发缺陷——transport 重入死锁 & manager 生命周期冻结
- *(computer)* Restore office membership after reconnect (#204)

### Features

- *(computer)* Add OAuth support with rmcp 2.2 (#157)
- *(computer)* Complete OAuth host integration (#158)
- *(computer)* Complete injectable OAuth credential store (#159)
- *(computer)* Finalize OAuth host integration contract (#160)
- *(computer)* Add OAuth resource and status events
- *(computer)* Add transactional OAuth cancellation (#170)
- *(computer)* Negotiate MCP OAuth automatically (#174)
- *(computer)* Negotiate MCP OAuth automatically + adopt PRM scopes (#174, #176)
- *(computer)* Make HTTP OAuth negotiation automatic-only (#180)
- *(computer)* Materialize MCP inputs at process start
- *(computer)* Prioritize client resolver over cache
- *(computer)* #161 结构化窗口枚举诊断 + 三传输 list_windows 能力门
- *(computer)* #162 结构化 Runtime diagnostics：键控可清除 + 单调 revision/事件
- *(computer)* Support dynamic Socket.IO auth provider
- *(computer)* #186 逐 MCP runtime 状态变更事件——投影单源、弱锚点断环、自死锁根治

### Miscellaneous Tasks

- Cargo fmt
- Ignore isolated target-*/ build dirs
- *(workspace)* Declare MSRV 1.86
- *(lock)* Refresh Cargo.lock against workspace manifests
- *(workspace)* MSRV resolver fallback 移至 .cargo/config.toml（RFC 3537）
- *(ci)* 新增 resolver config 回归守卫（#203）

### Styling

- *(smcp-computer)* Cargo fmt（#188 测试断言换行）

### Testing

- *(uat)* Add full-protocol acceptance coverage (#24)
- *(uat)* Keep Atlassian OAuth credentials in memory
- *(smcp-computer)* #188 staging 根归属复现测试——被根覆盖的资源不再当独立根物化（红灯）
- *(smcp-computer)* #188 审查跟进——钉住 mode 统一 / 独立坏根 ERROR / 前缀边界 / 无 source 子资源物化
- *(smcp-computer)* #189 裸 Bearer challenge 准入放开红灯——well-known 回退链 + issuer 一致性门

## [0.3.1] - 2026-07-27

### Bug Fixes

- *(computer)* List_available_tools 每 server 仅拉一次 tools/list (#91)
- *(computer)* 账本删除后从 installedPlugins 意图重建 bundled MCP server (#104)
- *(computer,agent)* #106 接通三传输 MCP 运行期变化通知全链路
- *(computer)* #117 BundleID fallback 摘要改用 raw 连接身份（对齐协议 #17）
- *(computer)* [**breaking**] #121 实例 config 上下文注入 + CRUD 改 bundle_id 寻址
- *(computer)* [**breaking**] #123 add_or_update_server 新 server 默认落 local（不入 git）+ 显式 scope opt-in（协议#19 加固）
- *(computer)* #122 runtime-only 外部通道 rustdoc 前置条件 + 复活守护测试自足
- *(computer)* #125 补齐 available plugin 目录声明能力（DeclaredCapabilities）
- *(computer)* #126 同名用户 MCP 归属门控上移 Computer 层（enabled-gated）、config 层去插件感知
- *(computer)* [**breaking**] #127 SKILL mcp <server> 段改用 bundle_id + display-name-as-identity 全面扫漏
- *(computer)* [**breaking**] #131 删除 MCP 审批门档④（bundled 名免批准）—— 授权门绕过 + gate 签名收敛
- *(computer)* [**breaking**] #143 不受信 scope 不得自我批准 —— TRUSTED_SCOPE_ONLY_FIELDS（档④ 残留同构面收口）
- *(computer)* [**breaking**] #134 default_tool_meta.alias 塌名根治——alias 不从 default 继承 + 配置诊断
- *(computer)* [**breaking**] #138 snapshot.bundled 改 origin==Plugin 推导 + 删 bundled_mcp_server_names（F8 判据②）
- *(computer)* [**breaking**] #139 ledger 纯 bundle_id 数组 + 回收判据(b) + hook seam 收 bundle_id（D3/D5/F1·Discussion#32）
- *(computer)* [**breaking**] #141 库层 API 收 bundle_id + CLI resolve_target 按协议 §5.1（R4·Epic#129 收官）
- *(computer)* #150 401+WWW-Authenticate 保型穿过 MCPClientError 边界→结构化 downcast 产出 4006（AUTH-01 收口）
- *(computer)* #148 高层 disconnect/shutdown 真关 transport + MCP start/stop 同步 server:update_tool_list
- *(computer)* #144 boot_up 上抛 InputResolution(对齐 mount_server,非仅日志)
- *(computer)* #145 settings get 越权诊断——改 read_scope_with_errors + 删吞错误包装
- *(computer)* #128 损坏配置诊断穿出 snapshot——diagnostics 字段收口 MCP+settings 双吞错
- *(computer)* #151 part1 跨 scope 遮蔽非法声明诊断穿出
- *(computer)* #152 enabled Plugin bundled MCP 冷启动后恢复运行态
- *(computer)* AUTH-01 真实传输测试 flake 根治——删裸 403/401 marker + 收尾 fmt
- *(computer)* Redact marketplace git credentials (#156)
- *(computer)* Gate CLI-only scope parser

### Documentation

- *(computer)* #131/#130 隔离复审整改 —— 记录档⑤/⑥ 残留攻击面(#143) + 订正文档过度声称
- *(smcp)* #133 契约注释订正 —— mcp_server doc 从「名称」订正为 bundle_id（照注释传 display 名必 4014）
- *(workspace)* #146 rustdoc 清零——73 处 pre-existing 警告归零 + CI -D warnings 门禁
- *(changelog)* V0.3.0 变更日志

### Features

- *(computer)* 暴露 MCP server ownership + active-inventory 查询 API (#97)
- *(computer)* 治理恢复同构对齐 python-sdk#117——CLI 重挂接线 + remount inputs 注入/同名跳过 (#100)
- *(computer)* [**breaking**] V0.3.0 plugin install/enable 生命周期分离 (#102)
- *(protocol)* [**breaking**] PROTOCOL_VERSION 0.2.0 → 0.3.0，握手测试解耦具体值
- *(computer)* #108 S1 ComputerConfig 快照 + provenance
- *(computer)* #109 S2 写目标消解器（对称纯函数，disable≠remove）
- *(computer)* #110 S3 Config CRUD + 写计划执行器
- *(computer)* #111 config validate/migrate/import/export（schema-only + 脱敏）
- *(computer)* [**breaking**] #112 S5 inputs 边界订正（D1 运行期 resolver 注入）
- *(computer)* #114 S7 ComputerStatusSnapshot + 分离 revision + 事件订阅
- *(computer)* #113 S6 runtime mutate 落盘接线（补 #96 洞）
- *(computer)* #115 S8 集成回归守护 + R1 enable/disable bump 对称化
- *(computer)* [**breaking**] #117 BundleID 模型统一 MCP tool 命名与路由（协议 0.3.0）
- *(computer)* [**breaking**] #118 server-facing wire 身份 flip 到 bundle_id（协议 #18）
- *(computer)* #124 高层 API-only Marketplace/plugin governance snapshot/inventory
- *(smcp)* [**breaking**] #136 SMCPTool.bundle_id + Agent get_computer_config（F2 servers=运行期活跃集）
- *(skills)* Upgrade-guidance —— 向下游兄弟生成升级指导报告（含首次真跑 golden 示例）
- *(computer)* [**breaking**] #137 R1/F6 scope 完整序 + origin=plugin 读侧投影 + flag 次高 + --mcp-config
- *(computer)* [**breaking**] #147 embed origin 运行期投影——宿主构造入参入权威声明集（S14/Discussion #32）
- *(computer)* [**breaking**] #140 input env 名 ENV_SEGMENT 单一权威 + A2C_INPUT_→A2C_SMCP_ 硬切（D2/F4-F5·PROTO-5）
- *(computer)* #151 part2 typed MCP import + 零写 preflight
- *(computer)* #153 携带 BundleId 的窗口元数据枚举 API
- *(computer)* #155 plugin input 按 §5.11 scoped→global 解析序

### Miscellaneous Tasks

- Bump version 0.2.4-dev.0 → 0.3.0-dev.0

### Refactor

- *(computer)* [**breaking**] 移除 workdir/workspace 概念——project/local 锚定进程 cwd (#98)
- *(smcp)* [**breaking**] #130 BundleId newtype（构造即校验）—— 消除 name/bundle_id 类型同型的结构性根因
- *(computer)* #132 卫生批次 —— 清除「名叫 server_name、值是 bundle_id」残留（归并 #119）
- *(computer)* #151 隔离复审 🟡 三项——${input:} 文法单一权威 + 原子性文档收窄 + facade 测试

### Styling

- *(computer)* Apply rustfmt

### Testing

- *(computer)* #143 隔离复审整改 —— 补 merge 交互守护 + 诊断回路闭合（settings show / boot 呈现）
- *(computer)* AUTH-01 真实传输 401/403 可达性判据（协议 Discussion #34）
- *(computer)* #142 R5/F7 收官守护——夹具取值分叉 + 异 id 同名寻址对拍 + 消三处假绿（Epic#129 收官）
- *(computer)* #149 共享 Streamable mock 去重 + auto_reconnect 分支真覆盖

## [0.2.4-dev.0] - 2026-07-01

### Bug Fixes

- *(computer)* Tool_call ack 结果级 _meta 重映射为协议规范 meta（跨 SDK 互通，对齐 protocol#5）

### Features

- *(computer)* Computer 级 marketplace/plugin lifecycle API + CLI 迁移 (#94)
- *(computer)* Computer::reconcile_governance() + boot 启动恢复 (#95)

### Miscellaneous Tasks

- *(version)* Workspace bump 0.2.3 → 0.2.4-dev.0（预发布内测版）

### Testing

- *(computer)* 补 #93 治理层（#94/#95）代码审查缺口 + CLI JSON 键名 parity

## [0.2.3] - 2026-06-18

### Bug Fixes

- *(computer)* Forbidden_tools 跨 server 暴露面对账，禁用先于 alias/冲突收集（对标 Python #106/#107）
- *(skills)* SKILL watcher 回调 marshal 回 Tokio 运行时再 mark_dirty（修热重载 panic）

### Documentation

- *(changelog)* 更新 v0.2.3 变更日志

### Features

- *(auth)* 全 SDK Socket.IO auth-dict-only 连接面鉴权（退役 HTTP-header 鉴权）

### Miscellaneous Tasks

- Release v0.2.3

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

