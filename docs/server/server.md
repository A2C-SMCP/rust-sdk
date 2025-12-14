
# SMCP Socket.IO Server：`smcp-server-core` 与 `smcp-server-hyper` 如何配合

本文档介绍当前仓库里，`crates/smcp-server-core` 如何与 `crates/smcp-server-hyper` 组合，实现 **A2C-SMCP 的 Socket.IO Server**。

面向读者：Rust 新手；目标是让你理解“为什么这样分层、数据怎么流动、hyper 为什么能接上 core”。

## 1. 总体分层：core 负责“协议与业务”，hyper 负责“传输与运行”

你可以把整个 Server 拆成两层：

- **`smcp-server-core`（协议/业务层）**
  - 定义并实现 SMCP 的 Socket.IO namespace、事件名、事件处理函数
  - 做认证（`AuthenticationProvider`）、会话管理（`SessionManager`）
  - 维护共享状态（`ServerState`）
  - 最关键：构建并导出一个可以被任何 HTTP 框架挂载的 **`tower::Layer`**（见 `SmcpServerLayer`）

- **`smcp-server-hyper`（传输适配层 / runtime 层）**
  - 选择 Hyper 作为 HTTP server 实现
  - 接受 TCP 连接、跑 HTTP/1.1、开启 upgrade（WebSocket）
  - 把 core 的 `SocketIoLayer` 挂到 tower service 链上
  - 提供可运行的 `run_server` / `HyperServer::run` 之类入口

这种拆法的直接收益：

- **core 不绑定具体 Web 框架**（hyper/axum/warp/actix 的差异都被隔离）
- **hyper crate 只做 I/O 与 glue code**，不关心 SMCP 事件细节
- 你未来想换运行时/框架时，通常只需要改 adapter（hyper crate），core 基本不动

## 2. 核心连接点：`SmcpServerLayer` = `SocketIo` + `SocketIoLayer` + `ServerState`

关键类型在：`crates/smcp-server-core/src/server.rs`

- `SmcpServerBuilder`
  - 收集配置（认证 provider、会话管理器等）
  - 调用 `build_layer()` 产出 `SmcpServerLayer`

- `SmcpServerLayer`
  - `io: SocketIo`：Socket.IO “服务器对象”（用于注册 namespace/事件，也可用于跨 socket 广播）
  - `layer: SocketIoLayer`：一个 tower layer，会“拦截并处理”与 Socket.IO 相关的 HTTP 请求（例如 `/socket.io/` 握手、upgrade 等）
  - `state: ServerState`：你业务侧需要的共享状态（会话管理、认证、以及一个 `Arc<SocketIo>`）

`build_layer()` 里面最重要的 3 步（概念化解释）：

1. `let (layer, io) = SocketIo::builder().build_layer();`
   - 这一步来自 `socketioxide`：它同时给你
     - 一个“协议入口”`layer`（放进 HTTP service 链里）
     - 一个“编程 API”`io`（用来注册 namespace、事件回调）

2. 组装 `ServerState`（`Arc` 共享）
   - `ServerState` 在 `crates/smcp-server-core/src/handler.rs`
   - 里面带：`session_manager`、`auth_provider`、`io: Arc<SocketIo>`

3. `SmcpHandler::register_handlers(&io, state.clone());`
   - 这里把 SMCP 的 namespace 与事件回调注册到 `SocketIo` 上
   - 这一步完成后，**core 侧就“声明式地”完成了协议行为**

## 3. `SmcpHandler`：把 SMCP 映射成 Socket.IO 的 namespace + event handlers

代码在：`crates/smcp-server-core/src/handler.rs`

### 3.1 namespace 级别的“连接钩子”

`register_handlers()` 会对固定 namespace（例如 `SMCP_NAMESPACE`）做：

- `io.ns(SMCP_NAMESPACE, |socket| async move { ... })`
- 连接建立时会先跑 `on_connect()`：
  - 从 `socket.req_parts().headers` 拿到请求头
  - 调用 `state.auth_provider.authenticate(&headers, auth_data).await?`

你可以把它理解为：

- **HTTP 握手阶段**（Engine.IO / Socket.IO 内部流程）
- 成功后进入 **Socket.IO 连接态**，才能收发事件

### 3.2 per-socket 的事件注册

连接通过认证后，会对这个 socket 注册一堆事件：

- `socket.on_disconnect(...)`
- `socket.on(smcp::events::SERVER_JOIN_OFFICE, ...)`（带 ack）
- `socket.on(smcp::events::CLIENT_TOOL_CALL, ...)`（带 ack）
- 等等

这里的模式是：

- `socket.on(event, handler)` 把“事件名”绑定到 async handler
- handler 入参通常是 `SocketRef` + `Data<T>` + `AckSender`
- handler 内部调用 `SmcpHandler::on_xxx(...)`，返回结果用 `ack.send(&result)` 发回去

这就是 SMCP 在 Socket.IO 之上的 RPC-ish 通信方式。

## 4. `SessionManager` 与 `ServerState`：让 handler “无状态”但系统“有状态”

`SessionManager` 在：`crates/smcp-server-core/src/session.rs`

它使用 `DashMap` 做并发 map，核心目的：

- 用 `sid -> SessionData` 保存连接的会话信息
- 用 `name -> sid` 做反查（并处理“同名冲突”）

`ServerState`（在 `handler.rs`）把以下对象用 `Arc` 包起来：

- `session_manager: Arc<SessionManager>`
- `auth_provider: Arc<dyn AuthenticationProvider>`
- `io: Arc<SocketIo>`

这样每个事件 handler 都可以 `clone()` 一份 `ServerState`，实现：

- handler 函数本身更像“纯函数”（入参 + 共享状态 + 输出）
- 共享状态在内部是线程安全/并发安全的

## 5. `AuthenticationProvider`：依赖倒置点（可插拔认证）

认证抽象在：`crates/smcp-server-core/src/auth.rs`

- `trait AuthenticationProvider`
  - `async fn authenticate(&self, headers: &HeaderMap, auth: Option<&Value>) -> Result<(), AuthError>`

- `DefaultAuthenticationProvider`
  - 默认从 header 里读 `x-api-key`（可配置字段名）

这里体现了一个非常经典的设计：

- **依赖倒置（DIP）**：handler 依赖抽象 `AuthenticationProvider`，而不是依赖某个具体实现
- 你可以在 builder 里注入自定义认证（查 DB、JWT、签名校验等），core 的事件逻辑不用改

## 6. hyper 侧怎么“接入”core：tower service 栈 + `SocketIoLayer`

关键代码在：`crates/smcp-server-hyper/src/lib.rs`

你可以把 hyper crate 理解为：

- 启动 TCP listener
- 对每条 TCP 连接跑 `hyper::server::conn::http1::Builder::new().serve_connection(...).with_upgrades()`
- 用 `tower::ServiceBuilder` 把 core 的 `layer.layer`（即 `SocketIoLayer`）挂上去

核心几行是：

- `let layer = self.layer.ok_or("SMCP layer not configured")?;`
- `let service = ServiceBuilder::new().layer(layer.layer).service(service_fn(...));`

解释一下这个 service 链：

- `service_fn` 你可以理解成“兜底的 HTTP handler”（比如 `/health`）
- `SocketIoLayer` 是一个 **中间件层**：
  - 当请求是 Socket.IO/Engine.IO 相关路径（通常是 `/socket.io/` 及其升级流程）时，中间件会接管
  - 否则就把请求交给下游的 `service_fn`

也就是说：

- **Socket.IO 相关请求走 core 提供的 layer**
- **普通 HTTP 路由走 hyper crate 自己的 handler**

这就是“为什么 hyper 能与 core 配合”的本质：**tower Layer 把协议处理做成可组合的中间件**。

## 7. 典型调用链（从 TCP 到你的事件函数）

下面用“文字时序图”描述一次典型流程：

1. 客户端发起 HTTP 请求：`GET /socket.io/?EIO=4&transport=polling...`
2. Hyper 接收到请求，进入 tower service 栈
3. `SocketIoLayer` 匹配到 `/socket.io/`，接管请求并完成 Engine.IO/Socket.IO 的握手逻辑
4. 协议内部在合适时机建立 Socket.IO 连接对象（socket）
5. 因为 core 在启动时已执行过 `SmcpHandler::register_handlers(&io, state)`：
   - socket 进入 `io.ns(SMCP_NAMESPACE, ...)` 的连接回调
6. `on_connect()`
   - 从请求头提取 API key
   - `AuthenticationProvider::authenticate()`
7. 认证通过后，`handle_connection()` 对该 socket 注册所有事件
8. 客户端 `emit` 一个 SMCP 事件（例如 `CLIENT_TOOL_CALL`）
9. socketioxide 将 payload 反序列化为 `Data<ToolCallReq>`，调用对应 handler
10. handler 执行业务逻辑、查/改 `SessionManager`，并用 `AckSender` 返回结果

## 8. 这里用到的设计模式（用 Rust 术语重新理解）

- **分层架构（Layered Architecture）**
  - core = 业务/协议层
  - hyper = 传输/运行层

- **适配器模式（Adapter）**
  - `smcp-server-hyper` 把 Hyper 的“连接/请求模型”适配成 tower service，并通过 `SocketIoLayer` 接入协议处理
  - 如果将来你写 `smcp-server-axum`，本质也是相同的 adapter

- **依赖注入 + 依赖倒置（DI + DIP）**
  - `AuthenticationProvider` 用 trait 抽象认证
  - `SmcpServerBuilder::with_auth_provider(...)` 注入实现

- **共享状态 + 并发安全容器**
  - `Arc<T>` 在 async/多任务间共享
  - `DashMap` 处理并发读写

- **事件驱动（Event-driven）**
  - SMCP 的交互以 Socket.IO event 为边界
  - handler = event 的消费者

## 9. 最小启动示例（伪代码，帮助你建立心智模型）

下面是“你想自己起一个 server”时的最小思路（非完整可编译示例，偏概念）：

```rust
// 1) core：构建 layer（里面包含 SocketIoLayer + SocketIo + ServerState）
let smcp_layer = smcp_server_core::SmcpServerBuilder::new()
    .with_default_auth(Some("admin-secret".into()), None)
    .build_layer()?;

// 2) hyper adapter：把 layer 挂到 service 栈并运行
smcp_server_hyper::HyperServerBuilder::new()
    .with_layer(smcp_layer)
    .with_addr("127.0.0.1:3000".parse()?)
    .build()
    .run("127.0.0.1:3000".parse()?)
    .await?;
```

如果你只想快速跑起来：`smcp-server-hyper` 里也提供了 `run_server(addr)`，内部会默认构建 `SmcpServerBuilder::new().build_layer()`。

## 10. 你作为新手可以从哪里开始读代码

建议阅读顺序（每个文件都不长）：

1. `crates/smcp-server-core/src/server.rs`
   - 先理解 `SmcpServerBuilder::build_layer()` 为什么同时产出 `io` 和 `layer`
2. `crates/smcp-server-core/src/handler.rs`
   - 看 `register_handlers()`、`on_connect()`、以及事件注册模式
3. `crates/smcp-server-core/src/session.rs`
   - 看会话如何注册/注销/查找
4. `crates/smcp-server-hyper/src/lib.rs`
   - 看 `ServiceBuilder::layer(layer.layer)` + `.with_upgrades()`

## 11. 常见疑问

### 11.1 为什么 `handle_request()` 里对 `/socket.io/` 返回 404 也没关系？

因为真正处理 `/socket.io/*` 的不是 `handle_request()`，而是挂在它“上面”的 `SocketIoLayer`。

当 `SocketIoLayer` 匹配到 Socket.IO 请求时，它会提前处理并生成响应，下游根本不会走到你写的 match 分支。

### 11.2 `ServerState` 里为什么要放一个 `Arc<SocketIo>`？

某些业务场景需要在一个事件 handler 内，对“其他 socket / 房间”进行广播或定向发送。

通过 `Arc<SocketIo>`，你可以在任意 handler 中拿到同一个 `io` 引用，实现跨连接通信（具体 API 取决于 socketioxide 的接口）。

---

## 状态

- 已在本文档说明 core/hyper 的协作机制与设计模式。
- 如你希望我补一段“从测试用例看真实交互”的讲解，我可以继续基于 `crates/smcp-server-core/tests` 或集成测试补充一节。

