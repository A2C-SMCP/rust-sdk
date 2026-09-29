# PR #228：旧连接退役修复验证

审查项 B1：替换已经进入自动重连的 Agent 连接后，旧连接仍可能重新连回旧端点。

## 根因与修复

`tf-rust-socketio 0.9.2` 的异步 reader 是 detached task；`disconnect()` 设置 Manual 后，已经进入的重试循环、退避等待和握手不会停止。此外，发送 namespace DISCONNECT 失败会提前返回，跳过后续清理。

修复位于上游 [tf-rust-socketio 提交 6be42eb](https://github.com/A2C-SMCP/tf-rust-socketio/commit/6be42eb85c0f9d99618d30874b2393059649687b)，分支 `fix/async-disconnect-cancels-reconnect`，基于 `e7e08b352f1d82c9c4443a414c6ac2a4593e3ae7`；本地工作树为 `tf-rust-socketio-retirement`：

- Client 的所有 clone 共享 reader 句柄；disconnect 取消并等待 reader 结束，再关闭其最终持有的 socket，防止重连在关闭后安装新连接。
- 清理由独立任务持有，并串行处理重复断开；业务回调被取消、调用方放弃等待都不会中断已启动的清理。
- namespace 关闭帧发送失败仍执行 Engine.IO 清理；两层 socket 即使发送失败也撤销本地 connected 状态。
- 已关闭连接上的重复 disconnect 成功返回；其他传输错误在清理后返回。

SDK 新增真实 TCP 故障回归：旧端点拒绝连接，确认旧连接已发起重连后切换端点，再恢复旧端点，观察 8 秒确保旧连接不会复活。Computer 原有测试改为断言最终只有新连接且不再出现旧认证，不再要求底层产生一次迟到 CONNECT。

Computer 集成复验发现：关闭帧失败时，原有清理逻辑会把已经终止的 client 留在槽内，CLI 换名失败后 `has_socketio_client()` 仍为真。现将安装替换、显式断开和 shutdown 的退役槽清理合流：disconnect 完成后无论发送是否成功都解除租约并清槽，同时照常返回发送错误。调用方在 await 期间取消时仍保留槽供后续清理；已有取消测试继续覆盖这一边界。

## 验证方式

SDK 基线为 `b503afee9660e624cf2a7bb363442e69dfb9aa4d`。首批 4 项底层回归（重连回调、退避、HTTP 握手、正常重连后的重复断开）及 SDK 新增端点替换用例，均先观察原版失败，再验证修复版通过。另补充 2 项调用方取消相关用例和 1 项 CLOSE 发送失败用例。

本地 SDK 验证通过 Cargo 命令行同时覆盖两个依赖，不把绝对路径写入项目配置：

```sh
export CARGO_TARGET_DIR=/tmp/pr228-sdk-target
export CARGO_PROFILE_DEV_DEBUG=0
cargo \
  --config 'patch.crates-io.tf-rust-socketio.path="../tf-rust-socketio-retirement/socketio"' \
  --config 'patch.crates-io.tf-rust-engineio.path="../tf-rust-socketio-retirement/engineio"' \
  test -p smcp-agent -p smcp-client-transport --all-features -- --test-threads=1
cargo \
  --config 'patch.crates-io.tf-rust-socketio.path="../tf-rust-socketio-retirement/socketio"' \
  --config 'patch.crates-io.tf-rust-engineio.path="../tf-rust-socketio-retirement/engineio"' \
  test -p smcp-computer --features cli \
  --test auth_dict_injection_test --test cli_socket_join_test
```

独立构建目录用于避开原 target 下近百万文件造成的 macOS 目录枚举开销（#229）；没有清理原缓存。

## 已完成的验证

| 范围 | 结果 |
| --- | --- |
| 原版 SDK 的新增端点替换用例 | 按预期失败：`retired transport reconnected to the old endpoint` |
| 底层新增生命周期用例 | 7/7 通过：取消重连回调、退避、HTTP 握手；正常重连；回调内断开；取消断开调用方及重复断开；CLOSE 发送失败仍撤销克隆的 connected 状态 |
| Engine.IO 全部单测 | 46/46 通过 |
| Agent + 共享传输层测试 | 202 通过、1 个已有用例忽略；含 Office 生命周期 20/20 |
| Computer auth 生命周期集成测试 | 20/20 通过 |
| CLI 换名/真实 REPL 集成测试 | 串行复验 5/5 通过 |
| Computer Socket.IO 相关单测（`--lib socketio_`） | 37/37 通过，含槽位事务错误清理测试 |
| Socket.IO 既有单测 | 54 通过、1 失败；新增 7 项另外执行并通过 |
| 两仓格式与 diff 空白检查 | 通过 |
| 上游 CI 同等 Clippy（workspace + all-features，`-D warnings`） | 通过 |
| 底层新增集成测试的 Clippy（`--test disconnect_reconnect`，`-D warnings`） | 通过 |
| SDK 定向 Clippy（Agent Office、Computer auth/CLI 三个测试目标，`-D warnings`） | 通过 |

Agent Office 测试初次并行执行时同步门面用例失败，单独复验及整组串行执行均通过；未修改该用例或为通过而放宽断言。其余 Agent / 共享传输测试在初次命令中通过，因 Cargo 遇到 Office 失败后提前停止，后续 6 个已构建测试程序另行执行并全部通过。

Computer 首轮 CLI 的关闭失败用例稳定失败，促成上述槽清理修复。修复后该用例通过；该轮并行执行的真实 REPL 用例发生一次 ACK 超时，随后完整 5 项 CLI 用例串行复验全部通过，没有放宽超时或断言。

Socket.IO 的 `socket_io_long_callback_keeps_heartbeat_alive` 在修复版两次失败；复制相同依赖锁和测试依赖到未修改的 `e7e08b3` 基线后，该用例也失败（基线在存活断言、修复版在后续 echo 断言）。本机不能宣称上游全量回归通过，此异常独立保留，不修改心跳参数或断言来掩盖。

上游全目标 Clippy 被未修改的示例/单测告警阻断，包括 `unused_unit`、`io_other_error`、`module_inception`、`unnecessary_literal_unwrap`、`useless_vec`。须区分这些扩大检查的结果与库及本次新增测试的定向检查。

TLS 初次回归的 8 项失败来自临时测试证书缺少合适的叶证书扩展。补齐 SAN、KeyUsage、serverAuth 后 46 项全通过；没有修改系统信任库或产品 TLS 校验。

补充 CLOSE 发送失败用例时，共用 target 的基线构建导致旧 Engine.IO 产物被复用，首跑状态断言失败。未修改修复代码，仅触发两个库重新编译后 7/7 通过。后续基线对照应使用独立 target；SDK 的验证一直使用独立目录。

本次静态复核覆盖 reader 所有权、socket 替换顺序、锁顺序、回调自取消、重复断开和失败返回路径；未发现新增阻塞项。B1 的代码修复成立，正式集成仍受下述交付边界约束。

## 交付边界

本次交付包含两仓的代码与回归测试提交，尚未发布上游补丁。SDK 的正式依赖仍为 crates.io 0.9.2；必须先合入上游修复并发布补丁版本，再同步更新 SDK 的两个依赖及 Cargo.lock，才能解除 PR #228 的 B1 合并阻塞。不得把本地 path patch 的通过结果当作已发布版本修复。

## 评论 5868443500 的后续处理

- **B1（仍待正式接入）**：上游 [0.9.3 发布准备提交 e90544a](https://github.com/A2C-SMCP/tf-rust-socketio/commit/e90544a191a7bd45e2b1dda926a067300ac1949b) 已将两个包版本及 Socket.IO 对 Engine.IO 的最低版本提升到 0.9.3，并更新锁文件与发布说明。7 项底层回归和上游 CI 同等 Clippy 通过；Engine.IO `cargo publish --dry-run` 完成打包及构建验证，未上传。Socket.IO 的注册表打包验证须等 Engine.IO 0.9.3 发布后完成，顺序与上游发布 workflow 一致。
- **W1（已补充）**：同步工作线程在服务端观察到重放请求后，于成功分支有界等待 `confirmed_office_id()`，同时断言 `JoinedOffice`，然后才执行退房。该用例在正式 0.9.2 依赖上通过，定向 Clippy 与格式检查通过；拒绝分支仍验证主动退房取消恢复。

使用本地 0.9.3 候选版覆盖两个依赖后，完整 Office 生命周期测试串行执行 20/20 通过，包括 W1 同步恢复断言及 B1 的旧端点不复活用例。验证后还原 SDK 锁文件中的临时 path 来源，正式依赖升级留到包发布后执行。

下一步需要公开发布授权：发布两个 0.9.3 包后，将 SDK 的两个最低依赖版本同步改为 0.9.3，生成注册表来源的 Cargo.lock，并移除本地覆盖复验。B1 在该步骤完成前保持阻塞，不能以候选版验证替代正式接入。


## 2026-09-29：完成度核对后的补充验证

本节记录后续工作，不覆盖上面的历史失败记录。SDK 基线为
`be5e11394470708a0b2c0d84e41d0ae45bf7e4cc`，上游工作树基线为
`e90544a191a7bd45e2b1dda926a067300ac1949b`。

隔离审查进一步发现：退役中的 polling POST 如果不返回，第二个 disconnect 会一直等待；
已经安装的 polling GET / WebSocket 还可能被共享流或 client 克隆保留。
新增真实 TCP 用例先复现 POST 阻塞，再修复发送取消、传输所有权撤销和共享流清理。
namespace DISCONNECT 与 Engine.IO CLOSE 分别最多等待一秒；这是关闭帧的最佳努力期限，
本地所有权在发送前撤销。调用方取消不取消后台清理，并发 disconnect 共享串行清理。
连接建立与退役的 connected 状态在同一所有权锁内提交，避免断开后重新标记为在线。

| 后续验证 | 结果 |
| --- | --- |
| 底层真实退役集成测试 | 10/10；新增卡住的关闭 POST、存活克隆下 polling GET 和 WebSocket 的 TCP 关闭 |
| Engine.IO 库测试（真实 Node fixtures） | 46/46 |
| Socket.IO 库测试（真实 Node fixtures） | 55/55，本轮包含此前失败的长回调心跳用例；不能据此抹去历史失败 |
| 上游 workspace all-features Clippy | 零告警 |
| Engine.IO 0.9.3 publish dry-run | 打包与构建验证通过，未上传 |

SDK 同期完成 #223 的会话身份与 flat 403 补齐、#227 的原始工具观测缓存、#230 的待补发更新，
以及 #229 的默认 macOS Cargo/nextest runner。真实 MCP 进程验证 N=6/20 启动分别只读 6/20 次工具列表；
显式强制刷新仍逐 bundle 重读。macOS runner 已覆盖参数、环境、cwd、非零退出、信号转发、
子进程 SIGKILL 与临时目录清理，并在真实 Cargo 和 nextest 入口验证。

正式发布边界不变：本节的 0.9.3 是本地候选。只有注册表中两个 0.9.3 包正式可用、SDK 清除 path 覆盖并
升级两个最低版本及锁文件、正式依赖回归通过后，才可解除 #219/#224 与 PR #228 的依赖阻塞。


### SDK 候选集成与 UAT 结果

本轮 `cargo test --workspace --all-features --no-fail-fast -- --test-threads=1`
首轮为 **1935 passed / 4 failed / 37 ignored**，退出 101，不能表述为首轮全绿：

- Agent `connection_replacement_waits_for_in_flight_join` 在首次连接提交前失败，尚未进入替换步骤；
  同一二进制单独复验通过，完整 Office 生命周期串行复验 20/20 通过。未改断言或期限，保留间歇性结果。
- `resource_subscription_e2e` 两项在 `npx @playwright/mcp@latest` 初始化 30 秒期限内未完成，
  尚未执行资源断言；同一二进制原样整组复验 6/6 通过，未修改 Stdio 实现、测试或期限。
- `socketio_interop` 仍断言非 Agent 的 client:* 请求没有 ACK；这与本次 #223 的明确验收契约冲突。
  已将该断言改为精确比较 ACK 参数数组内的 flat 403（规定 message、无 details），保持真实协议路径。

Computer 全功能单测 1139/1139、auth 生命周期 24/24（含取消入房后的补发）通过。
显式执行默认忽略的真实 MCP 通知套件 3/3 和协议矩阵 16/16 通过；它们单独记账，不将其它忽略项算通过。
真实 MCP 本轮 N=6/20 分别产生 6/20 次 tools/list，强制刷新和停止行为也通过计数断言。

UAT / seed 影响评估：本次涉及 full-protocol 的成员生命周期、工具读取与取消广播；
新补发和身份边界由真实 Socket.IO 集成测试覆盖，现有 skill 包格式和 seed 无变化。
`valid-skill-pkg/acceptance.md` 的包体校验当次通过。
使用本轮候选构建的 Server + Computer + Agent 三个真实进程，运行既有
`full-protocol-uat.sh` 的临时副本（仅将 ROOT 与二进制路径指向工作区和隔离构建目录），
10 个 Agent mode、F-05 版本拒绝检查均通过。skill/config 目录使用临时隔离路径并清理，
没有改动 seed 或放宽 UAT 预期。


最后复验与门禁：`socketio_interop` 修订后 5/5 通过；workspace 全 targets / 全 features
Clippy（`-D warnings`）通过；workspace 全 features rustdoc（`RUSTDOCFLAGS="-D warnings"`）通过。
最终完整 diff 隔离审查为 APPROVE，无代码阻塞；保留非阻塞测试建议：对单种通知发送失败、
旧发送成功不能清除新 revision/session 的 pending 增加受控竞态覆盖。
验证结束已恢复原始 registry 0.9.2 锁文件，没有把本地 path patch 纳入交付。
Cargo 全量测试已构建的三端二进制完成 UAT；后续重复的额外 dev 构建主动停止，不记作额外构建通过。
