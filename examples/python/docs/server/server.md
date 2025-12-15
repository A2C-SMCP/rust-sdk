## 服务端模块概览
 
 `a2c_smcp/server` 模块负责实现 **SMCP 信令服务端**，以 Socket.IO 命名空间为载体，在智能体与计算机之间提供统一的会话、认证与事件转发能力：
 
 - **房间管理**：维护智能体-计算机一对一会话上下文，确保加入、离开与广播都有明确语义；
 - **事件路由**：把智能体侧的 `client:*` 请求（如工具调用、获取桌面）可靠转发给目标计算机，并处理返回结果；
 - **系统通知**：在计算机上报配置、工具或桌面变化时，向房间内其他成员广播 `notify:*` 事件，保持状态一致；
 - **安全与审计**：通过可插拔认证接口，在连接阶段完成身份校验，为后续事件提供安全前提。 @a2c_smcp/server/base.py#20-185
 
 服务端与其他角色协作关系：智能体通过 `SMCPAgentClient` 连接服务端，计算机通过 `SMCPComputerClient` 连接服务端，所有跨端能力都由服务端的 `SMCPNamespace` 在房间内转发与编排。

---

## 核心组件与职责

### 认证层：`AuthenticationProvider`

- 抽象类 `AuthenticationProvider`（异步）与 `SyncAuthenticationProvider`（同步）定义了统一的 `authenticate()` 钩子，可接入自定义鉴权逻辑；
- 默认实现 `DefaultAuthenticationProvider` / `DefaultSyncAuthenticationProvider` 支持从请求头提取 `x-api-key`（可配置）并与 `admin_secret` 比对，通过后才允许连接；
- 认证函数会收到完整的环境变量、认证载荷与原始请求头，开发者可以在此扩展数据库校验、令牌签名等策略。 @a2c_smcp/server/auth.py#16-85 @a2c_smcp/server/sync_auth.py#16-84

### 命名空间抽象：`BaseNamespace` 与 `SyncBaseNamespace`

- 两个抽象类分别对应异步/同步服务端，用统一的方式封装连接生命周期：
  - `on_connect`：提取请求头、调用认证提供者、打点日志；
  - `on_disconnect`：回收房间、注销 name→sid 映射；
  - `trigger_event`：把协议里的 `server:*`/`client:*` 事件名中的冒号转换成下划线，匹配 Socket.IO 方法命名约定；
- 名称注册表 `_name_to_sid_map` 允许通过计算机/智能体名称快速定位 Socket.IO 的 `sid`，是后续点对点工具调用的基础。 @a2c_smcp/server/base.py#20-185 @a2c_smcp/server/sync_base.py#20-147

### 业务命名空间：`SMCPNamespace`

- 继承自 `BaseNamespace`，固定工作在 `SMCP_NAMESPACE = "/smcp"` 下；
- 负责所有协议事件的实际处理，包括加入/离开房间、工具调用、桌面与配置同步、工具列表广播、房间会话查询等；
- 使用 `pydantic.TypeAdapter` 对每个请求/响应进行严格校验，避免跨进程结构漂移；
- 借助 `aget_all_sessions_in_office` 等工具函数获取会话快照，为智能体提供 `server:list_room` 能力。 @a2c_smcp/server/namespace.py#50-500

### 工具函数：`a2c_smcp/server/utils.py`

- 提供同步/异步版本的 `get_computers_in_office`、`get_all_sessions_in_office`，统一封装 Socket.IO manager 的房间遍历逻辑；
- 这些函数会自动过滤智能体自身的 `sid`，并通过 `TypedDict` 做结构验证，可直接用于广播或监控场景。 @a2c_smcp/server/utils.py#18-131

---

## 关键事件流

### 办公室（房间）管理

- `on_server_join_office`：校验角色、写入会话（`role`/`name`/`office_id`），然后调用自定义的 `enter_room` 执行房间加入逻辑；
- `enter_room`：为智能体/计算机注入默认名称、注册 name→sid 映射，并根据角色广播 `notify:enter_office`；若智能体重复进入或计算机同名冲突会直接抛错；
- `on_server_leave_office` 与 `leave_room`：先向房间广播离开通知，再清理会话中的 `office_id` 与名称映射，保证状态回收。 @a2c_smcp/server/namespace.py#66-200 @a2c_smcp/server/namespace.py#201-265

### 工具调用链

- `on_client_tool_call`：限定只有智能体可发起，使用 `_name_to_sid_map` 查找目标计算机的 `sid`，然后通过 `self.call` 转发 `TOOL_CALL_EVENT`，支持自定义超时；
- `on_client_get_tools` / `on_client_get_desktop`：同样要求智能体与计算机在同一房间，调用计算机侧对应事件并把结果回传；
- 若计算机未注册（名称不存在）或不在房间中，将立即抛出 `ValueError` 保障一致性。 @a2c_smcp/server/namespace.py#332-440

### 配置、工具列表与桌面通知

- `on_server_update_config` / `on_server_update_tool_list` / `on_server_update_desktop`：由计算机侧触发，服务端在房间内广播 `notify:update_config`、`notify:update_tool_list`、`notify:update_desktop`，提示智能体重新拉取最新状态；
- `on_server_tool_call_cancel`：智能体可向房间广播取消某次工具调用的指令，服务端会验证智能体身份与名称后再发出通知，避免误取消。 @a2c_smcp/server/namespace.py#266-461

### 会话洞察

- `on_server_list_room` 借助 `aget_all_sessions_in_office` 返回房间内所有计算机/智能体的 `SessionInfo` 列表，严格校验发起智能体必须身处同一房间；
- 工具函数可用于运维监控、网页界面或 CLI 的房间态势展示。 @a2c_smcp/server/namespace.py#462-500 @a2c_smcp/server/utils.py#84-131

---

## 会话与房间管理要点

1. **名称-连接 ID 映射**：注册/注销流程会结合 Socket.IO 会话，确保即使重连也能保持名称唯一；
2. **角色约束**：智能体只能存在于一个房间，计算机在切换房间时会自动广播离开消息；
3. **会话冗余信息**：服务端在 `enter_room` 过程中写入 连接 ID、名称、房间 ID 等字段，为后续广播、列表查询提供冗余索引；
4. **异常回滚**：`on_server_join_office` 在任何异常时都会恢复旧会话，避免部分状态写入导致脏数据；
5. **同步/异步双栈**：同步版本 Namespace 与认证接口让需要运行在同步 WSGI/ASGI 环境（或测试环境）的用户也能重用同样的协议实现。 @a2c_smcp/server/base.py#114-166 @a2c_smcp/server/namespace.py#66-200 @a2c_smcp/server/sync_base.py#86-138

---

## 扩展与最佳实践

1. **保持协议前缀约定**：新增事件时沿用 `client:*` / `server:*` / `notify:*` 命名，便于 Socket.IO 层统一路由；
2. **强校验优先**：延续 `TypeAdapter`+TypedDict 的写法，让数据结构问题在服务端侧即时暴露；
3. **关注房间一致性**：任何需要智能体/计算机定位的事件，都应复用 `_name_to_sid_map` 与 `office_id` 断言，防止“串线”；
4. **善用工具函数**：在运维工具或 CLI 中复用 `get_all_sessions_in_office` 等函数，可快速洞察当前房间状态；
5. **认证可插拔**：如果要接入企业认证、JWT 或多级权限，只需实现自定义 `AuthenticationProvider` 并在 Namespace 初始化时注入。 @a2c_smcp/server/namespace.py#201-500 @a2c_smcp/server/utils.py#18-131


---

## 模块入口与导出

`a2c_smcp.server` 提供了对外最常用的导出，便于使用方“按需导入”并保持 API 稳定：

- **异步栈**：`AuthenticationProvider`、`DefaultAuthenticationProvider`、`BaseNamespace`、`SMCPNamespace`
- **同步栈**：`SyncAuthenticationProvider`、`DefaultSyncAuthenticationProvider`、`SyncBaseNamespace`、`SyncSMCPNamespace`
- **类型与工具函数**：`OFFICE_ID`、`SID`、`Session`、`aget_*` / `get_*` 系列房间快照函数

当你只需要“快速搭建一个可用的 SMCP 服务端”时，通常直接从 `a2c_smcp.server` 导入上述对象即可，无需关心具体文件路径。


---

## 快速开始：如何挂载 SMCP 命名空间

下面示例的核心目标是说明“挂载点”与“依赖注入点”（认证提供者），方便你在任意网页框架（ASGI/WSGI）中集成。

### 异步：`SMCPNamespace`

```python
import socketio

from a2c_smcp.server import DefaultAuthenticationProvider, SMCPNamespace


sio = socketio.AsyncServer(async_mode="asgi")

# 注入认证：默认实现使用请求头 x-api-key 与 admin_secret 比对
auth_provider = DefaultAuthenticationProvider(admin_secret="YOUR_ADMIN_SECRET")
sio.register_namespace(SMCPNamespace(auth_provider=auth_provider))

# 交给你的 ASGI 框架挂载
app = socketio.ASGIApp(sio)
```

### 同步：`SyncSMCPNamespace`

```python
import socketio

from a2c_smcp.server import DefaultSyncAuthenticationProvider, SyncSMCPNamespace


sio = socketio.Server(async_mode="threading")

auth_provider = DefaultSyncAuthenticationProvider(admin_secret="YOUR_ADMIN_SECRET")
sio.register_namespace(SyncSMCPNamespace(auth_provider=auth_provider))

# 交给 WSGI 服务器挂载
app = socketio.WSGIApp(sio)
```


---

## 同步栈（`sync_*`）的定位与抽象思路

同步栈的设计目标是：在“同步线程模型/WSGI”或部分测试环境中，让你无需引入 `asyncio` 也能复用同一套 SMCP 协议语义。

1. **同构抽象**：
   - `BaseNamespace` 对应 `SyncBaseNamespace`
   - `SMCPNamespace` 对应 `SyncSMCPNamespace`
   - `AuthenticationProvider` 对应 `SyncAuthenticationProvider`

2. **一致性优先**：
   - 事件名规则一致：通过重写 `trigger_event()` 将协议事件里的 `:` 映射为方法名 `_`，保证 `server:join_office` → `on_server_join_office`。
   - 状态模型一致：都使用 Socket.IO 会话存储 `role/name/office_id/sid`，并维护 `_name_to_sid_map` 做 name→sid 的快速定位。

3. **差异边界**：
   - 异步栈使用 `await self.call(...)`，同步栈使用 `self.call(...)`；两者对上层语义一致，但同步版本更依赖线程/进程调度避免阻塞。
   - 工具函数提供 `aget_*` 与 `get_*` 两套实现，调用方应与自身服务端类型（`AsyncServer`/`Server`）匹配。

如果你的部署环境天然是异步（ASGI），优先使用异步栈；只有在必须同步运行或测试约束下，再选择同步栈。
