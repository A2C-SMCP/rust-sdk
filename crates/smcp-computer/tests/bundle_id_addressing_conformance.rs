//! #142 / R5②+F7 —— BundleID **寻址行为**跨 SDK 对拍（`bundle_id_conformance.rs` 的另一半）。
//!
//! # 为什么要有这个文件
//!
//! `bundle_id_conformance.rs` 对拍的是**生成算法**（给定 config → 期望 bundle_id 的逐字节向量）。而
//! server_name-as-identity 类缺陷**全部发生在寻址行为上**——恰是没有对拍的那一半（协议
//! [conformance-tests.md §2.0-2](../../../../a2c-smcp-protocol/docs/specification/computer-management/conformance-tests.md)）。
//!
//! 更要命的是**缺省派生**：`derive_bundle_id` 缺省即 `normalize_name(name)`（`mcp_clients/bundle_id.rs:151`），
//! 于是默认路径下 display name 与 bundle_id **逐字重合**，把身份裂缝整个盖住——「按 name 取键」与「按
//! bundle_id 取键」两种实现在这种夹具下**双双通过**，零鉴别力。故本文件全部夹具遵 **§2.0-1 取值分叉条款**：
//! display 名一律带 `.`／空格／括号，令 `name ≠ bundle_id`。
//!
//! # 与 python-sdk 的对拍关系
//!
//! 逐条对应 python-sdk `tests/unit_tests/computer/test_python_rust_alignment.py`（`2fc8428`，#150）的
//! R5② 向量与 `tests/unit_tests/computer/settings/test_mcp_dependency_model.py` 的四景向量。SDK 方法名不强制
//! 对拍，**wire / 寻址语义一致即可**。
//!
//! # 变异验证（守卫非永真）
//!
//! rust 生产侧七个寻址面在本批开工前即全部正确（详见 #142 收尾说明），故本文件的向量是**回归栅栏**而非
//! 红灯。为证明它们不是恒真断言，每条都做过变异验证——把对应生产不变量反向改一次，确认必红后还原。
//! 具体变异点逐条记在各测试的 `变异验证：` 行。

use std::collections::HashMap;
use std::time::Duration;

// #149：共享 Streamable HTTP mock（非 cli 门控，`test-ws` 无 cli 下亦可编译）。
#[path = "common/streamable_mock.rs"]
mod streamable_mock;
use streamable_mock::{spawn_streamable_mock, MockOpts};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map};
use tempfile::TempDir;

use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{BundleId, ServerName};
use smcp_computer::mcp_clients::{
    HttpServerConfig, HttpServerParameters, MCPServerConfig, MCPServerManager, StdioServerConfig,
    StdioServerParameters,
};
use smcp_computer::settings::scope::EnvMap;
use smcp_computer::settings::store::{update_installed_plugins, update_installed_plugins_intent};
use smcp_computer::settings::{
    InstalledPluginRecord, McpHookError, McpInstallHooks, UninstallOptions,
};

// ═══════════════════════════════════════════════════════════════════════════════
// 夹具（§2.0-1：display name 与 bundle_id MUST 取值分叉）
// ═══════════════════════════════════════════════════════════════════════════════

/// display 名带 `.` 与空格与括号 → `normalize_name` 映射为 `_`、折叠、裁首尾。
const AUTH_NAME: &str = "auth.srv (display)";
/// `normalize_name("auth.srv (display)")` == `auth_srv_display`（与 name 取值分叉）。
const AUTH_BID: &str = "auth_srv_display";

/// R5② 共存夹具：**同一** display 名，两条**显式异** bundle_id。
const DUP_NAME: &str = "dup.disp";
const DUP_BID_A: &str = "dup-a";
const DUP_BID_B: &str = "dup-b";

fn bid(s: &str) -> BundleId {
    BundleId::try_from(s.to_string()).expect("夹具 bundle_id 须合法")
}

/// 一条 stdio 配置，可注入**显式** bundle_id（令 display 名与身份可控地分叉）。
fn stdio_cfg(name: &str, command: &str, explicit_bid: Option<&str>) -> MCPServerConfig {
    let mut cfg = MCPServerConfig::Stdio(StdioServerConfig::new(
        name,
        StdioServerParameters {
            command: command.to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
        },
    ));
    cfg.set_bundle_id(explicit_bid.map(bid));
    cfg
}

// ═══════════════════════════════════════════════════════════════════════════════
// Mock MCP Server：经共享 [`streamable_mock`] 起一台握手/列表/resources 放行、仅 tools/call 返 403 的 mock。
// ═══════════════════════════════════════════════════════════════════════════════
//
// 用 403（而非 401）是刻意的：rmcp 在 401 带 `WWW-Authenticate` 时会短路成 `AuthRequired`，其 Display 为
// 字面量 `"Auth required"`、**不含**任何状态码判别子串 ⇒ `classify_auth_error` 返 `None`（该缺陷另线跟踪，
// 见 protocol Discussion #34）。本文件要锁的是 **bundle_id 身份传递**，不应被那条正交缺陷挡住。
// `MockOpts::default()` 逐字复现原 `mock_handler`（403 / 无 WWW-Authenticate / 有 resources/list）。

/// 起一台连上 mock 的 manager，server display 名为 [`AUTH_NAME`]（缺省派生 ⇒ bundle_id = [`AUTH_BID`]）。
async fn manager_on_mock() -> MCPServerManager {
    let port = spawn_streamable_mock(MockOpts::default()).await;
    let cfg = MCPServerConfig::Http(HttpServerConfig::new(
        AUTH_NAME,
        HttpServerParameters {
            url: format!("http://127.0.0.1:{port}"),
            headers: HashMap::new(),
        },
    ));
    let manager = MCPServerManager::new();
    manager.initialize(vec![cfg]).await.unwrap();
    // time-box：驱动真实 socket 的路径 MUST 有超时保护，否则 rmcp 行为回归时 CI 会**无限挂**而非报错
    // （姊妹文件 `auth_error_real_transport.rs` 同款约定）。
    //
    // 阈值取 60s 仅为「无限挂」底线保护——实测握手在亚秒级完成（本套件 7 测试含多次 start_all，总用时 <1s）。
    // 此前注释称「握手实测 20–50s、GET 405 × `CONNECT_TIMEOUT_SECS=30` 走满超时」系**误归因**（#149 第 3 项已查）：
    // rmcp 0.11.0 在 GET 返 405 时映射为 `ServerDoesNotSupportSse`
    // （`transport/common/reqwest/streamable_http_client.rs:45-46`），worker 捕获后**立即降级**跳过 GET 流
    // （`transport/streamable_http_client.rs:429`），不消耗任何超时——故 GET 405 非延迟来源。
    tokio::time::timeout(Duration::from_secs(60), manager.start_all())
        .await
        .expect("HANG: start_all 未在 60s 内完成（握手挂起，非仅仅慢）")
        .unwrap();
    manager
}

// ═══════════════════════════════════════════════════════════════════════════════
// B1 + B4：`get_config` 身份键 + F7 真实构造路径
// ═══════════════════════════════════════════════════════════════════════════════

/// **R5② 寻址对拍 + F7 真实构造路径**：真实 `Computer`、**构造期 server 集合为空**、两条**同 display 名 +
/// 显式异 bundle_id** 的 server 全部经**运行期挂载**进入 ⇒ 运行期投影按各自 bundle_id **各自保留**（不塌陷）。
///
/// English: two servers sharing a display name but carrying explicitly distinct bundle_id coexist in the
/// runtime projection, each keyed by its own bundle_id; built through the real construction path.
///
/// 三条协议条款一次兑现：
/// - **§2.0-2**（异 id 同名共存）：若寻址误按 display 名，两条塌成一条，`len == 2` 即红；
/// - **§2.0-3**（真实构造路径）：构造期 `mcp_servers = None`（**空集**）、server 全部运行期 `mount_server`
///   挂入——这正是条款点名的形态。桩测试爱把「构造期集合有内容」这个**生产中恒假**的前提固化为真
///   （真实 CLI 构造期集合恒空），从而把缺陷焊死在测不到的地方；
/// - **§2.0-1**（夹具分叉）：`DUP_NAME` 含 `.`，与两个显式 bundle_id 三者互不相等。
///
/// 对应 python：`test_alignment_same_name_distinct_bundle_id_coexist`（transient `amount_server` ×2）。
///
/// 变异验证：`Computer::get_server_status` 的 `.0` 若改回 `config.name()`（或 manager 的 `servers_config`
/// 改按 name 为键），两条塌陷 ⇒ `len == 2` 与 bundle_id 集合断言双双转红（已实测）。
#[tokio::test]
async fn get_config_keys_by_bundle_id_same_name_distinct_ids_coexist_f7() {
    // F7：构造期 server 集合**为空**（`mcp_servers = None`）。Preboot mount 只登记 raw desired
    // state；boot 才建立 Manager 投影。`auto_connect=false` 保证不会拉起真实子进程。
    let td = TempDir::new().unwrap();
    let computer = Computer::new("c", SilentSession::new("s"), None, None, false, false)
        .with_skill_home(td.path().join("skills"))
        .with_blob_cache_root(td.path().join("blob"));

    // 运行期挂载两条：同 display 名、显式异 bundle_id（transient，不落盘）。
    computer
        .mount_server(stdio_cfg(DUP_NAME, "/bin/echo", Some(DUP_BID_A)))
        .await
        .unwrap();
    computer
        .mount_server(stdio_cfg(DUP_NAME, "/bin/cat", Some(DUP_BID_B)))
        .await
        .unwrap();
    computer.boot_up().await.unwrap();

    let status = computer.get_server_status().await;

    // ① 两条各自保留（按 name 寻址会塌成 1）。
    assert_eq!(
        status.len(),
        2,
        "同 display 名 + 显式异 bundle_id MUST 各自共存（去重/寻址键 = 身份，非 display 名）"
    );

    // ② 身份键集合 == 两个显式 bundle_id。
    let ids: std::collections::HashSet<&str> =
        status.iter().map(|(b, _, _, _)| b.as_str()).collect();
    assert_eq!(
        ids,
        [DUP_BID_A, DUP_BID_B].into_iter().collect(),
        "运行期投影 MUST 按各自 bundle_id 为身份键"
    );

    // ③ 两识别空间分账：display 名合法碰撞、两条都仍叫 DUP_NAME。
    assert!(
        status.iter().all(|(_, n, _, _)| n == DUP_NAME),
        "display 名碰撞合法且 MUST 原样保留（非身份）"
    );
    computer.shutdown().await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════════
// B2：`get_resources(mcp_server=…)` 按 bundle_id 寻址，display 名 → 4014
// ═══════════════════════════════════════════════════════════════════════════════

/// **寻址对拍**：`list_resources` 收 **bundle_id**；对**同一台已连接**的 server 传 display 名 → 4014
/// （`McpServerNotFound`），零 name 回退。
///
/// English: resources are addressed by bundle_id; passing the display name of the *same connected* server
/// yields 4014 — no name fallback.
///
/// **鉴别力来自「同一台已连接的 server」**：若拿一台没连上的 server 来测，正确 id 与 display 名会**双双**
/// 返 4014（都查不到 `active_clients`）⇒ 该测试无法分辨实现是否有 name 回退，又是一处假绿。故此处必须先
/// `start_all()` 让客户端真正就位，再对拍两种 token。
///
/// 变异验证：`manager.list_resources` 若在 bundle_id 未命中时补一段「再按 display 名找一遍」的回退，
/// `Err` 断言立即转红（已实测）。
#[tokio::test]
async fn get_resources_addresses_by_bundle_id_display_name_yields_4014() {
    let manager = manager_on_mock().await;

    // ① 正例：按 bundle_id 寻址命中（证明这台 server 确实已连接、资源可达）。
    let (resources, _) = manager
        .list_resources(AUTH_BID, None)
        .await
        .expect("按 bundle_id MUST 命中已连接的 server");
    assert!(
        !resources.is_empty(),
        "mock 声明了 resources capability，列表不应为空"
    );

    // ② 负例：**同一台** server 的 display 名 MUST NOT 命中 → 4014。
    let err = manager
        .list_resources(AUTH_NAME, None)
        .await
        .expect_err("传 display 名 MUST 返 4014（McpServerNotFound），MUST NOT 回退按 name 解析");
    let msg = err.to_string();
    assert!(
        msg.contains(AUTH_NAME),
        "4014 应携带调用方传入的原 token，便于诊断；实际：{msg}"
    );

    // ③ 负例二：token **语法上是合法 bundle_id**、只是没注册。① 的 `AUTH_NAME` 含 `.`／空格／括号，
    //    在 `BundleId::try_from` 就被判废（`manager.rs:1273`）⇒ 那条只证明「非法串被拒」，够不到查表层。
    //    本条 token 合法，必须走到 `active_clients` 查表才会落空 —— 打的才是寻址层本身。
    let err2 = manager
        .list_resources("auth-srv", None)
        .await
        .expect_err("语法合法但未注册的 bundle_id MUST 返 4014");
    assert!(err2.to_string().contains("auth-srv"));

    // ③ 夹具自检：把常量钉到**真实派生函数**上。此前写的是 `assert_ne!(AUTH_NAME, AUTH_BID)`——那只是在比较
    //    两个字面量常量、运行期恒真，守不住派生算法漂移（正是本 Issue 要消灭的伪覆盖形态）。
    assert_eq!(
        smcp_computer::mcp_clients::bundle_id::derive_bundle_id(&stdio_cfg(AUTH_NAME, "x", None))
            .as_str(),
        AUTH_BID,
        "AUTH_BID 须是 AUTH_NAME 的真实派生值（派生算法漂移时此处即红）"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// B3：4006/7 `meta.mcp_server` == 路由所用 bundle_id（承接 #120 的「补断言」）
// ═══════════════════════════════════════════════════════════════════════════════

/// **AUTH-01 生产路径身份锁**：以**真实派生 bundle_id**（而非 `"srv-a"` 字面量）驱动 4007，断言
/// `meta.mcp_server` 逐字等于**路由实际使用的 bundle_id**，锁死整条链：
///
/// ```text
/// validate_tool_call → bundle_id → call_tool(&bundle_id) → finalize_tool_result → build_auth_error_result
/// ```
///
/// English: drive a real 403 through the manager and assert `meta.mcp_server` equals the bundle_id actually
/// used for routing — not the display name.
///
/// 承接 **#120** 的「补断言」诉求（其核心疑点已由 Discussion #23 B-8② 否定式作答、Issue 已关闭，断言转入
/// #142）。要点在于**真实派生值**：若用字面量夹具，`meta.mcp_server` 取 bundle_id 还是取 ServerName 无从分辨。
/// 此处 `AUTH_NAME`（`"auth.srv (display)"`）与 `AUTH_BID`（`auth_srv_display`）取值分叉，二者可辨。
///
/// 变异验证：`finalize_tool_result` 里 `build_auth_error_result(bundle_id.as_str(), …)` 改传 ServerName ⇒
/// `meta.mcp_server` 断言转红（已实测）。
#[tokio::test]
async fn auth_error_meta_carries_routing_bundle_id_not_display_name() {
    use smcp::tool_meta::{AUTH_ERROR_CODE_KEY, AUTH_MCP_SERVER_KEY};

    let manager = manager_on_mock().await;

    // ① 路由：exposed 名 = `{bundle_id}__{tool}`，整键查表（不 split 反解）。
    let exposed = format!("{AUTH_BID}__protected");
    let (routed_bid, display_name, tool) = manager
        .validate_tool_call(&exposed, &serde_json::json!({}))
        .await
        .expect("已注册工具 MUST 可路由");

    assert_eq!(
        routed_bid.as_str(),
        AUTH_BID,
        "validate_tool_call 的 `.0` MUST 是 bundle_id"
    );
    assert_eq!(
        display_name, AUTH_NAME,
        "`.1` 是 display 名（人看的那一半），MUST NOT 被身份键顶替"
    );
    assert_eq!(tool, "protected", "`.2` 是上游原始工具名（去 bundle 前缀）");

    // ② 用**路由实际产出的 bundle_id** 发起调用 —— 上游 403 ⇒ 走 Err 分支 ⇒ 分类 4007。
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        manager.call_tool(routed_bid.as_str(), &tool, serde_json::json!({}), None),
    )
    .await
    .expect("HANG: call_tool 未在 30s 内返回（上游拒绝后挂起）")
    .expect("授权错误经 finalize_tool_result 转成 CallToolResult(isError)，非 Err");

    assert_eq!(result.is_error, Some(true), "授权失败 MUST 是 isError 结果");
    let meta = result.meta.as_ref().expect("MUST 携带结果级 meta");

    assert_eq!(
        meta.get(AUTH_ERROR_CODE_KEY).and_then(|v| v.as_i64()),
        Some(i64::from(smcp::ErrorCode::ToolAuthorizationFailed.code())),
        "上游 403 MUST 分类为 4007"
    );

    // ③ 身份锁：meta.mcp_server 逐字 == 路由所用 bundle_id，**不是** display 名。
    let carried = meta
        .get(AUTH_MCP_SERVER_KEY)
        .and_then(|v| v.as_str())
        .expect("meta MUST 携带 mcp_server");
    assert_eq!(
        carried,
        routed_bid.as_str(),
        "4006/7 的 meta.mcp_server MUST == 路由所用 bundle_id（与 get_config 键、get_resources 入参同一身份空间）"
    );
    assert_ne!(
        carried, AUTH_NAME,
        "MUST NOT 是 display 名——这正是 #120 要锁死的那一跳"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// C：§4.9.1-2 回收判据**四景**（Discussion #32 裁决）—— 走真实构造路径
// ═══════════════════════════════════════════════════════════════════════════════
//
// # 为什么这四景必须在这里重做一遍
//
// `settings/reconciler.rs` 里已有同名四景，但它们是对**纯函数** `reclaimable_mcp_deps(deps, other,
// non_plugin)` 的单测——`non_plugin` 集合是**字面量硬塞**进去的。也就是说它们永远不会执行
// `Computer::non_plugin_declared_bundle_ids` 的**构造过程**，而 flag / embed 两条 origin 投影**恰恰只发生在
// 那个构造里**（`resolve_snapshot(flag_mcp_config_path:…, embed_servers:…)`）。若哪天漏传其中之一，那四条
// 纯函数测试**照样全绿**——这正是本 Epic 反复踩的「测了过滤器、没测数据源」形态。
//
// 协议对此有明文（conformance §5 四景条款 + runtime-contract §4.9.1-2）：判据 MUST 评估在**带 origin 的
// 运行期权威配置集**上，MUST NOT 用裸活跃集（无 origin ⇒ flag/embed/plugin 三条挂载路径可观测同形）。
// python 侧同样明确避开了裸活跃集夹具，改注入**真实 flag 文件 / 真实 embed 构造入参**——本节逐景对齐。
//
// 双端对拍：python `test_vector{1,2,3,4}_*`（`tests/unit_tests/computer/settings/test_mcp_dependency_model.py`）。

/// 回收判据夹具：display 名带 `.`，令「误用 name 当身份」的实现无法蒙混过关（§2.0-1）。
const FIGMA_NAME: &str = "figma.mcp";
/// `normalize_name("figma.mcp")` == `figma_mcp`。
const FIGMA_BID: &str = "figma_mcp";

/// 记录停摘名单的 hook —— 四景的唯一观测点。
struct RecordingHooks {
    removed: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl McpInstallHooks for RecordingHooks {
    fn existing_servers(&self) -> std::collections::HashMap<BundleId, ServerName> {
        // 冲突门控非本节关注点；四景只观测「谁被停摘」。
        std::collections::HashMap::new()
    }

    async fn register_server(&self, _cfg: MCPServerConfig) -> Result<(), McpHookError> {
        Ok(())
    }

    async fn remove_server(&self, id: &BundleId) -> Result<(), McpHookError> {
        self.removed.lock().unwrap().push(id.as_str().to_string());
        Ok(())
    }
}

/// user scope（XDG）锚到 `<tmp>/xdg`，隔离宿主 `~/.config`——否则判据会读到开发机真实声明面。
fn xdg_env(td: &TempDir) -> EnvMap {
    std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        td.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect()
}

fn home_of(td: &TempDir) -> PathBuf {
    td.path().join("home")
}

/// 写一份 **flag 层** mcp.json（模拟 `a2c-computer run --mcp-config <file>`）。
///
/// mcp.json 是 **name-keyed** 声明面：键即 display 名，bundle_id 由其缺省派生（`figma.mcp` → `figma_mcp`）。
fn flag_mcp_file(td: &TempDir, command: &str) -> PathBuf {
    let p = td.path().join("flag-mcp.json");
    std::fs::write(
        &p,
        serde_json::to_string(&json!({
            "servers": { FIGMA_NAME: { "type": "stdio", "server_parameters": { "command": command } } },
            "inputs": []
        }))
        .unwrap(),
    )
    .unwrap();
    p
}

/// 播种一条「声明依赖 `deps` 的已安装 plugin」账本记录（F1：`mcpServers` = 纯 bundle_id 数组）。
///
/// `install_path`：
/// - `None` —— plugin 只在**账本**里声明依赖，**不**向 `resolve_snapshot` 贡献任何 `origin=Plugin` 配置条目
///   （①②③ 用此形态：X 的来源只有 flag / embed / 无）；
/// - `Some(root)` —— plugin 树真实存在且内含 `mcp-servers/*.json`，配合 `enabledPlugins` 使
///   `collect_enabled_bundled_servers` 产出该 server ⇒ resolve 里真的多出一条 `origin=Plugin` 条目
///   （④ 用此形态：同一 bundle_id 上 **plugin 与 flag 两个来源并存**）。
fn seed_plugin_with_deps(
    home: &Path,
    env: &EnvMap,
    pid: &str,
    deps: &[&str],
    install_path: Option<&Path>,
) {
    let mut extra = Map::new();
    extra.insert("version".to_string(), json!("1.0.0"));
    extra.insert("scope".to_string(), json!("user"));
    let record = InstalledPluginRecord {
        install_path: install_path.map(|p| p.to_string_lossy().into_owned()),
        mcp_servers: deps
            .iter()
            .map(|s| BundleId::try_from((*s).to_string()).unwrap())
            .collect(),
        extra,
    };
    let pid_owned = pid.to_string();
    update_installed_plugins(
        move |f| {
            f.account
                .plugins
                .insert(pid_owned.clone(), vec![record.clone()]);
        },
        Some(home),
        Some(env),
    )
    .unwrap();

    let pid_intent = pid.to_string();
    update_installed_plugins_intent(
        move |f| {
            f.account.installed_plugins.insert(pid_intent.clone());
        },
        Some(home),
        Some(env),
    )
    .unwrap();
}

/// 卸载 `pid`，返回**实际被停摘**的 bundle_id 列表。
async fn uninstall_and_capture(
    computer: &Computer<SilentSession>,
    env: &EnvMap,
    pid: &str,
) -> Vec<String> {
    let removed = Arc::new(Mutex::new(Vec::new()));
    let hooks = RecordingHooks {
        removed: Arc::clone(&removed),
    };
    computer
        .uninstall_plugin(
            pid,
            UninstallOptions {
                scope: None,
                keep_servers: false,
                env: Some(env),
            },
            Some(&hooks),
        )
        .await
        .expect("uninstall MUST 成功");
    let out = removed.lock().unwrap().clone();
    out
}

/// 造一棵真实 plugin 树（内含 `mcp-servers/figma.mcp.json`），并把 `enabledPlugins[pid]=true` 写进
/// **user scope settings**——两者齐备，`collect_enabled_bundled_servers` 才会产出该 bundled server，
/// `resolve_snapshot` 也才会为它追加一条 `origin=Plugin` 基线条目。
fn seed_plugin_tree_and_enable(td: &TempDir, env: &EnvMap, pid: &str, command: &str) -> PathBuf {
    let root = td.path().join("plugins").join(pid.replace('@', "_at_"));
    let sd = root.join("mcp-servers");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(
        sd.join(format!("{FIGMA_NAME}.json")),
        serde_json::to_string(&json!({
            "type": "stdio",
            "name": FIGMA_NAME,
            "server_parameters": { "command": command }
        }))
        .unwrap(),
    )
    .unwrap();

    // enabledPlugins 必须落在**真实 settings 文件**里——resolve_snapshot 读的是合并后的文件层，
    // 不是调用方手递的 map。
    let sp = smcp_computer::settings::scope::user_settings_path(Some(env));
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(
        &sp,
        serde_json::to_string(&json!({ "enabledPlugins": { pid: true } })).unwrap(),
    )
    .unwrap();
    root
}

/// 一台注入全上下文的 Computer（skill_home = 账本根、config_dir = project/local 锚、config_env = XDG）。
///
/// `embed`：`Some(..)` 走宿主构造入参（origin=embed，四景②）；`None` 为构造期空集。
/// 四景**共用本函数**——若各景各写一套 builder 链，将来动其一会让另一些景静默脱钩（vectors ①②④ 的断言
/// 都是「什么都没被停摘」，任何令 seeding 失效的改动都会让它们**空过**；挡住空过的正对照是 ③，故 ③ 与
/// ①②④ MUST 走同一条构造路径才有对照意义）。
fn base_computer(
    td: &TempDir,
    env: &EnvMap,
    embed: Option<std::collections::HashMap<String, MCPServerConfig>>,
) -> Computer<SilentSession> {
    Computer::new("c", SilentSession::new("s"), None, embed, false, false)
        .with_skill_home(home_of(td))
        .with_blob_cache_root(td.path().join("blob"))
        .with_config_dir(td.path().join("config"))
        .with_config_env(env.clone())
}

/// **四景①**：X 经 flag（`--mcp-config`）挂载 ⇒ 卸载声明依赖 X 的 plugin **不回收** X。
///
/// English: a server declared via `--mcp-config` (origin=flag) is never collaterally torn down.
///
/// `origin=flag` ⇒ 落进 `non_plugin_declared_bundle_ids` ⇒ 判据「X 非用户声明」为假 ⇒ 不回收。
///
/// 变异验证：`non_plugin_declared_bundle_ids` 的 `SnapshotArgs` 去掉 `flag_mcp_config_path` ⇒ 本例必红
/// （已实测）。纯函数四景测不到这一点——那正是本节存在的理由。
#[tokio::test]
async fn vector1_flag_mounted_server_is_never_collateral() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    seed_plugin_with_deps(&home_of(&td), &env, "audit@acme", &[FIGMA_BID], None);

    // 用户经命令行点名声明 figma.mcp（真实 flag 文件，非合成集合）。
    let computer =
        base_computer(&td, &env, None).with_mcp_flag_config(flag_mcp_file(&td, "user-cmd"));

    let removed = uninstall_and_capture(&computer, &env, "audit@acme").await;
    assert!(
        removed.is_empty(),
        "经 --mcp-config 挂载的用户 server 被连坐停摘（协议 §4.9.1-2 四景①），实际停摘：{removed:?}"
    );
}

/// **四景②**：X 经**宿主构造入参**挂载（`Computer::new(mcp_servers=…)`，origin=embed）⇒ **不回收**。
///
/// English: a server mounted through the host constructor (origin=embed) is never collaterally torn down.
///
/// 宿主自有 server 不因 plugin 卸载连坐（runtime-contract §2.5-3：embed 是非-plugin origin）。embed 层由
/// **#147（S14）** 引入并投影进权威集——本景即其核心可观测后果。
///
/// 变异验证：`SnapshotArgs` 去掉 `embed_servers` ⇒ 本例必红（已实测）。
#[tokio::test]
async fn vector2_embed_mounted_server_is_never_collateral() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    seed_plugin_with_deps(&home_of(&td), &env, "audit@acme", &[FIGMA_BID], None);

    // 宿主构造入参挂载（键被忽略、身份从 config 派生 ⇒ figma.mcp → figma_mcp）。
    let embed: std::collections::HashMap<String, MCPServerConfig> = std::iter::once((
        FIGMA_NAME.to_string(),
        stdio_cfg(FIGMA_NAME, "/bin/echo", None),
    ))
    .collect();
    let computer = base_computer(&td, &env, Some(embed));

    let removed = uninstall_and_capture(&computer, &env, "audit@acme").await;
    assert!(
        removed.is_empty(),
        "宿主构造入参挂载的 server 被连坐停摘（协议 §4.9.1-2 四景②），实际停摘：{removed:?}"
    );
}

/// **四景③**：X 仅由 plugin 声明（无其他 plugin 依赖、无任何非-plugin 声明）⇒ **回收**。
///
/// English: a server declared solely by the uninstalled plugin is reclaimed (no leak).
///
/// 四景里唯一「该回收」的一条——它守住判据**不会因①②的修法而全面失灵**（否则 server 永久泄漏成僵尸）。
/// plugin 声明**不进 resolve 的非-plugin 集**（`filter(origin != Plugin)` 结构性排除）⇒ X ∉ 非-plugin 集 ⇒ 回收。
///
/// 变异验证：本例与①②互为对照——把判据改成「恒不回收」时本例必红，改成「恒回收」时①②必红，
/// 故三者合起来锁死判据的**两个方向**，任一单向恒真实现都无法同时通过。
#[tokio::test]
async fn vector3_plugin_only_server_is_reclaimed() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);
    seed_plugin_with_deps(&home_of(&td), &env, "audit@acme", &[FIGMA_BID], None);

    // 无 flag、无 embed、无 durable 声明 ⇒ 非-plugin 集为空。
    let computer = base_computer(&td, &env, None);

    let removed = uninstall_and_capture(&computer, &env, "audit@acme").await;
    assert_eq!(
        removed,
        vec![FIGMA_BID.to_string()],
        "无人依赖 ∧ 无非-plugin 声明的 server 未被回收 ⇒ 泄漏（四景③）"
    );
}

/// **四景④**：同 `bundle_id` **混源碰撞**（plugin 与 flag 在同一 bundle_id 上**两个来源并存**，flag > plugin）
/// ⇒ **不回收**。
///
/// English: mixed-origin collision on the same bundle_id (a real plugin-declared entry plus a flag
/// declaration) ⇒ never reclaimed.
///
/// **与①的实质区别在夹具，不在结论**（二者结论同为「不回收」——协议 §5 的 ①④ 条款本就重叠）：
/// - ① 的 X **只有 flag 一个来源**：plugin 仅在**账本**里声明「我依赖 X」（`install_path: None` ⇒ 不产出
///   任何 `origin=Plugin` 配置条目）；
/// - ④ 的 X **两个来源并存**：plugin 树真实存在且内含 `mcp-servers/{FIGMA_NAME}.json`、且 `enabledPlugins`
///   已开 ⇒ `resolve_snapshot` 会为它追加一条 `origin=Plugin` 基线条目，再被同 bundle_id 的 flag 条目按
///   `plugin < flag` 层序**吸收**（`settings/config/snapshot.rs:417` 的 `claimed_bundle_ids` 门）。
///
/// 考的正是：「plugin 也声明了同一个 bundle_id」这件事**不会**把「非-plugin 声明」盖掉、令用户那条失效
/// （runtime-contract §2.5-3 用户主权）。
#[tokio::test]
async fn vector4_mixed_origin_same_bundle_id_is_never_collateral() {
    let td = TempDir::new().unwrap();
    let env = xdg_env(&td);

    // plugin 侧：真实树 + enabledPlugins ⇒ 产出 origin=Plugin 条目；账本同时声明依赖该 bundle_id。
    let root = seed_plugin_tree_and_enable(&td, &env, "audit@acme", "plugin-baseline-cmd");
    seed_plugin_with_deps(&home_of(&td), &env, "audit@acme", &[FIGMA_BID], Some(&root));

    // 夹具自检：坐实 plugin 侧**确实**产出了同 bundle_id 的声明条目——否则本景会退化成①（空过风险）。
    let declared: Map<String, serde_json::Value> =
        std::iter::once(("enabledPlugins".to_string(), json!({ "audit@acme": true }))).collect();
    let plugin_decls = smcp_computer::settings::collect_enabled_bundled_servers(
        &home_of(&td),
        Some(&env),
        &declared,
    );
    assert_eq!(
        plugin_decls.len(),
        1,
        "夹具自检失败：plugin 未产出 bundled server 声明，本景将退化为四景①（零增量鉴别力）"
    );
    assert_eq!(
        smcp_computer::mcp_clients::bundle_id::resolve_bundle_id(&plugin_decls[0].config).as_str(),
        FIGMA_BID,
        "夹具自检失败：plugin 声明的 bundle_id 与 flag 侧不同 ⇒ 不构成同 bundle_id 碰撞"
    );

    // 用户经 flag **也**声明同一 bundle_id（连接参数与 plugin 基线不同，构成真正的混源覆盖）。
    let computer =
        base_computer(&td, &env, None).with_mcp_flag_config(flag_mcp_file(&td, "user-owned-cmd"));

    let removed = uninstall_and_capture(&computer, &env, "audit@acme").await;
    assert!(
        removed.is_empty(),
        "混源（plugin + flag）同 bundle_id 被连坐停摘（协议 §4.9.1-2 四景④），实际停摘：{removed:?}"
    );
}
