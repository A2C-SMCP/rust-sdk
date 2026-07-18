// #106：运行期工具集变化——真链路 e2e（真 Server + 真 AsyncSmcpAgent + 真 Computer + 真可变 MCP 子进程）。
//
// 严格三态（对齐 python#127）：运行期 新增 → 同名换 schema → 移除，断言**真实 smcp-agent** 因
// `notify:update_tool_list` 自动回拉后的本地工具视图分别出现/更新/消失。消费方**加法式**（on_tools_received
// 只 merge、不删），故「移除后 dyn_tool 消失」真正证明预清回调 `on_computer_update_tool_list` load-bearing。
//
// 运行 / run（需 Node.js）:
//   cargo test --features full --test e2e_tool_list_update_test -- --ignored --nocapture

mod e2e;

use e2e::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

#[allow(unused_imports)]
use async_trait::async_trait;
use smcp::{SMCPTool, UpdateToolListNotification};
use smcp_agent::{AsyncAgentEventHandler, AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig};
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, StdioServerConfig, StdioServerParameters,
};

fn mutable_server_path() -> String {
    format!(
        "{}/tests/mutable-mcp-server/index.js",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// 加法式消费方 + 预清回调：模拟只 add 不 remove 的下游（如 TFRobotServer）。
/// `on_tools_received` 仅 insert/overwrite；`on_computer_update_tool_list`（预清）清空该 computer 的视图。
/// 唯有预清 load-bearing，「移除」才能在此加法式视图里体现为 dyn_tool 消失。
#[derive(Clone, Default)]
struct AdditiveToolView {
    /// computer -> (tool name -> params_schema 签名)
    view: Arc<tokio::sync::Mutex<HashMap<String, BTreeMap<String, String>>>>,
}

impl AdditiveToolView {
    async fn names(&self, computer: &str) -> Vec<String> {
        self.view
            .lock()
            .await
            .get(computer)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
    async fn schema_sig(&self, computer: &str, tool: &str) -> Option<String> {
        self.view
            .lock()
            .await
            .get(computer)
            .and_then(|m| m.get(tool).cloned())
    }
}

#[async_trait]
impl AsyncAgentEventHandler for AdditiveToolView {
    async fn on_computer_update_tool_list(
        &self,
        data: UpdateToolListNotification,
        _agent: &AsyncSmcpAgent,
    ) -> smcp_agent::Result<()> {
        // 预清：清空该 computer 的工具视图（加法式消费方靠此感知移除/换 schema）。
        self.view
            .lock()
            .await
            .entry(data.computer)
            .or_default()
            .clear();
        Ok(())
    }

    async fn on_tools_received(
        &self,
        computer: &str,
        tools: Vec<SMCPTool>,
        _agent: &AsyncSmcpAgent,
    ) -> smcp_agent::Result<()> {
        // 加法式重加：只 insert/overwrite，绝不删除。
        let mut g = self.view.lock().await;
        let m = g.entry(computer.to_string()).or_default();
        for t in tools {
            m.insert(
                t.name.clone(),
                serde_json::to_string(&t.params_schema).unwrap_or_default(),
            );
        }
        Ok(())
    }
}

/// 轮询 `cond` 直到为真或超时。
async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test]
#[cfg(all(feature = "agent", feature = "computer", feature = "server"))]
#[ignore = "e2e: 需要 Node.js + full features；cargo test --features full --test e2e_tool_list_update_test -- --ignored"]
async fn tool_list_update_full_chain_add_rename_remove() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init()
        .ok();

    let server = TestServer::start().await.expect("start server");
    let office_id = generate_office_id();
    let computer_name = generate_computer_name();

    // ── Computer：挂可变 MCP 子进程 ──
    let mut servers = HashMap::new();
    servers.insert(
        "mutable".to_string(),
        MCPServerConfig::Stdio(StdioServerConfig::new(
            "mutable",
            StdioServerParameters {
                command: "node".to_string(),
                args: vec![mutable_server_path()],
                env: HashMap::new(),
                cwd: None,
            },
        )),
    );

    let computer = Computer::new(
        computer_name.clone(),
        SilentSession::new("s"),
        None,
        Some(servers),
        true,
        true,
    );
    computer.boot_up().await.expect("boot");
    computer
        .start_all_mcp_clients()
        .await
        .expect("start mcp client");
    computer
        .connect_socketio(
            server.url(),
            ConnectOptions {
                auth_payload: Some(auth_dict("test_secret")),
                ..Default::default()
            },
        )
        .await
        .expect("computer connect");
    computer
        .join_office(&office_id, &computer_name)
        .await
        .expect("computer join");

    // ── Agent：加法式消费方 + 预清回调 ──
    let recorder = AdditiveToolView::default();
    let auth = DefaultAuthProvider::new(generate_agent_name(), office_id.clone())
        .with_api_key("test_secret".to_string());
    let mut agent =
        AsyncSmcpAgent::new(auth, SmcpAgentConfig::default()).with_event_handler(recorder.clone());
    agent.connect(server.url()).await.expect("agent connect");
    agent.join_office("agent").await.expect("agent join");

    // ── 新增：set_phase(1) → server 增 dyn_tool + 发 tools/list_changed → 全链路 → agent 视图出现 dyn_tool ──
    agent
        .tool_call(
            &computer_name,
            "set_phase",
            serde_json::json!({ "phase": 1 }),
        )
        .await
        .expect("set_phase(1)");
    let ok_add = wait_until(
        || async {
            let n = recorder.names(&computer_name).await;
            n.contains(&"set_phase".to_string()) && n.contains(&"dyn_tool".to_string())
        },
        WAIT,
    )
    .await;
    assert!(
        ok_add,
        "新增未生效：agent 视图应含 dyn_tool，实得 {:?}",
        recorder.names(&computer_name).await
    );

    // ── 同名换 schema：set_phase(2) → dyn_tool schema A({x}) → B({y}) → agent 视图 schema 更新 ──
    agent
        .tool_call(
            &computer_name,
            "set_phase",
            serde_json::json!({ "phase": 2 }),
        )
        .await
        .expect("set_phase(2)");
    let ok_schema = wait_until(
        || async {
            recorder
                .schema_sig(&computer_name, "dyn_tool")
                .await
                .map(|s| s.contains("\"y\""))
                .unwrap_or(false)
        },
        WAIT,
    )
    .await;
    assert!(
        ok_schema,
        "换 schema 未生效：dyn_tool 应含字段 y，实得 {:?}",
        recorder.schema_sig(&computer_name, "dyn_tool").await
    );

    // ── 移除：set_phase(3) → server 去 dyn_tool → 预清清空 + 回拉重加 → agent 视图 dyn_tool 消失（正向终态）──
    agent
        .tool_call(
            &computer_name,
            "set_phase",
            serde_json::json!({ "phase": 3 }),
        )
        .await
        .expect("set_phase(3)");
    let ok_remove = wait_until(
        || async {
            let n = recorder.names(&computer_name).await;
            n == vec!["set_phase".to_string()]
        },
        WAIT,
    )
    .await;
    assert!(
        ok_remove,
        "移除未生效：agent 视图应仅剩 set_phase（证明预清回调 load-bearing），实得 {:?}",
        recorder.names(&computer_name).await
    );

    let _ = agent.leave_office().await;
    computer.shutdown().await.ok();
}
