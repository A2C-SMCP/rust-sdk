//! #224: real server membership and the executable REPL must agree on identity.
#![cfg(feature = "cli")]

/// protocol#64：隔离的 user-config 环境（XDG-first），测试绝不读写真实 `$HOME`。
/// 后续统一抽共享 helper（本批未含）；`#[allow(dead_code)]` 兼容部分 feature 组合下用例被 cfg 掉的情况。
#[allow(dead_code)]
fn isolated_config_env(
    base: impl AsRef<std::path::Path>,
) -> std::collections::HashMap<String, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // 每次调用分配独立子目录：同一测试二进制内的多个 Computer、以及共享 temp_dir 的 e2e
    // 用例都不会复用同一身份（PR3 记录/回显身份后“两台电脑”前提仍然成立）。
    let base = base.as_ref().join(format!(
        "smcp-identity-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut env = std::collections::HashMap::new();
    env.insert("HOME".to_string(), base.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env
}

use std::{process::Stdio, sync::Arc, time::Duration};

use http_body_util::Full;
use hyper::body::Bytes;
use smcp_computer::{
    cli::commands::{CliConfig, CommandError, CommandHandler},
    computer::{Computer, SilentSession},
    errors::ComputerError,
    mcp_clients::manager::MCPServerManager,
    socketio_client::SmcpComputerClientBuilder,
};

use smcp_server_core::{SessionManager, SmcpServerBuilder};
use smcp_server_hyper::{VersionHandshakeConfig, VersionHandshakeService};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};
use tower::Layer;

/// Require both auth and a routing header, so reconnect must preserve both.
#[derive(Debug)]
struct ConnectionAuth;

#[async_trait::async_trait]
impl smcp_server_core::AuthenticationProvider for ConnectionAuth {
    async fn authenticate(
        &self,
        headers: &http::HeaderMap,
        auth: Option<&serde_json::Value>,
    ) -> Result<(), smcp_server_core::AuthError> {
        if headers.get("x-cli-join").and_then(|v| v.to_str().ok()) != Some("preserved") {
            return Err(smcp_server_core::AuthError::InvalidApiKey);
        }
        smcp_server_core::DefaultAuthenticationProvider::new(Some("join-test-secret".into()), None)
            .authenticate(headers, auth)
            .await
    }
}

struct Server {
    url: String,
    sessions: Arc<SessionManager>,
    io: socketioxide::SocketIo,
    task: JoinHandle<()>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let layer = SmcpServerBuilder::new()
            .with_auth_provider(Arc::new(ConnectionAuth))
            .build_layer()
            .unwrap();
        let sessions = layer.state.session_manager.clone();
        let io = layer.io.clone();
        let fallback = hyper::service::service_fn(|_| async {
            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(Bytes::new())))
        });
        let service = VersionHandshakeService::new(
            layer.layer.layer(fallback),
            VersionHandshakeConfig::default(),
        );
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = service.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self {
            url,
            sessions,
            io,
            task,
        }
    }

    fn members(&self, office: &str) -> Vec<smcp_server_core::SessionData> {
        self.sessions.get_sessions_in_office(&office.to_string())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn handler(name: &str, home: &TempDir) -> CommandHandler {
    handler_with_env(name, home, isolated_config_env(home.path().join(name)))
}

/// 与 [`handler`] 相同，但由调用方持有隔离 env（便于直接读身份文件）。
fn handler_with_env(
    name: &str,
    home: &TempDir,
    env: std::collections::HashMap<String, String>,
) -> CommandHandler {
    CommandHandler::new(
        Computer::new(
            name,
            SilentSession::new("join-test"),
            None,
            None,
            false,
            false,
        )
        .with_config_dir(home.path().join(name).join("config"))
        .with_skill_home(home.path().join(name).join("skills"))
        .with_blob_cache_root(home.path().join(name).join("blob"))
        .with_config_env(env),
        CliConfig {
            // A rename must reuse the actual interactive URL, not this startup default.
            url: Some("http://127.0.0.1:1".into()),
            namespace: "/smcp".into(),
            auth: Some("join-test-secret".into()),
            headers: Some("x-cli-join:preserved".into()),
            office_rejoin_budget_secs: None,
        },
    )
}

async fn connect(handler: &mut CommandHandler, server: &Server) {
    handler.computer.boot_up().await.unwrap();
    let config = handler.cli_config.clone();
    handler
        .connect_socketio(
            &server.url,
            &config.namespace,
            &config.auth,
            &config.headers,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn rename_changes_sid_and_cleans_old_membership() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let mut cli = handler("initial", &home);
    assert!(cli.join_socket_room("office-a", "alice").await.is_err());
    connect(&mut cli, &server).await;
    assert!(matches!(
        cli.computer.join_office("office-a", "alice").await,
        Err(ComputerError::IdentityMismatch { .. })
    ));
    cli.join_socket_room("office-a", "alice").await.unwrap();
    let first = server.members("office-a");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].name, "alice");
    cli.join_socket_room("office-a", "alice").await.unwrap();
    assert_eq!(server.members("office-a")[0].sid, first[0].sid);
    cli.join_socket_room("office-b", "alice").await.unwrap();
    assert!(server.members("office-a").is_empty());
    assert_eq!(server.members("office-b")[0].sid, first[0].sid);
    cli.join_socket_room("office-c", "bob").await.unwrap();
    assert!(server.members("office-b").is_empty());
    let renamed = server.members("office-c");
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].name, "bob");
    assert_ne!(renamed[0].sid, first[0].sid);
    cli.disconnect_socketio().await.unwrap();
}

#[tokio::test]
async fn shared_computer_rejects_rename_before_changing_membership() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let mut cli = handler("old", &home);
    connect(&mut cli, &server).await;
    cli.join_socket_room("office", "old").await.unwrap();
    let original_sid = server.members("office")[0].sid.clone();
    let alias = cli.computer.clone();
    assert!(cli.join_socket_room("new-office", "alice").await.is_err());
    assert_eq!(server.members("office")[0].sid, original_sid);
    assert!(server.members("new-office").is_empty());
    assert_eq!(alias.name(), "old");
    alias.join_office("office", alias.name()).await.unwrap();
    drop(alias);
    cli.join_socket_room("new-office", "alice").await.unwrap();
    assert!(server.members("office").is_empty());
    assert_eq!(server.members("new-office")[0].name, "alice");
    cli.disconnect_socketio().await.unwrap();
}

#[tokio::test]
async fn rejected_rename_keeps_new_connection_and_allows_retry() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let mut occupied = handler("alice", &home);
    connect(&mut occupied, &server).await;
    occupied.join_socket_room("taken", "alice").await.unwrap();
    let mut cli = handler("old", &home);
    connect(&mut cli, &server).await;
    cli.join_socket_room("old-office", "old").await.unwrap();
    let error = cli.join_socket_room("taken", "alice").await.unwrap_err();
    assert!(error.to_string().contains("4101"), "{error}");
    assert!(server.members("old-office").is_empty());
    assert_eq!(server.members("taken").len(), 1);
    assert_eq!(cli.computer.name(), "alice");
    assert!(cli.computer.has_socketio_client().await);
    cli.join_socket_room("available", "alice").await.unwrap();
    assert_eq!(server.members("available")[0].name, "alice");
    cli.disconnect_socketio().await.unwrap();
    occupied.disconnect_socketio().await.unwrap();
}

#[tokio::test]
async fn failed_reconnect_restores_name_without_claiming_membership() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let mut cli = handler("old", &home);
    connect(&mut cli, &server).await;
    // Close the namespace and listener: the replacement cannot connect.
    server.io.close().await;
    server.task.abort();
    assert!(timeout(
        Duration::from_secs(20),
        cli.join_socket_room("office", "new")
    )
    .await
    .unwrap()
    .is_err());
    assert_eq!(cli.computer.name(), "old");
    assert!(!cli.computer.has_socketio_client().await);
}

struct Repl {
    child: Child,
    input: ChildStdin,
    lines: mpsc::UnboundedReceiver<String>,
}

impl Repl {
    async fn start(server: &Server, home: &TempDir, auto_connect: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_smcp-computer"));
        command
            .args([
                "--no-color",
                "--auth",
                "join-test-secret",
                "--headers",
                "x-cli-join:preserved",
            ])
            .env("A2C_SKILL_HOME", home.path().join("skills"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if auto_connect {
            command.args(["--url", &server.url]);
        }
        command.arg("run");
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let (tx, lines) = mpsc::unbounded_channel();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out_tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = out_tx.send(line);
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
        let mut repl = Self {
            child,
            input,
            lines,
        };
        repl.until("Enter interactive mode").await;
        if !auto_connect {
            repl.send(&format!("socket connect {}", server.url)).await;
            repl.until("Connected to Socket.IO").await;
        }
        repl
    }

    async fn send(&mut self, line: &str) {
        self.input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    async fn until(&mut self, marker: &str) -> String {
        let mut output = String::new();
        timeout(Duration::from_secs(30), async {
            while let Some(line) = self.lines.recv().await {
                output.push_str(&line);
                output.push('\n');
                if line.contains(marker) {
                    return;
                }
            }
            panic!("CLI exited before {marker}: {output}");
        })
        .await
        .unwrap_or_else(|_| panic!("CLI timed out before {marker}: {output}"));
        output
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn executable_repl_reports_real_identity_and_never_false_join_success() {
    for auto_connect in [false, true] {
        let server = Server::start().await;
        let home = TempDir::new().unwrap();
        let blocker_home = TempDir::new().unwrap();
        let mut blocker = handler("occupied", &blocker_home);
        connect(&mut blocker, &server).await;
        blocker.join_socket_room("taken", "occupied").await.unwrap();
        let mut repl = Repl::start(&server, &home, auto_connect).await;
        repl.send("help socket").await;
        assert!(repl
            .until("rename reconnects")
            .await
            .contains("computer_name"));
        repl.send("socket join office-a alice").await;
        repl.until("Joined office:").await;
        let first = server.members("office-a");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "alice");
        repl.send("socket join office-b bob").await;
        repl.until("Joined office:").await;
        assert!(server.members("office-a").is_empty());
        assert_ne!(server.members("office-b")[0].sid, first[0].sid);
        repl.send("socket join taken occupied").await;
        let output = repl.until("命令执行失败").await;
        assert!(!output.contains("Joined office:"), "{output}");
        assert!(output.contains("4101"), "{output}");
        assert!(server.members("office-b").is_empty());
        assert_eq!(server.members("taken").len(), 1);
        repl.send("socket join retry occupied").await;
        repl.until("Joined office:").await;
        assert_eq!(server.members("retry")[0].name, "occupied");
        repl.child.kill().await.unwrap();
        blocker.disconnect_socketio().await.unwrap();
    }
}

/// protocol#64：连接态 `identity reset` 编排——退房 → 断开 → 重置 → 重连 → 回房；
/// 连接中直接调用 `Computer::reset_computer_id` 必须被拒绝（身份在连接内不可变）。
#[tokio::test]
async fn identity_reset_orchestrates_disconnect_and_rejoin() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let env = isolated_config_env(home.path().join("reset-comp"));
    let mut cli = handler_with_env("reset-comp", &home, env.clone());
    connect(&mut cli, &server).await;

    cli.join_socket_room("office-reset", "reset-comp")
        .await
        .unwrap();
    let path = smcp_computer::identity::computer_id_path(Some(&env));
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(
        smcp::is_canonical_computer_id(before.trim_end()),
        "{before}"
    );

    // 连接中直接重置被拒。
    assert!(matches!(
        cli.computer.reset_computer_id().await,
        Err(ComputerError::InvalidState(_))
    ));

    cli.reset_computer_identity().await.unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let new_id = after.trim_end().to_string();
    assert!(smcp::is_canonical_computer_id(&new_id), "{after}");
    assert_ne!(before.trim_end(), new_id, "文件必须换成新身份");
    assert!(cli.computer.has_socketio_client().await, "编排后必须已重连");

    // 等回房完成后，从 Server 侧确认在房 Computer 使用的是**新**身份。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let members = server.members("office-reset");
        if let Some(computer) = members.iter().find(|s| s.name == "reset-comp") {
            assert_eq!(
                computer.computer_id.as_deref(),
                Some(new_id.as_str()),
                "回房后 Server 应记录新身份"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "重连后未回房：{members:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// protocol#64：未连接时 `identity reset` 仅更新持久化值（不建立/改动连接）。
#[tokio::test]
async fn identity_reset_offline_only_updates_file() {
    let home = TempDir::new().unwrap();
    let env = isolated_config_env(home.path().join("idle"));
    let mut cli = handler_with_env("idle", &home, env.clone());
    let path = smcp_computer::identity::computer_id_path(Some(&env));

    let first = smcp_computer::identity::load_or_create_computer_id(Some(&env))
        .await
        .unwrap();
    cli.reset_computer_identity().await.unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert_ne!(after.trim_end(), first, "离线重置也必须换新身份");
    assert!(smcp::is_canonical_computer_id(after.trim_end()), "{after}");
    assert!(!cli.computer.has_socketio_client().await);
}

/// protocol#64：REPL `identity reset` 分发——命令被识别、身份文件原子轮换并输出确认行。
#[tokio::test]
async fn repl_identity_reset_rotates_identity() {
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let mut repl = Repl::start(&server, &home, true).await;

    let identity_path = home.path().join("config").join("a2c").join("computer-id");
    let before = std::fs::read_to_string(&identity_path).unwrap();

    repl.send("identity reset").await;
    let output = repl.until("Identity reset").await;
    assert!(output.contains("身份已重置"), "{output}");

    let after = std::fs::read_to_string(&identity_path).unwrap();
    assert_ne!(
        before.trim_end(),
        after.trim_end(),
        "REPL reset 必须轮换身份"
    );
    assert!(smcp::is_canonical_computer_id(after.trim_end()), "{after}");

    repl.send("quit").await;
}

/// protocol#64：连接不是经 `socket connect` 建立（无保存参数）⇒ `identity reset` 在改动任何
/// 状态前拒绝：仍在线、仍在房、身份文件不变。
#[tokio::test]
async fn identity_reset_without_replayable_args_refuses_before_changes() {
    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let server = Server::start().await;
    let home = TempDir::new().unwrap();
    let env = isolated_config_env(home.path().join("ext"));
    let mut cli = handler_with_env("ext-cli", &home, env.clone());

    // 外部低层 client 直装：不经过 `CommandHandler::connect_socketio` ⇒ 无 `ConnectionArgs`。
    let client = SmcpComputerClientBuilder::new(
        &server.url,
        Arc::new(tokio::sync::RwLock::new(None::<MCPServerManager>)),
        "ext-cli".to_string(),
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    )
    .auth_payload(serde_json::json!({"token": "join-test-secret"}))
    .headers(std::collections::HashMap::from([(
        "x-cli-join".to_string(),
        "preserved".to_string(),
    )]))
    .computer_id(ID)
    .connect()
    .await
    .unwrap();
    client.join_office("office-ext").await.unwrap();
    cli.computer
        .set_socketio_client(Arc::new(client))
        .await
        .unwrap();

    let path = smcp_computer::identity::computer_id_path(Some(&env));
    let before = std::fs::read(&path).ok();

    let error = cli
        .reset_computer_identity()
        .await
        .expect_err("无保存参数必须拒绝");
    assert!(
        matches!(error, CommandError::InvalidCommand(_)),
        "{error:?}"
    );
    assert!(cli.computer.has_socketio_client().await, "拒绝不得断开连接");
    assert_eq!(std::fs::read(&path).ok(), before, "拒绝不得改动身份文件");
    assert!(
        server
            .members("office-ext")
            .iter()
            .any(|session| session.name == "ext-cli"),
        "拒绝不得退房"
    );
}
