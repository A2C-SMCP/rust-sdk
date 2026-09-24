# Agent 自动重连后的 Office 成员关系恢复（#219）

本文说明 `smcp-agent` 在**传输层自动重连**后如何恢复 Office（房间）成员关系：为什么必须由客户端
自己恢复、状态机长什么样、有界退避重试的口径与预算、以及部署方需要配合的协议 SHOULD 约束。

镜像来源：python-sdk#203（已合入）+ [a2c-smcp-protocol Discussion#61](https://github.com/A2C-SMCP/a2c-smcp-protocol/discussions/61)
裁决（协议侧 [PR#62](https://github.com/A2C-SMCP/a2c-smcp-protocol/pull/62)，规范文本已入 `develop`）。

## 1. 问题：房间成员关系属于**会话**，不属于「客户端进程」

Socket.IO 的 room 成员关系挂在**namespace 会话**上。传输层断线后底层自动重连会建立一个**新会话**
（新 SID），服务端侧旧会话被销毁时一并退出房间，而客户端本地若只保留 `office_id`，就会形成
**「看起来还在房间、实际收不到任何房间流量」**的静默失联：

- 该 Agent 发出的 `client:*` 会被服务端按「无房 / 跨房」拒绝（**响亮**失败）；
- 房间内的 `notify:*` 广播**不会**再送达该 Agent，且**不会自愈**（**静默**失败）。

## 2. 恢复模型：意图 + 世代 + 会话 epoch

`smcp-agent` 维护一份成员状态机（`crates/smcp-agent/src/office.rs`，与 Computer 侧 #204/#211
同构）：

| 字段 | 含义 |
|---|---|
| `desired` | **客户端表达的入房意图** `(office_id, agent_name)`：`join_office` 成功后落账，显式退房 / 服务端踢出 / 回房放弃即清除 |
| `confirmed` | **服务端已确认**的成员关系；断连或回房失败即清空 |
| `generation` | 世代号：Connect / Close / 显式 join / 显式 leave 均使其前进，作废在途回房结果 |
| `transport_epoch` | 当前连接已观察到的最大 namespace epoch（包括先到的 Close），同 epoch 的关闭不可逆 |
| `connected` | namespace 是否在册 |

`agent_name` 必须随意图一起记住：Agent 侧它只出现在 `join_office` 的**调用实参**里
（`auth_provider` 只提供 `office_id`），不记住就无法在重连后原样重放。

### 2.1 生命周期钩子

传输层生命周期经 [`TransportLifecycle`](../../crates/smcp-agent/src/transport.rs) 通道交给 Agent
（`Connected { epoch }` / `Closed { reason, epoch }`）。⚠️ 内核的 `on_any` **只**派发 `Message` /
`Custom` 事件，故生命周期必须注册在 `on(Event::Connect)` 与 `on_close_with_session` 上。

| 事件 | 成员状态的处置 |
|---|---|
| namespace `Connected` | 绑定 `transport_epoch`、`connected = true`、`confirmed = None`（**新会话未经确认**）；若意图仍在 ⇒ 派生 generation-bound 回房 task 重放 `server:join_office` |
| namespace `Closed`（`transport close`） | 作废在途回房；`connected = false`、`confirmed = None`；**保留意图**（底层会自动重连） |
| namespace `Closed`（`io server disconnect` / `io client disconnect`） | 同上，但**清空意图**——否则会把用户主动退出、或被服务端踢掉的房在下次重连时自动加回去 |
| 陈旧 Close（epoch 小于已观察到的 epoch） | 整个事件丢弃（#211：旧 transport 的迟到 Close 不得清掉新会话的成员关系） |
| 更新 epoch 的 Close 先于 Connect 到达 | 记录该 epoch 并置断开；迟到的同 epoch Connect 不得复活会话，只有更大的 epoch 才能重新建立 |
| 重复或陈旧 Connect | 忽略，不清空已确认成员关系，不产生重复回房 |

### 2.2 可观测状态

`AsyncSmcpAgent::office_membership()` / `SyncSmcpAgent::office_membership()` 返回：

| 状态 | 含义 |
|---|---|
| `Disconnected` | namespace 未在册（**优先于其余状态**：会话不在册就不存在「在房」这一事实，故即便有残留的确认也不报 `JoinedOffice`） |
| `Connected` | 已连接但**不在任何房**（含**回房失败后的回退态**） |
| `JoinedOffice { office_id }` | 服务端已确认在房**且**该会话仍在册 |

`confirmed_office_id()` 返回服务端**已确认**的房号，与 `auth_provider` 配置里的 `office_id`
（意图）区分。

> **会话校验（两个提交点共用）**：传输层回调与 ack 分发在内核里是**各自独立的 task、无执行顺序
> 契约**，故「请求已发出」与「收到成功 ack」之间可能夹进一次 Close / Connect。房间事件的 ack 因此
> 只在**发起时的会话仍是当前且在册的会话**（`connected` ∧ 世代未前进 ∧ 会话 epoch 未变）时才落账：
>
> - 自动回房：`commit_rejoin` 三重校验（世代 / 意图 / 在册），不在册即丢弃；
> - 显式 `join_office`：`commit_join` 按捕获的 `OfficeSessionToken` 校验；会话已更替即**整包丢弃**
>   （既不算已确认，也**不**记录意图——否则会把期间被显式退房清掉的意图复活成下一次重连的目标），
>   并返回 `Err`，由调用方自行重试。
>
> 这条防线保证「namespace 已断开却报 `JoinedOffice`」在构造上不可达。
>
> 与之配套，`AsyncSmcpAgent::connect()` 只在**首个 `Connected` 事件的状态绑定完成后**才返回——否则
> 调用方可能在绑定之前发起 `join_office`，使用尚未绑定的会话标识（既会误丢合法 ack，也可能在
> `connected == false` 时落账成员关系）。若 10s 内仍未观察到该事件，`connect()` **如实返回错误**。
>
> `connect()` 是**事务性**的：在满足上述后置条件之前，它不触碰任何既有状态（transport 槽位、生命周期
> / 通知两个后台 task 的句柄、成员状态），失败时停止本次后台任务并显式关闭本次新建连接——因此**对已连接的 agent 再次
> `connect()` 失败不会破坏既有连接**（既有连接仍可发房间请求，其生命周期事件仍被消费、仍会自动回房）。

**成员关系属于连接**：每次 `connect()` 分配一个连接序号，成员状态只接受**该序号与已提交连接一致**的
生命周期事件；未提交连接（`connect()` 尚未提交）与已被替换连接的迟到事件一律忽略。注意各连接的
`session_epoch` **各自从 1 起算**，故 epoch 不能单独标识连接——这正是需要连接序号的原因。

**提交协议（原子裁决）**：`connect()` 在起生命周期 task **之前**先认领本次尝试槽位（归属先行，使
「`Closed` 先于首个 `Connected` 到达」时断开记录有处安放）；随后生命周期 task 在**同一把锁**内记录
「已建立 / 已断开」，且「已断开」是**单调**的（内核 Connect / Close 回调投递顺序无契约，故不复活）。
`connect()` 的提交点是一次加锁的 `commit_attempt`：在同一临界区内裁决「提交前是否已断开」并完成连接
归属 + 会话绑定 + 世代推进；判为「提交前已断开」即丢弃本次资源并返回错误。于是「检查」与「提交」不再
分离——**已到达的 Close 不可能被提交点漏判**，且不会把死连接报成「已连接」。

**连接发布与清理**：所有 Agent 克隆共享连接尝试门与后台任务归属，避免同时覆盖尝试槽位。
未提交尝试由资源守卫持有：握手失败、namespace 超时、等待操作门时取消或调用方丢弃 future，
都撤销本次尝试记录并停止本次后台任务；已取得的 transport 交给独立任务完成异步关闭。
成功提交才移交资源，取消失败的新尝试不会释放既有连接。
新连接就绪后，提交身份与替换 transport 在房间操作门内完成，且两步之间没有 await；显式入退房
因此不会捕获新身份却使用旧连接。成功替换会停止旧任务并显式关闭旧 transport，再让回房执行。
仅 drop transport 不会终止底层持有 Client 克隆的轮询任务，不能作为连接清理手段。关闭旧连接也会
唤醒仍在该连接上等待 ACK 的调用，使其返回连接错误。

**房间操作全序**：显式 `join_office` / `leave_office` 与自动回房的**逐次尝试**共用同一把操作门；
恢复成功提交持门完成；恢复放弃也必须取得操作门并重新校验世代，不能作废正在等待 ACK 的显式入房。
成员丢失回调在释放操作门后派发，允许回调重入 `join_office` / `leave_office`。
`leave_office` 的「清意图 + 作废在途回房」也在门内执行，使 leave 与 join 全序化——否则并发退房会插进
在途 join 的「发请求 → 落账」之间，或造成「服务端已退房、本地仍报 `JoinedOffice`」（本地状态与服务端
事实相反）。

回房**最终失败**时状态回退到 `Connected`、清空意图与已确认，并派发
`AsyncAgentEventHandler::on_office_membership_lost(office_id, reason, agent)`（默认实现只记 error
日志）。**绝不静默假装仍在房。**

## 3. 有界退避重试

### 3.1 为什么必须重试，且只能是**客户端补偿**

静默断线（拔网线 / 代理被杀 / NAT 超时 / 合盖唤醒）时，服务端要等**自身心跳超时**才回收旧会话。
客户端按默认重连延迟（秒级）重连后重放入房，很可能撞上尚未回收的旧会话而被拒：

| 角色 | 拒绝码 |
|---|---|
| Agent | `4101 Room Full`（一房一 Agent） |
| Computer | `4105 Name Conflict`（房内同 role 同名唯一） |

协议**明确禁止服务端收编 / 驱逐旧会话**（room-model.md §静默断线与会话回收）：服务端仅凭
`(role, name)` 无法区分「本客户端的僵尸会话」与「另一真实同名客户端」，收编等价于「任何同名者可
驱逐合法成员」，还会与「房内同名唯一」互斥而无限互踢。

因此**不存在**「旧会话已回收」事件可供等待——唯一机器可判的信号就是这两个拒绝码本身，
补偿只能是客户端有界退避重试（协议 error-handling.md §建议的重试策略）。

### 3.2 口径

- **只在恢复路径上重试**：仅对传输层重连后自动重放的 `4101` / `4105` 退避重试；
  **首次入房撞上视为永久冲突，不重试**。
- **单次尝试是下限**：预算配 0 也只做一次尝试，绝不无界重试。
- **其它一律不重试**：`4106 Already In Room`（重连产生的是新会话，不可能「已在其它房」，只由
  客户端状态错误产生）、`400` / `403` / `4103` / `4104`、**未知码**、**未获裁决**（ack 形状不认识）
  与传输层错误——重试不改变结果。判定实现见 `office::classify_rejoin_error`。
- **结果受世代约束**：落账前复核 `(generation, intent, connected)`；被 Close / 显式操作接管的在途
  结果一律丢弃。Close / 显式退房 / 显式入房都会 abort 在途回房 task（不持续抢房）。

### 3.3 默认预算与退避曲线

| 配置 | 默认 | 说明 |
|---|---|---|
| `office_rejoin_timeout` | `10s` | 单次重放的 ack 等待上限（对齐 python `OFFICE_REJOIN_TIMEOUT` 与本仓 Computer 侧同值） |
| `office_rejoin_budget_secs` | `60s` | 有界退避重试的**总预算**，覆盖 socket.io 默认最长回收窗口 45s |

退避曲线（SDK 自治）：首次**立即**尝试，其后 `1s → 2s → 4s → 8s`，此后封顶 8s。

**预算约束的是「下一次尝试的实际起始时刻」**：退避前检查 `elapsed + delay ≤ budget`，
后续操作锁等待受剩余预算约束，取得锁后发送前再次检查时间。首次尝试仍至少一次。
无并发显式操作时，总耗时上界 ≈ `预算 + 单次 ack 等待`；若显式入房已在途，失败状态提交与
成员丢失通知须等该事务结束后再裁决，因此可能晚于预算，但不会在预算外发送额外的重试。
默认值下的尝试起点（ack 立即返回时）：`0 / 1 / 3 / 7 / 15 / 23 / 31 /
39 / 47 / 55s`——最后一次尝试起点仍不晚于预算、且已越过 45s 回收窗口；ack 慢时尝试次数自然更少，
实际最后一次起点取决于 ACK 耗时与锁竞争。资源开销：默认预算内最多约 10 次 `server:join_office` 往返
（秒级间隔），无额外轮询、无长连接以外的常驻资源。

### 3.4 部署约束（协议 SHOULD）

> 服务端传输层的 `ping_interval + ping_timeout`（即最长回收窗口）**SHOULD 与客户端可接受的恢复
> 时延相称**。

socket.io 默认 `ping_interval(25) + ping_timeout(20)` ⇒ 最长 45s。**任何有限短窗都覆盖不了回收
窗口**——若部署方把回收窗口配得远大于客户端的恢复预算，补偿重试必然徒劳（客户端只能在预算耗尽后
如实报错）。放宽回收窗口时必须同步调大 `office_rejoin_budget_secs`。

## 4. 换房语义（`4106`）

Agent 已在**其它**房又请求入新房时，**MUST** 先显式 `server:leave_office`——Computer 的自动换房
规则**不适用**于 Agent（room-model.md §Agent 加入规则 1）。

`join_office` 因此在**本地**即按 canonical `4106 Already In Room` 快速失败（`details.office_id` 取
**当前**所在房，非被拒的目标房；文案与 `details` 键集复用
`smcp::build_room_rejection_error`，与服务端拒绝逐字节一致），**不发请求**：既避免无效往返，也避免
在服务端裁决前覆写本地意图。

## 5. 已知边界与跨端差异

- **无 `disconnect_final` 信号**：python 侧用 engineio 的 `__disconnect_final` 在「重连彻底放弃」时
  清空意图；`tf-rust-socketio` **不提供**该事件（重连次数耗尽后其内部循环静默 `break`）。后果：意图
  在传输层彻底失联后**保留**，但因不再有 Connect 事件，不会产生任何重放动作；若调用方之后重新
  `connect()`，则该意图会被重放（语义上正确：成员关系仍未恢复）。按 `max_reconnect_attempts`
  部署（本 SDK 未设置，即无限重试）时该分支不可达。
- **重连背压归底层**：重连延迟 / 退避 / 次数由 `tf-rust-socketio` 决定，本 SDK 的预算只约束**入房
  重放**，不影响传输层重连策略。
- **首次入房被拒不可自愈**：显式 `join_office` 撞上 `4101` / `4105` 时按协议视为永久冲突，直接
  返回 `Err`，不做退避重试，也不记录意图（须调用方处置）。
- **未 ack 的对端**：`tf-rust-socketio` 的 ack 超时只在 ack **到达**时才判过期，故房间事件在本 SDK
  侧额外加了一层**真 deadline**（见 `AsyncSmcpAgent::call_room_event`），保证「有界等待」名副其实。
  非房间事件（`client:*`）无此保护，属既有独立问题（另行跟踪）。
- **`SyncSmcpAgent` 同享**：同步包装直接复用同一份成员状态与后台回房 task，行为一致。

## 6. 测试

| 层次 | 位置 | 覆盖 |
|---|---|---|
| 纯函数 / 状态机单测 | `crates/smcp-agent/src/office.rs`（`#[cfg(test)]`） | 意图去留、世代作废、陈旧 Close、换房判据、退避曲线、预算边界、拒绝码分类、句柄世代约束 |
| 真设施端到端 | `crates/smcp-agent/tests/office_rejoin_test.rs` | 真实 Socket.IO 栈 + 真实 TCP 中断：重连重放并恢复 / 瞬态冲突退避重试恢复（并断言退避间隔真实生效）/ 预算耗尽回退且不再重试 / `4106` 不重试 / 在途回房被二次断连打断 / 无意图不重放 / 换房须先显式退房（本地 4106 且零请求） |
| 真实 SMCP 服务端 | `crates/smcp-agent/tests/room_ack_wiring_test.rs` | 入房 ack 契约与错误码接线（`4101` / `403` / `4103` / 幂等退房） |

```
cargo test -p smcp-agent --offline              # 单测 + 集成（含真设施回房矩阵）
cargo test -p smcp-agent --test office_rejoin_test
```

## 7. 相关

- 镜像来源：python-sdk#203、python-sdk#212；协议裁决
  [Discussion#61](https://github.com/A2C-SMCP/a2c-smcp-protocol/discussions/61)
- Computer 侧同源实现：#204（`05dfdc7`）、#211（陈旧 Close 守卫）
- 房间 ack 契约：#225 / #226（`server:join_office` 等三事件成功 = 空 ack、失败 = flat `ErrorPayload`）
