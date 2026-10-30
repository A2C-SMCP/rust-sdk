//! #245 PR3：`client:resolve_path` → `client:get_blob` **真实 socket 链路**与 sync/async parity。
//!
//! 覆盖：Agent async/sync 端到端执行、七字段 `ResolvePathRet` + `mtime`、分块拉取 / eof / sha256、
//! 拉取期重围栏（删除 → 4018 gone、撤权 → 4018 forbidden）、4020 四 reason 的 flat payload 透传。
//!
//! 真实 Server（`TestServer`）+ 真实 Computer（settings readRoots）+ 真实 Agent transport。

#![cfg(all(feature = "agent", feature = "computer", feature = "server"))]

mod e2e;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use e2e::*;
use serde_json::json;
use tempfile::TempDir;

use smcp_agent::{
    AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig, SmcpAgentError, SyncSmcpAgent,
};
use smcp_computer::blob::{decode_blob_handle, DecodedHandle};
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};

const SECRET: &str = "test_secret";

/// 隔离 user-config（HOME/XDG），绝不读写真实 `$HOME`。
fn isolated_env(base: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), base.join("home").display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env.insert(
        "XDG_STATE_HOME".to_string(),
        base.join("xdg-state").display().to_string(),
    );
    env
}

/// 一套真实 FS + 隔离 settings 的测试栈。
struct Stack {
    tmp: TempDir,
    share: PathBuf,
    env: HashMap<String, String>,
    settings: PathBuf,
}

impl Stack {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().join("share");
        let env = isolated_env(tmp.path());
        fs::create_dir_all(&share).unwrap();
        fs::create_dir_all(tmp.path().join("home")).unwrap();
        fs::create_dir_all(tmp.path().join("xdg-config/a2c")).unwrap();
        let settings = tmp.path().join("xdg-config/a2c/settings.json");
        fs::write(
            &settings,
            json!({"readRoots": [share.to_string_lossy()]}).to_string(),
        )
        .unwrap();
        Self {
            tmp,
            share,
            env,
            settings,
        }
    }

    fn write_settings(&self, value: serde_json::Value) {
        fs::write(&self.settings, value.to_string()).unwrap();
    }

    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.share.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, data).unwrap();
        path
    }

    fn computer(&self, name: &str) -> Computer<SilentSession> {
        Computer::new(name, SilentSession::new("pr3"), None, None, false, false)
            .with_config_dir(&self.tmp.path().join("proj"))
            .with_config_env(self.env.clone())
            .with_skill_home(&self.tmp.path().join("skills"))
            .with_blob_cache_root(&self.tmp.path().join("blob"))
    }
}

fn protocol_error(error: SmcpAgentError) -> smcp_agent::SmcpProtocolError {
    match error {
        SmcpAgentError::Protocol(payload) => *payload,
        other => panic!("expected protocol error, got: {other}"),
    }
}

async fn connect_stack(
    stack: &Stack,
    server_url: &str,
    office: &str,
    computer_name: &str,
    agent_name: &str,
) -> (Computer<SilentSession>, AsyncSmcpAgent) {
    let computer = stack.computer(computer_name);
    computer.boot_up().await.unwrap();
    computer
        .connect_socketio(
            server_url,
            ConnectOptions {
                auth_payload: Some(auth_dict(SECRET)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    computer.join_office(office, computer_name).await.unwrap();

    let auth = DefaultAuthProvider::new(agent_name.to_string(), office.to_string())
        .with_api_key(SECRET.to_string());
    let mut agent = AsyncSmcpAgent::new(auth, SmcpAgentConfig::default());
    agent.connect(server_url).await.unwrap();
    agent.join_office(agent_name).await.unwrap();
    // 等 join 在房间内稳定可见（与既有 e2e 用例一致）。
    tokio::time::sleep(Duration::from_millis(250)).await;
    (computer, agent)
}

async fn pull_all_chunks(
    agent: &AsyncSmcpAgent,
    computer: &str,
    handle: &str,
    max_chunk: u64,
    expected_sha: &str,
) -> (Vec<u8>, u64, usize) {
    let mut offset = 0u64;
    let mut bytes = Vec::new();
    let mut chunks = 0usize;
    loop {
        let chunk = agent
            .get_blob(computer, handle, Some(offset), Some(max_chunk))
            .await
            .unwrap();
        chunks += 1;
        assert_eq!(chunk.chunk_offset, offset);
        // 跨块恒定：sha256 / total_size 每块一致。
        assert_eq!(
            chunk.sha256, expected_sha,
            "sha must be stable across chunks"
        );
        let data = base64::engine::general_purpose::STANDARD
            .decode(chunk.blob.as_bytes())
            .unwrap();
        bytes.extend_from_slice(&data);
        offset += data.len() as u64;
        if chunk.eof {
            return (bytes, chunk.total_size, chunks);
        }
        // 非末块必须恰好用满 max_chunk（否则说明 clamp/切片实现未生效）。
        assert!(!chunk.eof);
        assert_eq!(
            data.len() as u64,
            max_chunk,
            "non-eof chunk must fill max_chunk_bytes"
        );
    }
}

#[tokio::test]
async fn async_agent_resolve_path_to_get_blob_end_to_end() {
    let server = TestServer::start().await.unwrap();
    let stack = Stack::new();
    let office = generate_office_id();
    let computer_name = generate_computer_name();
    let agent_name = generate_agent_name();
    let (_computer, agent) =
        connect_stack(&stack, &server.url(), &office, &computer_name, &agent_name).await;

    let file = stack.file("hello.txt", b"hello path");
    let ret = agent
        .resolve_path(&computer_name, file.to_str().unwrap())
        .await
        .unwrap();

    // 七字段 + mtime（wire 形状钉死）。
    assert_eq!(ret.total_size, 10);
    assert_eq!(ret.mime_type, "text/plain");
    assert_eq!(ret.sha256, smcp::utils::hash::sha256_hex(b"hello path"));
    assert!(!ret.req_id.as_str().is_empty());
    assert!(ret.mtime != 0, "mtime must be populated");
    assert!(ret.canonical_path.ends_with("hello.txt"));
    assert!(matches!(
        decode_blob_handle(&ret.blob_handle).unwrap(),
        DecodedHandle::Path(_)
    ));

    // 分块拉取（4 字节/块）：eof + 全量 sha + 重组字节。
    let (bytes, total, chunks) =
        pull_all_chunks(&agent, &computer_name, &ret.blob_handle, 4, &ret.sha256).await;
    assert_eq!(total, 10);
    assert_eq!(bytes, b"hello path");
    // 10 字节 / 4 = 4+4+2 → 恰 3 块。
    assert_eq!(chunks, 3, "max_chunk_bytes must drive chunking");

    // 4020 四 reason 的 flat payload（Agent 侧按 protocol error 透传）。
    let outside = stack.tmp.path().join("outside.txt");
    fs::write(&outside, b"x").unwrap();
    let cases = [
        (outside.to_str().unwrap().to_string(), "forbidden"),
        (
            stack
                .share
                .join("missing.txt")
                .to_str()
                .unwrap()
                .to_string(),
            "not_found",
        ),
        (stack.share.to_str().unwrap().to_string(), "not_a_file"),
        ("relative/path".to_string(), "invalid_path"),
    ];
    for (path, expected_reason) in cases {
        let error = protocol_error(agent.resolve_path(&computer_name, &path).await.unwrap_err());
        assert_eq!(error.code, 4020, "path={path}");
        assert_eq!(error.message, "Path not accessible", "path={path}");
        assert_eq!(
            error.reason.as_deref(),
            Some(expected_reason),
            "path={path}"
        );
    }

    // 拉取期重围栏：删除 → 4018 gone；撤权 → 4018 forbidden。
    let gone_file = stack.file("gone.txt", b"bye");
    let gone_ret = agent
        .resolve_path(&computer_name, gone_file.to_str().unwrap())
        .await
        .unwrap();
    fs::remove_file(&gone_file).unwrap();
    let error = protocol_error(
        agent
            .get_blob(&computer_name, &gone_ret.blob_handle, None, None)
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, 4018);
    assert_eq!(error.message, "Blob not accessible");
    assert_eq!(error.reason.as_deref(), Some("gone"));

    let revoked_file = stack.file("revoked.txt", b"x");
    let revoked_ret = agent
        .resolve_path(&computer_name, revoked_file.to_str().unwrap())
        .await
        .unwrap();
    // 根列表**非空**但移除目标 root（避免空根短路，真正走围栏拒绝）。
    let other_root = stack.tmp.path().join("other-root");
    fs::create_dir_all(&other_root).unwrap();
    stack.write_settings(json!({"readRoots": [other_root.to_string_lossy()]}));
    let error = protocol_error(
        agent
            .get_blob(&computer_name, &revoked_ret.blob_handle, None, None)
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, 4018);
    assert_eq!(error.message, "Blob not accessible");
    assert_eq!(error.reason.as_deref(), Some("forbidden"));

    // 新 resolve 同样被拒（4020 forbidden）。
    let error = protocol_error(
        agent
            .resolve_path(&computer_name, revoked_file.to_str().unwrap())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, 4020);
    assert_eq!(error.reason.as_deref(), Some("forbidden"));
}

/// sync/async parity：后台线程跑真实 Server + Computer，主线程用 `SyncSmcpAgent` 驱动同一链路。
#[test]
fn sync_agent_resolve_path_to_get_blob_parity() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(String, String, String, PathBuf)>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let server = TestServer::start().await.unwrap();
            let stack = Stack::new();
            let office = generate_office_id();
            let computer_name = generate_computer_name();
            let computer = stack.computer(&computer_name);
            computer.boot_up().await.unwrap();
            computer
                .connect_socketio(
                    server.url(),
                    ConnectOptions {
                        auth_payload: Some(auth_dict(SECRET)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            computer.join_office(&office, &computer_name).await.unwrap();
            let file = stack.file("sync.txt", b"sync-parity");
            ready_tx
                .send((server.url().to_string(), office, computer_name, file))
                .unwrap();
            // 保持 server/computer 存活，直到主线程完成断言并发送关闭信号。
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
    });

    let (url, office, computer_name, file) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stack ready");
    let auth = DefaultAuthProvider::new("sync-agent".to_string(), office.clone())
        .with_api_key(SECRET.to_string());
    let mut agent = SyncSmcpAgent::new(auth, SmcpAgentConfig::default()).unwrap();
    agent.connect(&url).unwrap();
    agent.join_office("sync-agent").unwrap();
    std::thread::sleep(Duration::from_millis(400));

    let ret = agent
        .resolve_path(&computer_name, file.to_str().unwrap())
        .unwrap();
    assert_eq!(ret.total_size, 11);
    assert_eq!(ret.mime_type, "text/plain");
    assert_eq!(ret.sha256, smcp::utils::hash::sha256_hex(b"sync-parity"));
    assert!(ret.mtime != 0);
    assert!(matches!(
        decode_blob_handle(&ret.blob_handle).unwrap(),
        DecodedHandle::Path(_)
    ));

    let chunk = agent
        .get_blob(&computer_name, &ret.blob_handle, None, Some(100))
        .unwrap();
    assert!(chunk.eof);
    assert_eq!(chunk.total_size, ret.total_size);
    assert_eq!(chunk.sha256, ret.sha256);
    let data = base64::engine::general_purpose::STANDARD
        .decode(chunk.blob.as_bytes())
        .unwrap();
    assert_eq!(data, b"sync-parity");

    stop_tx.send(()).unwrap();
    handle.join().unwrap();
}
