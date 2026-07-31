# 场景：oauth-atlassian（Atlassian Rovo MCP 浏览器自动化 OAuth UAT）

## 测试目标

以真实上层应用视角验证 `smcp-computer` 对 Atlassian Rovo MCP 的 OAuth 2.1
Authorization Code + PKCE 流程：SDK 发起授权，上层驱动自动打开浏览器并监听 loopback
回调；用户仅在需要时完成登录、SSO 或 MFA，浏览器自动化选择站点、审阅并同意后，完成
MCP `initialize`、`tools/list` 和一个只读资源查询。

本场景还在同一进程内以共享 `InMemoryOAuthCredentialStore` 验证 manager 重建恢复和
`clear_oauth`，并引用自动化守护验证刷新、过期 state 与 issuer 不匹配。它不验证宿主的
跨进程持久化实现。API token/service account smoke 与本交互式 OAuth 场景严格分开，
不能互相替代。

此外，自动化前置通过纯测试 harness 验证 headless 云端宿主契约；它不依赖真实 TFClient、
生产 Socket 服务或公网 callback gateway，不能替代本地浏览器 consent UAT。

## 类型与执行策略

- **类型**：外部真实服务 + 浏览器自动化 consent；账号登录、SSO、MFA 可由用户完成。
- **仅显式执行**：`$uat oauth-atlassian`。
- **禁止默认执行**：默认 CI、fork PR、`$uat` 全量套件不得运行本场景。
- **服务端点**：`https://mcp.atlassian.com/v1/mcp/authv2`。
- **授权方式**：DCR + Authorization Code + PKCE S256。
- **只读 scopes**：`read:me read:account read:jira-work offline_access`。
- **只读工具**：`getAccessibleAtlassianResources`；不要求预建 Jira 项目。

`getAccessibleAtlassianResources` 本身只要求 `read:me read:account`，但 Atlassian 的 DCR
consent UI 在只有 common scopes 时不会渲染 workspace 选择器，同时保持 **接受**为 disabled。
因此本场景增加最小 Jira 产品只读 scope `read:jira-work`，用于触发站点/产品 consent 步骤；
驱动仍只调用上述无写入工具。该 scope 可读取测试账号本来有权访问的 Jira 内容，权限范围
大于资源列表调用本身；应优先使用专用测试账号/站点。代码会将服务端实际授予的 scopes 与
上述四项做顺序无关的精确 allowlist 校验；缺少、增加或替换任何 scope 都会先清除凭据再 FAIL。

## 前置条件

1. 有权访问至少一个 Atlassian Cloud 站点的账号；无需创建测试项目。
2. 组织管理员允许 Atlassian Rovo MCP OAuth 和 Read 权限。
3. 现代浏览器可用，终端允许启动默认浏览器；有可附着当前页面并按 role/name 操作正常
   可见控件的浏览器自动化工具。
4. 默认 callback 为 `http://127.0.0.1:3334/callback`：
   - 端口空闲；
   - 本机防火墙允许 loopback；
   - 如组织启用了 redirect/domain allowlist，管理员已允许该地址。
5. 本 UAT 只使用 SDK 的进程内存 OAuth store；无需也不得访问 OS Keychain、系统凭据库或
   token 文件。驱动退出后凭据自然销毁。
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
- OAuth 凭据只存在于当前 UAT 进程内存；不得改用 Keychain、Secret Service、Credential
  Manager 或文件持久化来延长生命周期。

### Consent 卡住时的浏览器诊断

当 consent 页缺少站点/权限选择器、**接受**按钮持续灰色，或驱动一直等待 callback 时，
使用已连接的浏览器工具做**只读、脱敏**诊断。浏览器工具只有在下述授权守卫全部通过后，
才能通过正常可见控件选择站点、审阅权限并点击 **Allow/接受**；不得绕过禁用按钮。

诊断要求：

1. 优先附着到当前浏览器会话，按 hostname + pathname 在工具内部定位
   `auth.atlassian.com/consent`；不得调用会把完整 tab URL 原样输出到会话的页面枚举，
   不得输出 query、fragment 或页面 source。
2. DOM 只记录：
   - `document.readyState`；
   - 站点选择相关 `select` / `combobox` / `radio` 是否存在、是否可见；
   - **接受**按钮的原生 `disabled` / `aria-disabled` 状态；
   - 页面步骤文案。账号菜单、邮箱、头像、隐藏 input 值一律排除。
3. 网络只记录去掉 query/fragment 后的 `hostname + pathname + method + status`。
   `/consent/info` 如需检查，只输出字段结构、数组长度与布尔值；字符串值、workspace ID、
   cloudId、context、state、code、token 一律脱敏。
4. 控制台只记录 error/warning，并先脱敏完整 URL、长随机值、账号标识；禁止保存 HAR、
   wire dump、截图地址栏或响应原文。
5. 等待 2-3 秒后刷新一次并重复检查，作为 FAIL 二次复验。完成证据采集后终止驱动并确认
   进程退出；内存凭据随进程销毁，不为凑满 callback timeout 而空等。

判定参考：

| 证据 | 判定 |
|---|---|
| 站点选择器存在，选择站点后 **接受**启用 | 页面行为正常，继续浏览器自动化 consent |
| 独立连接能列出站点，但 consent 响应无 workspace | 账号/组织策略或 Atlassian consent 后端阻塞 |
| consent 响应有 workspace，DOM 无选择器且 **接受**原生 disabled | Atlassian consent 前端/当前 scopes 兼容问题，不归因于 SDK callback/PKCE |
| `/consent/info` 非 2xx、请求失败或页面 JS exception | Atlassian consent 加载失败 |

若已有独立 Atlassian 连接，可只读调用 `getAccessibleAtlassianResources` 交叉确认账号确实能
访问 Cloud 站点；该连接使用的是另一份凭据，只能作为前置条件证据，不能替代本次 OAuth PASS。

### 浏览器自动化授权守卫

浏览器工具内部定位 consent 页后，必须依次执行以下守卫；只输出 PASS/FAIL 布尔结论，不输出
完整 URL、query、账号标识、workspace ID 或权限响应原文：

1. 页面显示的客户端名称是 `A2C SMCP Rust SDK UAT`。
2. 页面显示的 callback 与当前端口对应，且 scheme/host/path 严格为
   `http://127.0.0.1:<port>/callback`。
3. 页面存在可见的站点 `combobox` 和权限审阅入口，**接受**不是 disabled。
4. 打开权限审阅控件，确认只包含账号读取、Jira 读取和 offline access 等只读意图；出现
   create/edit/delete/write/admin 等写入意图立即 FAIL，禁止接受。
5. workspace 只有一个时自动选择或保留该站点；有多个时必须由
   `A2C_OAUTH_UAT_SITE=<精确站点 hostname>` 指定，缺失则 FAIL
   `stage=browser-site-ambiguous`，不得默认选第一个。
6. 关闭权限审阅浮层后，用正常可见的 **接受/Allow** 按钮点击一次；不得执行脚本移除
   `disabled`、直接调用 form submit、请求授权端点或构造 callback。
7. 点击后由终端 loopback callback 与脱敏 `UAT_RESULT` 判定成功；浏览器页面跳转本身不算 PASS。

若需要登录、SSO 或 MFA，浏览器工具停在身份认证页面并提示用户完成；身份认证结束后由工具
继续上述全部 consent 点击，不能把站点选择或 **接受**转交用户。

## 自动化前置验收

以下测试不需要浏览器、账号或真实 Atlassian 项目，必须先通过：

```bash
cargo test -p smcp-computer --lib oauth::tests
cargo test -p smcp-computer --test mock_server_integration
cargo test -p smcp-computer --example oauth_atlassian_uat
```

覆盖范围包括 PKCE、预注册/CIMD/DCR、Client Credentials secret/`private_key_jwt`、
刷新、共享内存 store 的 manager 重建恢复与清除、401、403 `insufficient_scope`、并发 begin、过期 callback、
state/issuer 校验、重复 callback 参数拒绝、deny/cancel/timeout 清理、bundle/resource/issuer
隔离、默认内存 store、真实 HTTP/SSE 边界及敏感 tracing/Debug 守护；
云端 harness 另覆盖稳定 HTTPS redirect URI、一次性 opaque state 路由、原 CLI coordinator
完成 callback、目标用户/目标 CLI 私有投递，以及 callback code 不进入 UI 广播。

## 测试步骤

### AT-01：进程内存凭据隔离

驱动每次启动都新建 `InMemoryOAuthCredentialStore`，并在打开浏览器前确认状态是
`Unauthorized`。不得读取旧进程凭据、OS Keychain、系统凭据库或 token 文件；因此不需要
预清理命令。

### AT-02：真实浏览器自动化授权

```bash
cargo run -q -p smcp-computer --example oauth_atlassian_uat -- run
```

浏览器自动化操作：

1. 驱动自动打开默认浏览器。
2. 工具附着到当前 consent 页；如需登录、SSO 或 MFA，只暂停等待用户完成身份认证。
3. 工具执行[浏览器自动化授权守卫](#浏览器自动化授权守卫)。
4. 工具选择唯一站点或 `A2C_OAUTH_UAT_SITE` 指定的站点。
5. 工具审阅权限后，通过正常可见控件点击 **Allow/接受**。
6. 浏览器显示 callback 已接收后，由工具关闭失效页面。

若站点/权限选择器未出现或 **接受**持续灰色，按
[Consent 卡住时的浏览器诊断](#consent-卡住时的浏览器诊断) 收集脱敏证据并二次复验；
不得让用户代点，也不得用脚本移除 `disabled`、直接提交表单或构造 callback。

终端预期只出现脱敏结果：

```text
UAT_RESULT: PASS browser-opened
UAT_RESULT: PASS authorized
UAT_RESULT: PASS initialize
UAT_RESULT: PASS tools-list count=<正整数>
UAT_RESULT: PASS read-only-resource-query
UAT_RESULT: PASS phase=authorize
UAT_RESULT: PASS manager-rebuild-restored
UAT_RESULT: PASS initialize
UAT_RESULT: PASS tools-list count=<正整数>
UAT_RESULT: PASS read-only-resource-query
UAT_RESULT: PASS phase=manager-rebuild
UAT_RESULT: PASS phase=clear status=Unauthorized
UAT_RESULT: PASS phase=run
```

### AT-03：MCP 路径验收

AT-02 驱动必须自行完成以下真实 Streamable HTTP 路径，不能用 mock trait 代替：

1. OAuth 后 MCP `initialize`；
2. `tools/list` 并发现 `getAccessibleAtlassianResources`；
3. 调用该无写入工具；
4. 不输出返回的站点详情，只报告 PASS。

AT-02 全部 PASS 即 AT-03 PASS。

### AT-04：共享内存 store 的 manager 重建恢复

AT-02 在完成第一次只读调用后关闭第一个 manager，但保留同一进程内的共享
`Arc<InMemoryOAuthCredentialStore>`；随后重建第二个 manager，必须输出
`manager-rebuild-restored`，且不再次打开浏览器，并再次完成真实 MCP 只读路径。

该公开状态只能证明凭据恢复与复用，不能区分请求使用了原 token 还是 refresh token。
因此 UAT 报告不得声称 refresh 已触发，也不得为等待过期而轮询；refresh 由 AT-06 的
可控自动化测试独立验收。该用例也不声称验证 OS 进程退出后的宿主持久化。

### AT-05：清除授权

AT-02 的第二个 manager 完成只读调用后必须在同一进程内执行 `clear_oauth`，确认状态为
`Unauthorized`，再关闭 manager。无单独 `clear` 或 `resume` 命令；即使驱动异常退出，
凭据也只存在于进程内存并随之销毁。

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
unset A2C_OAUTH_UAT_PORT
```

浏览器工具关闭本次已失效的 callback/consent 页面，并确认没有
`oauth_atlassian_uat` 进程残留。无需清理 OS 凭据。

## UAT 报告

| 用例 | 结果 | 备注 |
|---|---|---|
| AT-01 进程内存凭据隔离 | PASS/FAIL | 不访问 OS Keychain/文件 |
| AT-02 浏览器 OAuth consent | PASS/FAIL | 不附授权 URL/callback |
| AT-03 initialize/tools/list/只读调用 | PASS/FAIL | 只记录工具数 |
| AT-04 manager 重建恢复 | PASS/FAIL | 共享内存 store；不声称跨进程持久化或 refresh |
| AT-05 clear 后 Unauthorized | PASS/FAIL | |
| AT-06 refresh/state/issuer/过期 callback | PASS/FAIL | 自动化测试名 |
| AT-07 headless 云端宿主契约 | PASS/FAIL | 自动化 harness，不依赖产品环境 |
| API token smoke | SKIP/PASS/FAIL | 必须独立报告 |

报告末尾确认：未打印或落盘 token、code、state、secret、private key、完整授权 URL。
