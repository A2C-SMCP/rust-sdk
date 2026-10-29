/*!
 * protocol#64：生命周期门的**取消安全**回归。
 *
 * 场景（架构师终审 🔴）：宿主取消 `connect_socketio` 外层 future 后，detached 的 connect
 * 任务仍会继续「读身份 → 建连 → install」。若生命周期门随外层栈释放，同 handle 的
 * `reset_computer_id` 会在任务仍在途时成功并替换文件，随后旧身份 client 被装回槽位。
 *
 * 本用例用「只 accept 不回包的 TCP 端点」把握手挂起，并以 listener **首次 accept** 作为
 * 同步点：accept 证明 detached 任务已在途中（且已把守卫移入任务）后才取消外层 future，
 * 之后 reset **必须**阻塞在门上；等握手超时、任务结束后 reset 才可成功。
 */

use std::collections::HashMap;
use std::time::Duration;

use smcp_computer::computer::{Computer, ConnectOptions, SilentSession};
use smcp_computer::identity;

fn isolated_env(base: &std::path::Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), base.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        base.join("xdg-config").display().to_string(),
    );
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_connect_cannot_reset_and_install_stale_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolated_env(tmp.path());
    let computer = Computer::new(
        "race-computer",
        SilentSession::new("race"),
        None,
        None,
        false,
        false,
    )
    .with_skill_home(tmp.path().join("skills"))
    .with_blob_cache_root(tmp.path().join("blob"))
    .with_config_env(env.clone());
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
