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
