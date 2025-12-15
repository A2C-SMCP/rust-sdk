## 服务端集成测试策略

---

### 1. 测试目标

 1. **验证协议一致性**：确保 `a2c_smcp/server` 对 Socket.IO 事件的实现与官方 SMCP 协议完全一致（事件命名、载荷、权限校验）@tests/integration_tests/server/test_namespace_async.py#1-519。
2. **覆盖同步与异步双栈**：异步版（`SMCPNamespace`）与同步版（`SyncSMCPNamespace`）都需要真实服务端环境验证，避免 GIL、线程模型带来的分歧@tests/integration_tests/server/test_namespace_sync.py#1-539。
3. **校验房间/会话工具函数**：`aget_computers_in_office` 等工具函数必须在真实连接条件下返回正确的会话快照@tests/integration_tests/server/test_utils_async.py#1-72。

---

### 2. 目录结构与层级

```
tests/integration_tests/server/
 ├── test_namespace_async.py      # 异步命名空间行为回归
 ├── test_namespace_sync.py       # 同步命名空间行为回归
├── test_utils_async.py          # 异步工具函数
├── _local_sync_server.py        # 仅测试使用的同步 Socket.IO 服务端
└── a2c_smcp/server/...          # 测试专用轻量实现（如需要 monkeypatch）
```

分层约定：

 1. **夹具层**：`socketio_server`（异步）与 `_local_sync_server`（同步）会真实启动 Socket.IO 服务，所有测试都以「连上真实服务端」为前提。
 2. **场景层**：每个用例都创建智能体/计算机客户端（`socketio.AsyncClient` 或 `socketio.Client/SimpleClient`），模拟真实三方交互。
3. **断言层**：不仅断言返回值，也断言广播事件、房间状态、错误信息，以保证协议语义在其他语言 SDK 中可复现。

---

### 3. 环境依赖

 - **真实 Socket.IO 服务端**：
  - 异步测试复用 `tests/integration_tests/mock_socketio_server.py` 中的 `MockComputerServerNamespace`；
  - 同步测试通过 `_local_sync_server.py` 启动 WSGI + `socketio.Server`，并在独立进程/线程运行，确保 call() 不被阻塞。
 - **多客户端并发**：大多数用例至少需要 1 个智能体 + 1 个计算机，部分验证（如房间列举）需要 3 个及以上客户端。
- **跨进程/线程同步**：同步测试为了避免 GIL，采用 `multiprocessing.Process` + `threading.Event`，其它语言 SDK 可用各自的并发原语实现等效逻辑。

补充说明（同步栈关键参数）：

1. **同步服务端必须启用 async_handlers**：
  - Python 参考实现位于 `tests/integration_tests/server/_local_sync_server.py`。
  - 关键点：`socketio.Server(async_handlers=True)`，否则 `client.call()` 在同步模式下很容易被阻塞，导致“调用永远等不到 ACK”。
2. **建议独立进程启动 WSGI 服务**：
  - Python 使用 `werkzeug.serving.make_server(..., threaded=True)` 放入独立 `multiprocessing.Process`。
  - 目的：隔离测试主进程与服务端事件循环/线程调度，避免互相抢占造成不稳定。
3. **统一 socketio_path**：
  - 测试里使用 `socketio_path="/socket.io"`，其他语言复刻时也应保持一致。

结论：**所有集成测试必须连接真实服务端，不接受纯 mock。**

---

### 4. 关键测试场景

 1. **房间生命周期**：
   - `test_enter_and_broadcast` / `test_enter_and_broadcast_sync` 验证智能体/计算机加入顺序与 `notify:enter_office` 广播。
   - `test_leave_and_broadcast` / `test_leave_and_broadcast_sync` 验证离开事件与 `notify:leave_office` 广播。
 2. **工具调用链**：
   - 异步 `test_tool_call_roundtrip`、同步 `test_tool_call_forward_sync` 均要求智能体通过 `client:tool_call` 发起请求，服务端转发并聚合计算机返回值。
 3. **工具/配置同步**：
   - `test_get_tools_success_*`：智能体拉取计算机工具列表，计算机需通过事件处理器返回结构化结果。
   - `test_update_config_broadcast_*`：计算机触发 `server:update_config` 后，智能体必须收到 `notify:update_config`。
4. **房间会话查询**：
  - `test_list_room_success` 与 `test_list_room_empty_office` 覆盖多客户端与单客户端两种情况，确保 `server:list_room` 返回完整会话信息。
5. **名称冲突与房间切换**：
  - `test_computer_duplicate_name_rejected`、`test_computer_different_name_allowed`、`test_computer_switch_room_with_same_name_allowed` 规范计算机命名唯一性与切房行为。
6. **工具函数验证**：
  - `test_aget_computers_and_sessions` 通过真实连接验证 `aget_computers_in_office` 与 `aget_all_sessions_in_office` 的数据准确性。

每个场景都对应一组必要条件（房间状态、角色、命名约束），其他语言实现时需严格对齐，才能保证 SDK 间互操作。

---

 ### 4.1 用例矩阵（建议对照复刻）

本节用于把“场景”落到“具体测试函数”，便于其他语言 SDK 团队逐个复刻并保持断言一致。

#### 异步（`test_namespace_async.py`）

1. **`test_enter_and_broadcast`**：
  - 目标：验证 `server:join_office` 后，房间内其他成员收到 `notify:enter_office`。
  - 重点断言：智能体侧确实收到广播；载荷至少包含 `office_id` 且能区分 `agent/computer` 字段。
 2. **`test_leave_and_broadcast`**：
   - 目标：验证 `server:leave_office` 会触发 `notify:leave_office` 广播。
   - 重点断言：房间内其他客户端收到广播；使用名称而非 `sid` 标识离开者。
 3. **`test_tool_call_roundtrip`**：
   - 目标：验证智能体通过 `client:tool_call` 发起请求，服务端转发并聚合计算机返回值。
   - 重点断言：智能体的 `call()` 返回结构化结果。
 4. **`test_get_tools_success_same_office`**：
   - 目标：验证 `client:get_tools` 只允许同一 `office` 内的智能体拉取计算机工具列表。
   - 重点断言：计算机事件处理器被调用；返回 `tools` 列表与 `req_id` 透传。
 5. **`test_update_config_broadcast`**：
   - 目标：验证计算机 `server:update_config` 会触发 `notify:update_config` 广播。
   - 重点断言：广播载荷中 `computer` 字段正确。
6. **`test_list_room_success` / `test_list_room_empty_office`**：
  - 目标：验证 `server:list_room` 返回同房间会话快照。
  - 重点断言：`sessions` 数量正确；每个 session 的 `role/name/office_id` 合法。
 7. **`test_computer_duplicate_name_rejected`**：
   - 目标：验证同一房间内计算机名称唯一。
   - 重点断言：第二个同名计算机加入失败；错误信息包含 `already exists`。
 8. **`test_computer_different_name_allowed`**：
   - 目标：验证不同名计算机可共存。
   - 重点断言：第二个计算机加入成功且无错误。
 9. **`test_computer_switch_room_with_same_name_allowed`**：
   - 目标：验证同一个计算机可“切房”，且切房时允许保持相同名称。
   - 重点断言：第二次加入（不同 `office`）成功；服务端能正确从旧房间离开并加入新房间。

#### 同步（`test_namespace_sync.py`）

1. **`test_enter_and_broadcast_sync` / `test_leave_and_broadcast_sync`**：
  - 目标与断言：与异步版本同构；注意使用 `time.sleep` 等待广播。
2. **`test_get_tools_success_sync`**：
  - 目标：在同步客户端 `call()` 场景下，验证 get_tools 全链路。
  - 复刻建议：像 Python 一样把计算机与智能体放入独立进程，避免阻塞。
3. **`test_update_config_broadcast_sync`**：
  - 目标与断言：与异步版本一致。
 4. **`test_tool_call_forward_sync`**：
   - 目标：验证同步模式下的 `tool_call` 转发与返回。
   - 复刻建议：像 Python 一样用独立线程运行计算机客户端，并用同步事件协调“已连接/已完成”。
5. **`test_computer_*` 三件套**（重名拒绝/不同名允许/切房允许）：
  - 目标与断言：与异步版本一致；部分测试使用 `SimpleClient` 简化连接与 `call`。

#### 工具函数（`test_utils_async.py`）

1. **`test_aget_computers_and_sessions`**：
  - 目标：验证在真实房间状态下，`aget_computers_in_office` 返回的都是计算机 session，且数量正确；`aget_all_sessions_in_office` 返回房间内全部 session。
  - 重点断言：会话数 = 1 智能体 + N 计算机。

---

### 5. 迁移指南

1. **保持事件常量一致**：使用与 Python SDK 相同的 `JOIN_OFFICE_EVENT` 等常量，避免硬编码字符串造成协议漂移。
 2. **复用真实服务端模式**：
   - 也可以在目标语言中用相同逻辑实现一个轻量服务端，只需放行认证并复用 `SMCPNamespace` 行为。
3. **确保多客户端隔离**：测试必须支持至少 3 个客户端同时连入，以验证房间广播、工具列表与会话查询。
4. **模拟并发边界**：同步实现应在独立进程/线程运行，确保 `client.call()` 能等待到计算机的响应；异步实现用 `asyncio` 等事件循环完成整链验证。
 5. **断言房间状态**：除返回值外，还需检查服务端 session（`office_id`、`role`、`name`），可通过各语言的服务端接口或额外的 `list_room` 调用完成。
6. **复刻工具函数测试**：若语言 SDK 提供 server 工具函数，同样需要构造真实会话再调用，避免在空上下文中单元测试。

---

### 6. 建议补充的测试项（当前 Python 集成测试未覆盖）

以下用例对“跨语言 SDK 互操作一致性”很关键，即使 Python 端暂未覆盖，也建议其他语言在复刻时一并补齐（或至少在文档中明确不支持）：

1. **跨房间权限拒绝**：
  - 智能体与计算机不在同一 `office_id` 时：`client:get_tools`、`client:get_desktop`、`client:tool_call` 应拒绝或返回明确错误。
 2. **计算机不存在**：
   - `client:*` 指向的计算机名称未注册时，应返回稳定错误（建议错误码/关键字可断言）。
3. **智能体单房间约束**：
  - 智能体重复加入不同 office 应失败（对应 `SMCPNamespace.enter_room` 的约束）。
4. **角色冲突**：
  - 同一 sid 先以 `agent` 加入，再尝试以 `computer` 加入（或反之）应失败并返回明确错误。
5. **桌面相关事件**：
  - `client:get_desktop`（拉取）与 `server:update_desktop`（通知）建议补测。
6. **工具列表变化通知**：
  - `server:update_tool_list` 触发 `notify:update_tool_list` 广播建议补测。
7. **取消工具调用**：
  - `server:tool_call_cancel` 的权限校验（必须是对应 Agent）与广播语义建议补测。

遵循以上策略，即可在其他语言中获得与 Python SDK 相同的测试覆盖度与可靠性保障。
