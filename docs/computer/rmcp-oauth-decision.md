# Computer 远程 MCP OAuth 与 rmcp 升级决策

- 日期：2026-07-27
- 分支：`explore/smcp-oauth`
- 结论：**Go（分阶段）**
- 当前实施目标：升级到精确锁定的 `rmcp = 2.2.0`，实现面向 MCP `2025-11-25` 的完整 OAuth 客户端能力
- 暂不进入生产默认路径：`rmcp = 3.0.0-beta.2` 与 MCP `2026-07-28` 完整协议生命周期
- 规范性宿主契约：[Computer OAuth host integration contract](../../crates/smcp-computer/docs/oauth-host-integration.md)

## 1. 决策摘要

Computer 应当具备连接受 OAuth 保护的远程 Streamable HTTP MCP Server 的能力。OAuth 的协议状态、凭据恢复、授权发起、回调完成、刷新和增量授权属于 SDK；打开浏览器、监听本地回调端口和展示交互属于 Desktop/CLI 等上层封装。

本次建议：

1. 立即离开 `rmcp 0.11.0`。该版本落在多个 OAuth 高危漏洞的受影响范围内。
2. 生产实现基于 `rmcp 2.2.0`，同时开启 `auth` 与 `auth-client-credentials-jwt`。
3. 支持 rmcp 2.2 当前提供的全部 OAuth 方式：
   - Authorization Code + PKCE S256；
   - 预注册 public/confidential client；
   - Client ID Metadata Documents（CIMD）；
   - Dynamic Client Registration（DCR）；
   - Client Credentials：client secret；
   - Client Credentials：`private_key_jwt`；
   - token refresh、refresh token rotation；
   - `403 insufficient_scope` step-up authorization。
4. 在 SDK 中增加稳定的 OAuth facade，不将 rmcp 的 `OAuthState` 或 token 类型直接暴露给 Desktop/CLI。
5. `2026-07-28` 完整协议支持等待 rmcp 3 stable。当前 beta 只允许进入实验性 feature，不作为默认生产依赖。
6. 发布前必须解决或规避 rmcp OAuth discovery 的 synthetic `initialize` POST 可能产生孤儿 session 的问题。

如果业务明确要求在 rmcp 3 stable 发布前就连接“只支持 MCP 2026-07-28、拒绝 2025-11-25”的 Server，则该子目标应单独按 **Experiment/Defer** 管理，不能混入本次稳定 OAuth 交付。

## 2. 决策问题

> 为了让 Computer 连接受 OAuth 保护的远程 MCP Server，应该升级到哪个 rmcp 版本，SDK 应承担哪些 OAuth 生命周期职责，如何覆盖全部 rmcp 授权方式，同时控制公共 API、安全与未来 MCP 2026 升级风险？

### 必须满足

- OAuth 只作用于远程 Streamable HTTP MCP Client，不影响 stdio。
- SDK 可查询授权状态并主动发起授权。
- SDK 返回授权 URL 和结构化状态；SDK 本身不打开浏览器。
- token、refresh token、client secret、private key 不进入 `mcp.json`、日志或普通 Debug 输出。
- 已有静态 HTTP headers 鉴权继续可用。
- 已有 4006/4007 错误分类不能回退。
- 覆盖 rmcp 当前支持的全部交互式和机器身份授权方式。

### 非目标

- 在 SDK 内实现浏览器 UI。
- 在 SDK 内永久占用固定 callback 端口。
- 为 stdio MCP 增加 OAuth。
- 自研 OAuth 协议栈替代 rmcp。
- 本阶段完整实现 MCP 2026 的 stateless lifecycle、subscriptions/listen、standard headers、MRTR、Tasks 等非 OAuth 变更。
- 企业托管授权扩展；rmcp 当前没有完整支持该扩展。

## 3. 当前项目事实

### 当前依赖与传输

- workspace 使用 `rmcp 0.11.0`，未启用 `auth`。
- workspace 直接使用 `reqwest 0.12`。
- `HttpMCPClient::connect` 创建普通 reqwest client，并通过静态 headers 构造 `StreamableHttpClientTransport`。
- `HttpServerParameters` 只有 `url` 和 `headers`，没有 OAuth 配置。
- 当前实现已能把真实传输中的：
  - `401` 分类为 4006；
  - `403` 分类为 4007；
  - `401 + WWW-Authenticate` 对应的 rmcp `AuthRequired` 结构化分类为 4006。

### 升级不是内部依赖变更

`smcp-computer` 公开 re-export 多个 `rmcp::model` 类型，`MCPClientProtocol` 的返回值也直接使用 `Tool`、`CallToolResult` 和 resource 类型。因此 rmcp 升级会影响 SDK 公共 API 和下游编译，不能作为 patch 版本依赖更新处理。

建议：

- 以 `0.4.0` 或项目约定的下一个 breaking SDK 版本交付；
- 新增内部 `rmcp_compat` 层；
- 后续逐步用 SMCP 自有 facade 类型替代公开 rmcp 类型。

## 4. 外部规范与版本差异

### MCP 2025-11-25 OAuth

核心要求包括：

- RFC 9728 Protected Resource Metadata；
- RFC 8414 与 OIDC discovery；
- 解析 `401 WWW-Authenticate`；
- Authorization Code + PKCE S256；
- RFC 8707 `resource` 参数；
- 预注册、CIMD、DCR；
- 安全 token 存储；
- `403 insufficient_scope` step-up；
- Client Credentials 扩展用于 M2M。

来源：

- <https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization>
- <https://modelcontextprotocol.io/extensions/auth/oauth-client-credentials>

### MCP 2026-07-28 RC

OAuth 主流程没有被替换，主要强化：

- 授权响应 `iss` 校验；
- DCR `application_type`；
- DCR/预注册客户端凭据绑定 authorization server issuer；
- scope step-up 累积旧 scope 与新 scope；
- OIDC `offline_access` 指导；
- `.well-known` 路径澄清。

但 2026 的整体协议变化远大于 OAuth：

- stateless core；
- initialize/session 生命周期变化；
- standard MCP headers；
- `subscriptions/listen` 取代 legacy resources subscribe/unsubscribe；
- 新的 request metadata；
- result type、MRTR、Tasks 和 extension 机制变化。

来源：<https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/>

### rmcp 2.2 与 3 beta

| 维度 | rmcp 2.2.0 | rmcp 3.0.0-beta.2 |
|---|---|---|
| 稳定性 | 稳定 release | beta |
| 默认协议版本 | 2025-11-25 | 2025-11-25 |
| 2026 常量 | 存在，但没有完整现代 client lifecycle | 存在并有现代 lifecycle |
| OAuth Code + PKCE | 支持 | 支持 |
| 预注册 client | 低层 `AuthorizationManager` 支持 | 统一 `AuthorizationRequest` 支持 |
| CIMD | 支持 | 支持 |
| DCR | 支持 | 支持 |
| Client Credentials secret | 支持 | 支持 |
| `private_key_jwt` | feature 支持 | feature 支持 |
| issuer 绑定到 StoredCredentials | 无，需 SMCP 补充 namespace/校验 | 有 |
| scope 多轮累积 | 能做 step-up，但状态模型较弱 | 明确实现 SEP-2350 |
| discovery 来源可观测 | 不完整 | 暴露 PRM/AS/legacy 来源 |
| legacy metadata 缺少 issuer | 接受 | 拒绝，存在开放兼容性 Issue |
| 迁移到当前 SMCP | 25 个编译错误 | 26 个编译错误，另含 2026 lifecycle 警告 |

发布说明：

- rmcp 2.2.0：<https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v2.2.0>
- rmcp 3.0.0-beta.2：<https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.0.0-beta.2>

## 5. 安全证据

当前 `rmcp 0.11.0` 受以下问题影响：

- OAuth `resource_metadata` SSRF，受影响 `<= 1.8.0`，修复于 2.0.0：
  <https://github.com/modelcontextprotocol/rust-sdk/security/advisories/GHSA-c9xm-49cp-xcr9>
- PRM 缺少 `resource` 验证，可能导致 token 被发送给恶意 MCP Server，受影响 `<= 1.8.0`，修复于 2.0.0：
  <https://github.com/modelcontextprotocol/rust-sdk/security/advisories/GHSA-33f5-2c5q-wgwj>
- 自定义 HTTP header 可能随跨 origin redirect 泄漏，受影响 `<= 1.7.0`，修复于 2.1.0：
  <https://github.com/modelcontextprotocol/rust-sdk/security/advisories/GHSA-9g45-5xwm-f3wc>

因此：

- 不允许通过“给 0.11 开启 auth feature”完成需求；
- 最低候选必须是 `rmcp >= 2.1.0`；
- 本次选择 2.2.0。

仍需处理的上游风险：

1. rmcp 2.2 与 3 beta 的 OAuth discovery 都可能在 GET 404/405 后发送 synthetic `initialize` POST；若 Server 创建 session，rmcp 没有 DELETE，可能泄漏 session。
   - Issue：<https://github.com/modelcontextprotocol/rust-sdk/issues/1048>
   - 发布门禁：升级到包含修复的稳定版本，或在 SMCP 的 `OAuthHttpClient` decorator 中识别 probe response 并 best-effort DELETE。
2. rmcp 3 beta 对缺少 `issuer` 的 legacy AS metadata 直接拒绝。
   - Issue：<https://github.com/modelcontextprotocol/rust-sdk/issues/1047>
   - 本次本地实验已复现。

## 6. 实验记录

### 环境

- SMCP 基线 commit：`961771ea353ca4733935d45b816c265885cf0eae`
- 分支：`explore/smcp-oauth`
- Rust：本地项目 1.94.0；上游 rmcp 仓库固定工具链 1.96.1
- 候选：
  - rmcp 2.2.0，commit `519577601db3823616dbd7c4eb84ed569d8e17d4`
  - rmcp 3.0.0-beta.2，commit `14298b72e0b25473ea79d5465fe186e22eb86397`
- 所有 OAuth fixture 仅监听 `127.0.0.1`，使用测试 token 和临时 RSA key。
- 实验代码和 worktree 位于 `/tmp`，未进入业务实现。

### 依赖与编译

初次升级发现：

- rmcp 2.2 的 lockfile 保留 `sse-stream 0.2.3`，与 rmcp 调用的 `from_bytes_stream` API 不兼容；
- 显式升级到 `sse-stream 0.2.5` 后进入项目编译；
- 两个候选都使用 reqwest 0.13；
- 将 SMCP 直接 reqwest 升到 0.13 后，仍有部分传递依赖保留 reqwest 0.12，因此要检查跨版本类型边界。

对齐 reqwest 后：

| 候选 | `cargo check -p smcp-computer --all-features` |
|---|---|
| rmcp 2.2.0 | 25 errors、24 warnings |
| rmcp 3 beta.2 | 26 errors、28 warnings |

主要错误类别：

- `Content`、`Annotated`、`RawContent` 等模型重构；
- rmcp model 结构体改为 `non_exhaustive`，不能继续使用字段字面量构造；
- cancellation request id 改为 `Option`；
- Resource metadata API 变化；
- non-exhaustive enum match；
- beta 的 `last_modified` 类型变化；
- beta 的 legacy resources subscribe/unsubscribe 被标记废弃。

beta 相对 2.2 的源码增量很大：

- `transport/auth.rs`：约 `+1687/-232`；
- `model.rs`：约 `+1114/-245`；
- client/server service：约 `+2469/-59`；
- streamable HTTP client：约 `+868/-255`。

这说明 3 beta 的收益不只是 OAuth API 改善，也会把 2026 整体协议迁移带入本次工作。

### OAuth 上游测试

| 实验 | rmcp 2.2.0 | rmcp 3 beta.2 |
|---|---:|---:|
| OAuth 单元测试 | 118/118 | 157/157 |
| Client Credentials secret fixture | 4/4 | 4/4 |
| DCR + PKCE S256 + callback + token exchange | 通过 | 通过 |
| `private_key_jwt` localhost HTTP fixture | 被 HTTPS-only 检查拒绝 | 通过 |
| legacy AS metadata 缺少 issuer | 接受 | 拒绝 |
| message schema tests | 2/2 | 3/3 |
| 2026 `resultType` wire tests | 不存在 | 9/9 |

说明：

- Client Credentials 测试最初因当前环境设置了 HTTP proxy、未设置 NO_PROXY 而失败；排除 localhost proxy 后两版均通过。这是实验环境因素，不计为 rmcp 缺陷。
- 2.2 的 `private_key_jwt` 对 token endpoint 强制 HTTPS，因而无法使用纯 HTTP localhost fixture 完成网络端到端；其内部 JWT/config/metadata 单元测试通过。生产 acceptance 必须使用本地 TLS fixture 或真实测试 IdP 再验证一次。
- beta 对缺 issuer 的拒绝与上游 Issue #1047 一致，实验不是仅复述 Issue。

### SMCP 基线

| 基线测试 | 结果 |
|---|---:|
| `auth_error_real_transport` | 5/5 |
| `mcp_clients_integration` | 10/10 |
| `secret_store` | 3/3 |
| OAuth SDK 状态机原型 | 3/3 |

状态机原型验证了：

- SDK 可以返回 `AuthorizationLaunch { authorization_url, csrf_state }`；
- Desktop/CLI 可以独立决定如何打开浏览器；
- invalid callback 能形成 typed failure；
- step-up 能累积 scope；
- 核心层不需要 UI 依赖。

## 7. 方案比较

### A. 留在 rmcp 0.11，自行启用 OAuth

结论：**No-Go**

- 已有已披露高危 OAuth 漏洞；
- 需要自行回移安全修复；
- 最终仍要承担后续模型迁移；
- 没有生命周期收益。

### B. 升级 rmcp 2.2，交付稳定 OAuth

结论：**Go**

优势：

- 满足当前 Computer OAuth 目标；
- 通过 2025-11-25 client conformance 修复；
- 已修复当前 0.11 的关键安全漏洞；
- 支持要求中的所有授权方式；
- 相对 beta 保留更好的 legacy AS 兼容性；
- 避免把 2026 全协议迁移绑进 OAuth 项目。

代价：

- 仍需处理 25 个编译错误；
- 高层 API 对预注册 client 不如 beta 统一，需要 SMCP facade；
- StoredCredentials 无 issuer 字段，SMCP 必须自己做 issuer namespace；
- 需规避 discovery POST probe session leak。

### C. 立即升级 rmcp 3 beta

结论：**Defer，允许实验性 feature**

优势：

- OAuth API 更统一；
- issuer 绑定、scope 累积和 discovery provenance 更完整；
- 能开始适配 MCP 2026 现代 lifecycle。

风险：

- beta；
- 缺 issuer 的现实 Server 会回归；
- 全协议变更远超 OAuth 范围；
- 当前 SMCP resource subscription 与 rmcp 2026 lifecycle 存在直接冲突；
- 精确 wire 仍可能在 3 stable 前变化。

### D. 等 rmcp 3 stable 后再做任何 OAuth

结论：**No-Go**

- 继续停留在受安全漏洞影响的 0.11；
- 延迟已有明确用户价值的远程 OAuth；
- 稳定 OAuth 能力在 2.2 已足够。

## 8. 目标架构

### 分层

```text
Desktop / CLI / cloud host Flow Driver
  - 打开浏览器
  - 启动 loopback callback receiver，或维护 HTTPS Callback Gateway
  - 云端维护一次性 state -> tenant/CLI/Computer/bundle 路由
  - 展示授权状态
              │
              ▼
Computer public OAuth API
  - oauth_status
  - begin_oauth
  - complete_oauth / cancel_oauth
  - clear_oauth
              │
              ▼
OAuthCoordinator
  - discovery / registration / PKCE / callback
  - token restore / refresh / step-up
  - state machine / retry limits
  - rmcp compatibility facade
       │                              │
       ▼                              ▼
ExpiringStateStore             ScopedCredentialStore
PKCE/CSRF + TTL                bundle/resource/issuer/grant adapter
                                      │
                                      ▼
                            host OAuthCredentialStore
                       default: keyed process memory
                       injected: Keychain / DB / Vault
                                      │
                                      ▼
rmcp AuthClient<reqwest 0.13>
              │
              ▼
Streamable HTTP MCP Server
```

宿主 callback 路由表位于 SDK 外部，既不属于 `ExpiringStateStore`，也不进入
`OAuthCredentialStore`。

### 授权状态

当前公开类型为：

```rust
pub enum OAuthStatus {
    Unauthorized,
    AuthorizationPending,
    Authorized { scopes: Vec<String> },
    ReauthorizationRequired { required_scope: String },
    Error { message: String },
}
```

状态中不能包含 token、client secret、private key 或 PKCE verifier。

### SDK API

当前 `Computer` 公开 API：

```rust
impl<S: Session> Computer<S> {
    pub async fn oauth_status(
        &self,
        bundle_id: &BundleId,
    ) -> Result<OAuthStatus, OAuthError>;

    pub async fn begin_oauth(
        &self,
        bundle_id: &BundleId,
        request: OAuthBeginRequest,
    ) -> Result<OAuthLaunch, OAuthError>;

    pub async fn complete_oauth(
        &self,
        bundle_id: &BundleId,
        callback: OAuthCallback,
    ) -> Result<OAuthFlowOutcome, OAuthError>;

    pub async fn cancel_oauth(
        &self,
        bundle_id: &BundleId,
        cancellation: OAuthCancellation,
    ) -> Result<OAuthFlowOutcome, OAuthError>;

    pub async fn clear_oauth(&self, bundle_id: &BundleId) -> Result<(), OAuthError>;
}
```

语义：

- `connect()` 可恢复 token 并自动刷新；
- 首次需要用户参与时，返回 typed `AuthorizationRequired`；
- `begin_oauth()` 是 SDK 主动触发协议流程的入口；
- `complete_oauth()` 成功返回 `OAuthFlowOutcome::Authorized`；
- `cancel_oauth()` 返回带规范化原因及最终 `OAuthStatus` 的
  `OAuthFlowOutcome::Terminated`；
- 过期、state mismatch 与 issuer mismatch 分别返回
  `OAuthError::AuthorizationExpired`、`OAuthError::StateMismatch` 和
  `OAuthError::IssuerMismatch`；
- SDK 不调用 `open`、浏览器或 GUI API；
- Desktop/CLI 收到 `OAuthLaunch` 后负责用户交互；
- callback URL 必须同时校验 `state` 与可用的 `iss`。

### 配置模型

`HttpServerConfig` exposes OAuth options separately from negotiation policy:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub oauth: Option<OAuthOptions>

#[serde(default, rename = "authPolicy", skip_serializing_if = "Option::is_none")]
pub auth_policy: Option<HttpAuthPolicy>
```

An omitted policy is compatibility-aware: an existing `oauth` block remains proactive OAuth;
without that block the client is anonymous-first and performs automatic discovery. `authPolicy:
auto` makes `oauth` an optional override block, `oauth` selects proactive OAuth explicitly, and
`disabled` prevents OAuth negotiation.

实际序列化形态（camelCase）：

```yaml
oauth:
  # 可选；缺省为 HTTP MCP URL。仅当 canonical RFC 8707 resource 与传输 endpoint 不同时设置。
  resource: https://resource.example/canonical-mcp
  scopes: [files:read]
  mode:
    type: authorizationCode
    registration: preregistered
    clientId: my-client
```

`registration: dynamic` 表示 DCR；CIMD 使用
`registration: clientMetadataDocument` 并提供 `url`。

`redirect_uri` 不属于可序列化配置。宿主先绑定 callback listener，再把精确地址作为
`OAuthBeginRequest` 的运行期参数：

```rust
let listener = TcpListener::bind("127.0.0.1:0").await?;
let redirect_uri = format!("http://{}/callback", listener.local_addr()?);
let launch = computer
    .begin_oauth(
        &bundle_id,
        OAuthBeginRequest {
            redirect_uri,
            required_scope: None,
        },
    )
    .await?;
```

M2M `private_key_jwt`：

```yaml
oauth:
  scopes: [files:read]
  mode:
    type: clientCredentialsPrivateKeyJwt
    clientId: my-service
    privateKeyInput: oauth_private_key
    algorithm: RS256
```

所有 secret 字段必须通过现有 `SecretValueResolver`/input 引用解析，不能直接序列化明文。

`OAuthOptions.resource` 缺省时取 `HttpServerParameters.url`，保持旧配置兼容；显式值必须是绝对 URI
且不得包含 fragment。HTTP endpoint 继续承载 MCP 请求，effective resource 则统一进入
authorization request、token request 与 `OAuthCredentialKey`。两者跨 origin 时，endpoint 自定义 header
不得泄漏到 canonical resource origin。自动协商要求 effective resource 与 HTTP MCP endpoint 规范化后
一致，并把准入绑定到 challenge 中的精确 `resource_metadata` URL 及其声明的 endpoint；有意的
cross-resource 配置必须使用 proactive `authPolicy: oauth`，避免 endpoint A 的 challenge 错误准入
resource B 的授权元数据和 token。

### static headers 与 OAuth 优先级

1. 显式 proactive OAuth 禁止同时提供手写 `Authorization` header。
2. 存在静态 `Authorization` header 时保持静态鉴权；401/403 返回
   `StaticCredentialsRejected`，绝不静默回退 OAuth。
3. 没有静态凭据时匿名优先。只有合法 Bearer challenge 携带 `resource_metadata`，且 RFC 9728
   PRM 的精确 URL 已获取、其 resource 与当前 MCP endpoint 匹配，并且 RFC 8414/OIDC 授权服务器
   元数据包含合法 issuer 并通过校验，才创建 OAuth coordinator 并返回 `OAuthRequired`；rmcp 的
   legacy endpoint 推导不能作为自动准入证据。此后 `oauth_status` 与交互 flow 才可用。
4. Basic、Digest、未知 challenge、Bearer 缺 metadata、裸 401 和普通 403 分别返回稳定的
   `HttpAuthenticationError` 类别，不进入 OAuth。`403 insufficient_scope` 仅在已有 OAuth
   上下文中触发 step-up。
5. `authPolicy: disabled` 禁止自动协商；非 Authorization 自定义 headers 可与 OAuth 共存，
   但跨 origin redirect 不得携带这些 headers。

rmcp 2.2 的 Streamable HTTP 初始化错误当前只保留第一条独立 `WWW-Authenticate` header；若服务端
分别发送 Basic 与 Bearer 且 Basic 在前，SDK 无法恢复已丢失的 Bearer 字段。组合在同一 header 中的
多 challenge 可正确解析；独立多 header 需等待 rmcp 改用 `headers().get_all()` 后升级。

### 凭据存储

- SDK 默认使用进程内 `InMemoryOAuthCredentialStore`，不主动探测 OS Keychain、云密钥服务或
  其他持久化后端。
- 需要跨进程恢复时，宿主必须显式注入 `OAuthCredentialStore`：
  - Desktop 可注入独立 service/key namespace 的 Keychain adapter；
  - 多租户服务必须在宿主运行时把 tenant/principal 上下文绑定到 store，不能写进可序列化 MCP 配置。
- 持久 store 必须加密静态 value，且不能与普通 input ID 共用裸 key。
- 已显式注入的 store 是权威后端：后端失败返回 `OAuthCredentialStoreError`，SDK 不静默降级到内存。
- 同一个 store 实例会接收该 `Computer` 下所有 OAuth MCP 的异步 `load/save/delete`；实现必须支持并发调用。

Desktop/本地宿主装配形态（Keychain adapter 由宿主实现，不属于 SDK 默认策略）：

```rust
let store: Arc<dyn OAuthCredentialStore> =
    Arc::new(DesktopKeychainStore::new("com.example.app.oauth")?);
let computer = Computer::new(/* host arguments */)
    .with_oauth_credential_store(store);
```

Keychain adapter 应使用 `key.stable_id()` 作为非敏感 account/key，并把 `value` 作为 secret
写入；不得记录 `value` 或把 Keychain 不可用伪装成“无凭据”。

云端 DB/Vault adapter 必须在构造时捕获可信运行时上下文，而不是从 callback/config 取业务标识：

```rust
let store = TenantPrincipalOAuthStore::new(
    vault,
    authenticated_tenant,
    authenticated_principal,
);
let computer = Computer::new(/* host arguments */)
    .with_oauth_credential_store(Arc::new(store));
```

其后端 locator 应形如
`oauth/<trusted-tenant>/<trusted-principal>/<key.stable_id()>`；tenant/principal 来自已认证运行时
上下文，不能信任 callback 携带的 `bundle_id`、`computer_id` 或租户字段。

未调用 `with_oauth_credential_store` 时，`MCPServerManager::new()` 和 `Computer::new(...)`
均使用进程内 store；进程退出后不会恢复 OAuth 凭据。

SDK key 的完整隔离维度为：

```text
bundle_id
+ canonical protected resource
+ authorization server identity
+ grant/client/scopes fingerprint
+ record kind (credentials | issuer-index)
```

`OAuthCredentialKey::stable_id()` 对以上维度做带分隔符的 SHA-256，适合作为后端 locator；
host tenant/principal namespace 位于该 stable ID 外层。SDK value 有两种版本化 envelope：

```text
credentials:
  version
  mode_fingerprint
  serialized rmcp StoredCredentials

issuer-index:
  version
  issuers[]
```

- 先 discovery 得到 issuer，再加载对应 namespace。
- rmcp 2.2 自身不在 StoredCredentials 中保存 issuer，因此 SMCP envelope 必须验证 issuer，Authorization Server 迁移时不得复用旧 token/DCR client。
- CIMD client ID 可跨 issuer 保留；DCR client registration 与 token 必须按 issuer 隔离。
- `StoredCredentials` 保存 token 与 client ID；配置提供的 client secret/private key 不复制进
  CredentialStore，仍由 `SecretValueResolver` 在交换或恢复 client 时提供。

三类状态不得合并：

1. SDK `ExpiringStateStore`：PKCE verifier、CSRF state、generation、TTL；进程重启后重新发起授权。
2. 宿主 callback 路由表：`state -> tenant/CLI/Computer/bundle`，短期且一次性消费；只用于把
   Gateway callback 私密路由回原 coordinator。
3. `OAuthCredentialStore`：授权完成后的 token 凭据；不负责 callback 路由、Socket 或浏览器交互。

### 错误与事件

- 无 token、refresh 被拒绝：4006 + `ReauthorizationRequired`。
- `403 insufficient_scope`：4007 保持兼容，同时发出 `ReauthorizationRequired`；只有调用方选择后才开始浏览器流程。
- 非 scope 403：仍为 4007，不触发 step-up。
- OAuth discovery/registration/token exchange 使用 SDK 自有的 `OAuthProtocolError` typed
  cause，不依赖错误字符串，也不把 rmcp/provider 原文带入 `Display`、`Debug` 或错误链。
- scope upgrade 必须设置最大次数，防止循环授权。
- `Computer::subscribe_events()` 复用 runtime 有界 broadcast，发送
  `ComputerEvent::OAuthStatusChanged { bundle_id, status }`；同 bundle 同状态去重，覆盖 begin、complete、
  cancel、clear、refresh 失败、401 与 403 insufficient_scope。
- receiver `Lagged` 后按 `bundle_id` 调 `oauth_status()` 重同步；shutdown 终态后事件闸断。宿主不得用轮询
  补偿事件缺失。

## 9. 实施计划

### PR 1：rmcp 2.2 与模型迁移

范围：

- 精确锁定 `rmcp = "=2.2.0"`；
- 开启 `auth`、`auth-client-credentials-jwt`；
- 对齐直接 `reqwest = "0.13"`；
- 将 `sse-stream` 锁到兼容版本；
- 修复 25 个编译错误；
- 直接迁移到 rmcp 2.2 的扁平 `Resource`/content 模型，不保留旧 `Annotated`/`RawResource`
  construction shim；集中处理 builders、model conversions 与 non-exhaustive enums；
- 旧的 static headers 和 stdio 行为保持不变。

验收：

- workspace 全量 check/test；
- 当前 15 个鉴权与客户端基线测试全部通过；
- public API 变化形成 migration note；
- Cargo.lock 中 rmcp 精确为 2.2.0。

### PR 2：OAuth domain API 与状态机

范围：

- `OAuthOptions`、`AuthorizationStatus`、`AuthorizationLaunch`；
- `OAuthCoordinator`；
- Computer 查询、开始、完成、清除 API；
- typed events；
- callback state/issuer 校验；
- 状态不可泄漏 secret。

验收：

- 状态转移单测；
- 并发 begin 去重；
- callback 重放和错误 state 被拒绝；
- SDK 核心无浏览器依赖。

### PR 3：凭据与 secret

范围：

- host-injected `OAuthCredentialStore` 与默认 `InMemoryOAuthCredentialStore`；
- Desktop Keychain / 云端 tenant-principal store 由宿主 adapter 提供，不进入 SDK 默认策略；
- OAuth namespace/envelope；
- in-memory TTL `StateStore`；
- token refresh、rotation、clear；
- client secret/private key 接入 `SecretValueResolver`。

验收：

- 模拟进程重启后恢复 token；
- issuer 改变后旧 token/DCR credentials 不复用；
- 未注入持久 store 时行为明确为 session-only，持久 store 不可用时返回 typed error、不得静默误报恢复；
- 配置、Debug、tracing 不出现 secret。

### PR 4：全部 OAuth 方式

范围：

- authorization code：
  - auto priority；
  - preregistered public/confidential；
  - CIMD；
  - DCR；
- client credentials：
  - client_secret_post/basic；
  - `private_key_jwt`；
- refresh；
- insufficient_scope step-up。

验收矩阵：

| 场景 | 必须通过 |
|---|---|
| PRM via well-known | 是 |
| PRM via `WWW-Authenticate` | 是 |
| RFC8414 discovery | 是 |
| OIDC discovery | 是 |
| preregistered public | 是 |
| preregistered confidential | 是 |
| CIMD | 是 |
| DCR | 是 |
| PKCE S256 | 是 |
| callback state/issuer | 是 |
| client secret post/basic | 是 |
| private_key_jwt + local TLS fixture | 是 |
| refresh + rotation | 是 |
| 403 scope accumulation | 是 |
| credential restart restore | 是 |
| issuer migration | 是 |

### PR 5：HttpMCPClient 集成与回归

范围：

- 将 `AuthClient` 注入 Streamable HTTP transport；
- Bearer token 覆盖 POST/GET/DELETE 和 reinitialize；
- 401/403 映射；
- static header 兼容；
- discovery probe session cleanup workaround；
- 文档与 Desktop/CLI 集成示例。

验收：

- 无 OAuth Server 行为不变；
- static header Server 行为不变；
- OAuth Server 从未授权到工具调用完成；
- refresh 后继续调用；
- discovery 不遗留 session；
- token 不出现在 URL、日志或错误。

### 后续 PR：rmcp 3 stable / MCP 2026

触发条件：

- rmcp 3 stable 发布；
- #1047、#1048 等兼容问题有明确结论；
- MCP 2026 final schema 固定。

单独处理：

- 2026 protocol selection；
- stateless lifecycle；
- standard headers；
- `subscriptions/listen`；
- required request metadata；
- resultType/MRTR/extension 变化。

## 10. 发布门禁

全部满足才可发布：

- 不再依赖受影响的 rmcp 0.11；
- OAuth acceptance matrix 100% 通过；
- full workspace tests 通过；
- 真实 HTTPS IdP 或 TLS fixture 至少验证一次 `private_key_jwt`；
- 没有明文 token/secret 日志；
- 默认 session-only 与 host-injected persistent store 的行为、失败语义明确；
- discovery probe 不产生孤儿 session；
- public API breaking change 与版本升级说明完成；
- Desktop/CLI 能只依赖公开 SDK API 完成浏览器授权，不访问 rmcp 内部类型。

## 11. 回滚方案

- OAuth 通过 Cargo feature 和 `HttpServerParameters.oauth` 双重门控。
- static headers 路径保持原实现，可在 OAuth 故障时按 Server 配置回退。
- rmcp 精确版本锁定，禁止自动漂移到 3 beta。
- OAuth credential envelope 带版本号；回滚时可以保留或显式清除，不读取未知 schema。
- PR 按兼容层、状态机、存储、flows、集成拆分，可逐层回滚。

## 12. 最终结论

本需求值得做，且当前已有足够证据进入实现：

- **Go：rmcp 2.2 + 完整 MCP 2025 OAuth + SDK OAuth facade。**
- **Defer：rmcp 3 beta 作为生产默认依赖。**
- **Experiment：在独立 feature 中持续验证 MCP 2026，待 rmcp 3 stable 后切换。**

这条路线能够立即消除 0.11 的安全债务、完成 Computer 的远程 OAuth 目标，又不会把尚未稳定的 MCP 2026 全协议迁移强行耦合进同一交付。
