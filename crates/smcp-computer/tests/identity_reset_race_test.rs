/*!
 * protocol#64：生命周期门的**取消安全**回归。
 *
 * 场景（架构师终审 🔴）：宿主取消 `connect_socketio` 外层 future 后，detached 的 connect
 * 任务仍会继续「读身份 → 建连 → install」。若生命周期门随外层栈释放，同 handle 的
 * `reset_computer_id` 会在任务仍在途时成功并替换文件，随后旧身份 client 被装回槽位。
 *
 * 两个变体分别锁住门的**两半**：
 * 1. [`cancelled_connect_cannot_reset_and_install_stale_identity`]：握手永不完成（只 accept
 *    不回包）——reset 全程阻塞，握手超时任务结束后才可成功，且槽位保持为空。
 * 2. [`cancelled_connect_that_installs_late_blocks_reset_until_installed`]：握手延迟成功
 *    （gated proxy 扣住字节）——reset 先阻塞，install 完成后因槽位已有 client 返回
 *    `InvalidState`，期间身份文件不得被替换。
 */

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::errors::ComputerError;
use smcp_computer::identity;
use socketioxide::extract::SocketRef;
use socketioxide::SocketIo;
use tower::Layer;

fn isolated_env(base: &std::path::Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), base.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env
}

fn race_computer(tmp: &tempfile::TempDir, env: HashMap<String, String>) -> Computer<SilentSession> {
    Computer::new(
        "race-computer",
        SilentSession::new("race"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(tmp.path().join("skills"))
    .with_blob_cache_root(tmp.path().join("blob"))
    .with_config_env(env)
}

/// 裸 socketioxide `/smcp` 后端：握手字节由 gated proxy 扣住，放行后完成真实的命名空间连接。
async fn start_backend_namespace_server() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", |_: SocketRef| async {});

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let tio = hyper_util::rt::TokioIo::new(stream);
                        let svc = hyper_util::service::TowerToHyperService::new(service.clone());
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(tio, svc)
                                .with_upgrades()
                                .await;
                        });
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });
    (addr, shutdown_tx)
}

/// accept 后**扣住所有字节**，直到 release 才转发到真实后端——形成「TCP 已建立、命名空间
/// 握手延迟成功」的确定性窗口。socket.io 客户端可能按需新建多条 TCP 连接（engine.io
/// polling 的后续请求），因此 accept 循环不限条数：放行前建立的连接等待 `watch`，放行后的
/// 新连接直接转发（`watch` 不丢唤醒）。返回 (url, accepted_rx, release_tx)。
async fn start_gated_proxy(
    backend: SocketAddr,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::watch::Sender<bool>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut accepted_tx = Some(accepted_tx);
    tokio::spawn(async move {
        loop {
            let Ok((mut downstream, _)) = listener.accept().await else {
                return;
            };
            if let Some(tx) = accepted_tx.take() {
                let _ = tx.send(());
            }
            let mut release_rx = release_rx.clone();
            tokio::spawn(async move {
                while !*release_rx.borrow_and_update() {
                    if release_rx.changed().await.is_err() {
                        return;
                    }
                }
                if let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await {
                    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                }
            });
        }
    });
    (format!("http://{addr}"), accepted_rx, release_tx)
}

/// 变体 1：握手永不完成——reset 必须全程阻塞，任务因命名空间超时结束后才可成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_connect_cannot_reset_and_install_stale_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolated_env(tmp.path());
    let computer = race_computer(&tmp, env.clone());
    computer.boot_up().await.expect("boot computer");

    // 先物化一份身份，便于断言轮换。
    let before = identity::load_or_create_computer_id(Some(&env))
        .await
        .expect("materialize identity");

    // 只 accept、不回包：命名空间握手挂起，connect 任务在门内等待直到 namespace 超时。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let hold = tokio::spawn(async move {
        let mut sockets = Vec::new();
        let mut accepted_tx = Some(accepted_tx);
        while let Ok((stream, _)) = listener.accept().await {
            if let Some(tx) = accepted_tx.take() {
                let _ = tx.send(());
            }
            sockets.push(stream);
        }
    });
    let url = format!("http://{addr}");
    let options = ConnectOptions {
        namespace_connect_timeout: Duration::from_secs(3),
        ..Default::default()
    };

    // 与首次 accept 竞争：先到达的只能是 accept——connect 完成即说明握手没有挂起，前提不成立。
    let mut connect_fut = Box::pin(computer.connect_socketio(&url, options));
    tokio::select! {
        outcome = &mut connect_fut => panic!("握手挂起期间 connect 不得完成，实得 {outcome:?}"),
        accepted = accepted_rx => accepted.expect("listener 必须接受首个连接"),
    }
    // accept 已证明 detached connect 任务在途中（TCP 握手由任务内 builder 发起，守卫已被移入
    // 任务）。此刻丢弃外层 future，模拟宿主 `timeout` 取消——门仍由后台任务持有。
    drop(connect_fut);

    // 任务持门期间，reset **不得**成功（提前返回即说明门被外层取消释放）。
    let early =
        tokio::time::timeout(Duration::from_millis(300), computer.reset_computer_id()).await;
    assert!(
        early.is_err(),
        "在途 connect 任务持门期间 reset 必须阻塞，实得 {early:?}"
    );

    // 等握手超时（~3s）任务结束、门释放后，reset 才能成功并轮换身份。
    let new_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(computer_id) = computer.reset_computer_id().await {
                return computer_id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("握手超时后 reset 应可成功");
    assert_ne!(before, new_id, "reset 必须生成新身份");
    assert!(
        !computer.has_socketio_client().await,
        "被取消/失败的 connect 不得把 client 装入槽"
    );

    hold.abort();
    computer.shutdown().await.expect("shutdown");
}

/// 变体 2：取消外层 future 后握手才成功——install 半程同样在门的保护内。
///
/// 若门只覆盖到 spawn（不随任务持有），reset 会在 install 前成功并替换身份，随后旧身份
/// client 被装回——本用例断言 reset **先阻塞**、install 完成后返回 `InvalidState`，且身份
/// 文件保持原值。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_connect_that_installs_late_blocks_reset_until_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolated_env(tmp.path());
    let computer = race_computer(&tmp, env.clone());
    computer.boot_up().await.expect("boot computer");

    let before = identity::load_or_create_computer_id(Some(&env))
        .await
        .expect("materialize identity");

    let (backend_addr, backend_shutdown) = start_backend_namespace_server().await;
    let (url, accepted_rx, release_tx) = start_gated_proxy(backend_addr).await;
    let options = ConnectOptions {
        namespace_connect_timeout: Duration::from_secs(2),
        ..Default::default()
    };

    let mut connect_fut = Box::pin(computer.connect_socketio(&url, options));
    tokio::select! {
        outcome = &mut connect_fut => panic!("握手放行前 connect 不得完成，实得 {outcome:?}"),
        accepted = accepted_rx => accepted.expect("gated proxy 必须接受连接"),
    }
    // 取消外层 future：detached 任务仍在门内等被扣住的握手字节。
    drop(connect_fut);

    // 握手未放行 → 守卫由 detached 任务持有，reset 必须阻塞。
    let early =
        tokio::time::timeout(Duration::from_millis(300), computer.reset_computer_id()).await;
    assert!(
        early.is_err(),
        "握手挂起期间 reset 必须阻塞，实得 {early:?}"
    );

    // 放行握手 → detached 任务完成 connect → install（旧身份，但 reset 未成功，不存在混合态）。
    let _ = release_tx.send(true);
    let outcome = tokio::time::timeout(Duration::from_secs(10), computer.reset_computer_id())
        .await
        .expect("install 完成后 reset 不得再阻塞");
    match outcome {
        Err(ComputerError::InvalidState(reason)) => {
            assert!(reason.contains("while connected"), "{reason}");
        }
        other => panic!("install 完成后 reset 必须 InvalidState，实得 {other:?}"),
    }
    assert!(
        computer.has_socketio_client().await,
        "detached connect 应完成 install"
    );
    let after = identity::load_or_create_computer_id(Some(&env))
        .await
        .expect("reload identity");
    assert_eq!(after, before, "被拒绝的 reset 不得替换身份文件");

    computer.shutdown().await.expect("shutdown");
    let _ = backend_shutdown.send(());
}
