# 场景：oauth-atlassian（Atlassian Rovo MCP 人工 OAuth UAT）

## 测试目标

以真实上层应用视角验证 `smcp-computer` 对 Atlassian Rovo MCP 的 OAuth 2.1
Authorization Code + PKCE 流程：SDK 发起授权，上层驱动自动打开浏览器并监听 loopback
回调，用户登录、选择站点并同意后，完成 MCP `initialize`、`tools/list` 和一个只读资源查询。

本场景还验证凭据跨进程恢复、`clear_oauth`，并引用自动化守护验证刷新、过期 state 与
issuer 不匹配。API token/service account smoke 与本交互式 OAuth 场景严格分开，不能互相替代。

此外，自动化前置通过纯测试 harness 验证 headless 云端宿主契约；它不依赖真实 TFClient、
生产 Socket 服务或公网 callback gateway，不能替代本地浏览器 consent UAT。

## 类型与执行策略

- **类型**：外部真实服务 + 浏览器人工 consent。
- **仅显式执行**：`$uat oauth-atlassian`。
- **禁止默认执行**：默认 CI、fork PR、`$uat` 全量套件不得运行本场景。
- **服务端点**：`https://mcp.atlassian.com/v1/mcp/authv2`。
- **授权方式**：DCR + Authorization Code + PKCE S256。
- **最小 scopes**：`read:me read:account offline_access`。
- **只读工具**：`getAccessibleAtlassianResources`；不要求预建 Jira 项目。

## 前置条件

1. 有权访问至少一个 Atlassian Cloud 站点的账号；无需创建测试项目。
2. 组织管理员允许 Atlassian Rovo MCP OAuth 和 Read 权限。
3. 现代浏览器可用，终端允许启动默认浏览器。
4. 默认 callback 为 `http://127.0.0.1:3334/callback`：
   - 端口空闲；
   - 本机防火墙允许 loopback；
   - 如组织启用了 redirect/domain allowlist，管理员已允许该地址。
5. OS credential vault 可用，以验证跨进程凭据恢复。SDK 默认 OAuth store 是进程内存；
   本 UAT 驱动作为宿主显式注入专用 `a2c-computer-oauth-uat` vault adapter。vault 不可用时
   驱动以 `stage=credential-vault-unavailable` 失败，不会静默退化后再误报 AT-04 PASS。
6. 编译 UAT 驱动：

   ```bash
   cargo build -p smcp-computer --example oauth_atlassian_uat
   ```

如 3334 被占用，可选择其他空闲端口，并让管理员同步允许对应 callback：

```bash
export A2C_OAUTH_UAT_PORT=43334
```

## 安全约束

- 驱动仅输出脱敏的 `UAT_RESULT: PASS/FAIL stage=...`。
- 禁止打印、复制到报告或落盘：
  - authorization URL；
  - authorization code、state；
  - access/refresh token；
  - client secret、private key。
- 失败报告只记录 `stage`、退出码和不含敏感值的环境事实。
- 不开启 HTTP wire dump，不把浏览器地址栏或 callback query 截图附到 Issue。

## 自动化前置验收

以下测试不需要浏览器、账号或真实 Atlassian 项目，必须先通过：

```bash
cargo test -p smcp-computer --lib oauth::tests
cargo test -p smcp-computer --test mock_server_integration
cargo test -p smcp-computer --example oauth_atlassian_uat
```

覆盖范围包括 PKCE、预注册/CIMD/DCR、Client Credentials secret/`private_key_jwt`、
刷新、凭据恢复与清除、401、403 `insufficient_scope`、并发 begin、过期 callback、
state/issuer 校验、重复 callback 参数拒绝、deny/cancel/timeout 清理、bundle/resource/issuer
隔离、默认内存 store、真实 HTTP/SSE 边界及敏感 tracing/Debug 守护；
云端 harness 另覆盖稳定 HTTPS redirect URI、一次性 opaque state 路由、原 CLI coordinator
完成 callback、目标用户/目标 CLI 私有投递，以及 callback code 不进入 UI 广播。

## 测试步骤

### AT-01：清理 UAT 专属凭据

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- clear
```

预期：

```text
UAT_RESULT: PASS phase=clear status=Unauthorized
```

该操作只清理显式注入 store 中 bundle ID `oauth-atlassian-uat`、上述 Atlassian resource
和当前 OAuth 模式对应的凭据槽；即使其他 bundle 共用同一 store 与 resource，也不受影响。

### AT-02：真实浏览器授权

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- authorize
```

人工操作：

1. 驱动自动打开默认浏览器。
2. 登录 Atlassian。
3. 选择可访问的站点。
4. 审阅授权范围并选择 **Allow**。
5. 浏览器显示 callback 已接收后关闭页面。

终端预期只出现脱敏结果：

```text
UAT_RESULT: PASS browser-opened
UAT_RESULT: PASS authorized
UAT_RESULT: PASS initialize
UAT_RESULT: PASS tools-list count=<正整数>
UAT_RESULT: PASS read-only-resource-query
UAT_RESULT: PASS phase=authorize
```

### AT-03：MCP 路径验收

AT-02 驱动必须自行完成以下真实 Streamable HTTP 路径，不能用 mock trait 代替：

1. OAuth 后 MCP `initialize`；
2. `tools/list` 并发现 `getAccessibleAtlassianResources`；
3. 调用该无写入工具；
4. 不输出返回的站点详情，只报告 PASS。

AT-02 全部 PASS 即 AT-03 PASS。

### AT-04：进程重启后凭据恢复

AT-02 命令已经退出，另起一个新进程：

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- resume
```

预期不打开浏览器，直接输出：

```text
UAT_RESULT: PASS credentials-restored
UAT_RESULT: PASS initialize
UAT_RESULT: PASS tools-list count=<正整数>
UAT_RESULT: PASS read-only-resource-query
UAT_RESULT: PASS phase=resume
```

该公开状态只能证明凭据恢复与复用，不能区分请求使用了原 token 还是 refresh token。
因此人工报告不得声称 refresh 已触发，也不得为等待过期而轮询；refresh 由 AT-06 的
可控自动化测试独立验收。

### AT-05：清除授权

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- clear
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- resume
```

预期第一条 PASS；第二条以 `stage=status-not-authorized` 失败，且不发送已清除 token、
不自动打开浏览器。

### AT-06：state / issuer / cancel / timeout callback

由自动化测试执行，不对真实 Atlassian consent 制造伪 callback：

```bash
cargo test -p smcp-computer --lib \
  lifecycle_generation_protects_concurrent_pending_expiry_and_refresh
```

预期测试 PASS，证明并发 begin 去重、过期 state、旧 state、错误 issuer 均不能写入凭据。
`mock_server_integration` 与 UAT example 的自动化测试另证明 deny/cancel/timeout 会删除 pending
state，且重复 `code`/`state`/`error`/`iss` 或同时出现 `code` 与 `error` 的 callback 不会被接受。

### AT-07：headless 云端宿主契约

由不依赖产品环境的自动化 harness 执行：

```bash
cargo test -p smcp-computer --test mock_server_integration \
  test_cloud_flow_driver_routes_callback_privately_to_original_cli
```

预期测试 PASS，证明宿主使用稳定 HTTPS callback URI；callback gateway 只按一次性 opaque
state 路由，不信任 callback 中的 tenant/CLI/Computer/bundle 标识；授权链接仅发送给目标用户，
authorization code 仅发送给原 CLI coordinator，且重放或过期 callback 不会投递。

## API token / service account smoke（独立报告）

API token smoke 不是本 OAuth UAT 的前置或替代品。只有受保护 CI secrets 和组织管理员明确
启用 API token authentication 时才可另行运行；无 secrets 时必须记录：

```text
SKIP api-token-smoke: protected secrets unavailable
```

不得把 API token 放进命令行、仓库配置、测试输出或本场景报告。

## 清理

测试结束必须执行：

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- clear
unset A2C_OAUTH_UAT_PORT
```

## UAT 报告

| 用例 | 结果 | 备注 |
|---|---|---|
| AT-01 UAT 凭据清理 | PASS/FAIL | 仅记录 stage |
| AT-02 浏览器 OAuth consent | PASS/FAIL | 不附授权 URL/callback |
| AT-03 initialize/tools/list/只读调用 | PASS/FAIL | 只记录工具数 |
| AT-04 跨进程恢复 | PASS/FAIL | 不声称 refresh 是否触发 |
| AT-05 clear 后 Unauthorized | PASS/FAIL | |
| AT-06 refresh/state/issuer/过期 callback | PASS/FAIL | 自动化测试名 |
| AT-07 headless 云端宿主契约 | PASS/FAIL | 自动化 harness，不依赖产品环境 |
| API token smoke | SKIP/PASS/FAIL | 必须独立报告 |

报告末尾确认：未打印或落盘 token、code、state、secret、private key、完整授权 URL。
