//! #224: real server membership and the executable REPL must agree on identity.
#![cfg(feature = "cli")]

use std::{process::Stdio, sync::Arc, time::Duration};

use http_body_util::Full;
use hyper::body::Bytes;
use smcp_computer::{
    cli::commands::{CliConfig, CommandHandler},
    computer::{Computer, SilentSession},
    errors::ComputerError,
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
        .with_blob_cache_root(home.path().join(name).join("blob")),
        CliConfig {
            // A rename must reuse the actual interactive URL, not this startup default.
            url: Some("http://127.0.0.1:1".into()),
            namespace: "/smcp".into(),
            auth: Some("join-test-secret".into()),
            headers: Some("x-cli-join:preserved".into()),
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
    assert!(error.to_string().contains("4105"), "{error}");
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
        assert!(output.contains("4105"), "{output}");
        assert!(server.members("office-b").is_empty());
        assert_eq!(server.members("taken").len(), 1);
        repl.send("socket join retry occupied").await;
        repl.until("Joined office:").await;
        assert_eq!(server.members("retry")[0].name, "occupied");
        repl.child.kill().await.unwrap();
        blocker.disconnect_socketio().await.unwrap();
    }
}
