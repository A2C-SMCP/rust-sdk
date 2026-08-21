use serde::Serialize;
use serde_json::{json, Value};
use smcp_computer::computer::{Computer, SilentSession};
use smcp_computer::mcp_clients::model::{
    MCPServerConfig, StdioServerConfig, StdioServerParameters,
};
use smcp_computer::{ComputerEvent, ComputerStatusSnapshot};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

const PROJECTION_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_SETTLE: Duration = Duration::from_millis(750);

#[derive(Debug, Serialize)]
struct TransitionRecord {
    run: usize,
    transition: &'static str,
    requested_phase: i64,
    before: ComputerStatusSnapshot,
    after: ComputerStatusSnapshot,
    projection_reached: bool,
    projection_wait_ms: u128,
    before_projection: Value,
    after_projection: Value,
    observed_events: Vec<ComputerEvent>,
}

#[derive(Debug, Serialize)]
struct RunRecord {
    run: usize,
    initial_status: ComputerStatusSnapshot,
    initial_projection: Value,
    transitions: Vec<TransitionRecord>,
}

#[derive(Debug, Serialize)]
struct ExperimentReport {
    issue: &'static str,
    unix_started_at: u64,
    rustc: String,
    cargo: String,
    node: String,
    git_head: String,
    repetitions: usize,
    runs: Vec<RunRecord>,
}

fn command_version(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if stdout.is_empty() {
                stderr
            } else {
                stdout
            }
        })
        .unwrap_or_else(|error| format!("unavailable: {error}"))
}

fn mutable_server_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/mutable-mcp-server/index.js")
        .canonicalize()
        .expect("mutable MCP fixture must exist")
}

fn stdio_config() -> MCPServerConfig {
    MCPServerConfig::Stdio(StdioServerConfig::new(
        "mutable",
        StdioServerParameters {
            command: "node".to_string(),
            args: vec![mutable_server_path().to_string_lossy().into_owned()],
            env: HashMap::new(),
            cwd: None,
        },
    ))
}

async fn projection(computer: &Computer<SilentSession>) -> Value {
    let mut tools = computer
        .get_available_tools()
        .await
        .expect("get_available_tools must succeed");
    tools.sort_by(|left, right| left.name.as_ref().cmp(right.name.as_ref()));
    serde_json::to_value(tools).expect("tool projection must serialize")
}

fn projection_has(projection: &Value, tool_suffix: &str, schema_property: Option<&str>) -> bool {
    projection.as_array().is_some_and(|tools| {
        tools.iter().any(|tool| {
            let name_matches = tool
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.ends_with(tool_suffix));
            let schema_matches = schema_property.is_none_or(|property| {
                tool.pointer(&format!("/inputSchema/properties/{property}"))
                    .is_some()
            });
            name_matches && schema_matches
        })
    })
}

async fn wait_for_projection(
    computer: &Computer<SilentSession>,
    expected_count: usize,
    tool_suffix: &str,
    schema_property: Option<&str>,
    should_exist: bool,
) -> (bool, u128, Value) {
    let started = Instant::now();
    loop {
        let status = computer.status().await;
        let current = projection(computer).await;
        let exists = projection_has(&current, tool_suffix, schema_property);
        if status.tools == expected_count && exists == should_exist {
            return (true, started.elapsed().as_millis(), current);
        }
        if started.elapsed() >= PROJECTION_TIMEOUT {
            return (false, started.elapsed().as_millis(), current);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn collect_settled_events(
    receiver: &mut broadcast::Receiver<ComputerEvent>,
) -> Vec<ComputerEvent> {
    let deadline = tokio::time::Instant::now() + EVENT_SETTLE;
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, receiver.recv()).await {
            Ok(Ok(event)) => events.push(event),
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                panic!("event receiver lagged by {skipped}")
            }
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
        }
    }
    events
}

async fn transition(
    run: usize,
    computer: &Computer<SilentSession>,
    receiver: &mut broadcast::Receiver<ComputerEvent>,
    set_phase_tool: &str,
    label: &'static str,
    phase: i64,
    expected_count: usize,
    schema_property: Option<&str>,
    dyn_tool_should_exist: bool,
) -> TransitionRecord {
    let before = computer.status().await;
    let before_projection = projection(computer).await;
    computer
        .execute_tool(
            &format!("experiment-{run}-{label}"),
            set_phase_tool,
            json!({"phase": phase}),
            Some(5.0),
        )
        .await
        .expect("set_phase call must complete");

    let (projection_reached, projection_wait_ms, after_projection) = if label == "same_projection" {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let current = projection(computer).await;
        (
            computer.status().await.tools == expected_count
                && projection_has(&current, "__dyn_tool", schema_property) == dyn_tool_should_exist,
            250,
            current,
        )
    } else {
        wait_for_projection(
            computer,
            expected_count,
            "__dyn_tool",
            schema_property,
            dyn_tool_should_exist,
        )
        .await
    };
    let observed_events = collect_settled_events(receiver).await;
    let after = computer.status().await;

    TransitionRecord {
        run,
        transition: label,
        requested_phase: phase,
        before,
        after,
        projection_reached,
        projection_wait_ms,
        before_projection,
        after_projection,
        observed_events,
    }
}

async fn run_once(run: usize) -> RunRecord {
    let temp = tempfile::TempDir::new().expect("temporary isolation root");
    let mut servers = HashMap::new();
    servers.insert("ignored-key".to_string(), stdio_config());
    let env: HashMap<String, String> = std::iter::once((
        "XDG_CONFIG_HOME".to_string(),
        temp.path().join("xdg").to_string_lossy().into_owned(),
    ))
    .collect();
    let computer = Computer::new(
        format!("issue-196-experiment-{run}"),
        SilentSession::new("experiment"),
        None,
        Some(servers),
        false,
        false,
    )
    .with_skill_home(temp.path().join("skills"))
    .with_blob_cache_root(temp.path().join("blob"))
    .with_config_dir(temp.path().join("project"))
    .with_config_env(env)
    .with_confirm_callback(|_, _, _, _| true);

    computer
        .boot_up()
        .await
        .expect("Computer boot must succeed");
    computer
        .start_all_mcp_clients()
        .await
        .expect("mutable MCP server must start");

    let initial_status = computer.status().await;
    let initial_projection = projection(&computer).await;
    let set_phase_tool = initial_projection
        .as_array()
        .and_then(|tools| {
            tools.iter().find_map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| name.ends_with("__set_phase"))
                    .map(str::to_owned)
            })
        })
        .expect("set_phase must be routed");
    let mut receiver = computer.subscribe_events();

    let transitions = vec![
        transition(
            run,
            &computer,
            &mut receiver,
            &set_phase_tool,
            "add_tool",
            1,
            2,
            Some("x"),
            true,
        )
        .await,
        transition(
            run,
            &computer,
            &mut receiver,
            &set_phase_tool,
            "schema_change_same_count",
            2,
            2,
            Some("y"),
            true,
        )
        .await,
        transition(
            run,
            &computer,
            &mut receiver,
            &set_phase_tool,
            "same_projection",
            2,
            2,
            Some("y"),
            true,
        )
        .await,
        transition(
            run,
            &computer,
            &mut receiver,
            &set_phase_tool,
            "remove_tool",
            3,
            1,
            None,
            false,
        )
        .await,
    ];

    computer.shutdown().await.expect("clean shutdown");
    RunRecord {
        run,
        initial_status,
        initial_projection,
        transitions,
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "smcp_computer=debug,rmcp=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs();
    let mut runs = Vec::new();
    for run in 1..=3 {
        eprintln!("issue-196 experiment run {run}/3");
        runs.push(run_once(run).await);
    }
    let report = ExperimentReport {
        issue: "https://github.com/A2C-SMCP/rust-sdk/issues/196",
        unix_started_at: started_at,
        rustc: command_version("rustc", &["--version"]),
        cargo: command_version("cargo", &["--version"]),
        node: command_version("node", &["--version"]),
        git_head: command_version("git", &["rev-parse", "HEAD"]),
        repetitions: runs.len(),
        runs,
    };
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("results.json");
    std::fs::write(
        &output,
        serde_json::to_vec_pretty(&report).expect("report serialization"),
    )
    .expect("write experiment report");
    println!("{}", output.display());
}
