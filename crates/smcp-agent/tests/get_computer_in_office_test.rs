/*!
* 文件名: get_computer_in_office_test
* 作者: JQQ
* 描述: `AsyncSmcpAgent::get_computer_in_office`（protocol#66 单数便捷 API）的**端到端接线**测试 /
*       End-to-end wiring tests for the singular `get_computer_in_office` helper。
*
* 为什么需要真设施：纯函数 `response::parse_single_computer` 的单测只覆盖**投影逻辑**；「公共方法 →
* `list_room` 往返 → 投影 → 错误类型」的接线必须由真实 Socket.IO 往返证明。其中
* 「房内多于一台 Computer ⇒ 判服务端协议违规」是一条**合规服务端不可能产出**的分支
* （protocol#66 把服务端钉死为每 role 一席 ≤1 台），只能以**违规桩**模拟——这正是「不挑第一台、
* 如实报错」契约唯一的可执行证据。
*/

use std::net::SocketAddr;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data, SocketRef};
use socketioxide::SocketIo;
use tower::Layer;

use smcp_agent::{AsyncSmcpAgent, DefaultAuthProvider, SmcpAgentConfig, SmcpAgentError};

/// 违规桩：裸 socketioxide 服务端，对 `server:list_room` 恒回**指定台数**的 Computer 会话表。
///
/// 模拟违反「每 role 一席」的对端（protocol#66 下真实服务端不可产出）。
async fn spawn_list_room_server(computer_count: usize) -> String {
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid addr");

    let (layer, io) = SocketIo::new_layer();
    io.ns("/smcp", move |socket: SocketRef| {
        socket.on(
            "server:list_room",
            move |Data::<Value>(data): Data<Value>, ack: AckSender| async move {
                let office = data["office_id"].clone();
                let sessions: Vec<Value> = (0..computer_count)
                    .map(|i| {
                        json!({
                            "sid": format!("sid-{i}"),
                            "name": format!("computer-{i}"),
                            "role": "computer",
                            "office_id": office,
                        })
                    })
                    .collect();
                // 服务端不回空 ack 而是应答 ListRoomRet（与真实服务端同形：req_id 回显）。
                let _ = ack.send(&json!({
                    "sessions": sessions,
                    "req_id": data["req_id"],
                }));
            },
        );
    });

    let fallback = tower::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::<Bytes>::new(Bytes::new())))
    });
    let service = layer.layer(fallback);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
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
    });

    format!("http://127.0.0.1:{port}")
}

/// 建一个已连接、指向桩服务端的 Agent。
async fn connected_agent(url: &str, name: &str, office: &str) -> AsyncSmcpAgent {
    let auth = DefaultAuthProvider::new(name.to_string(), office.to_string())
        .with_api_key("test_secret".to_string());
    let config = SmcpAgentConfig::new()
        .with_default_timeout(5)
        .with_tool_call_timeout(5);
    let mut agent = AsyncSmcpAgent::new(auth, config);
    agent.connect(url).await.expect("connect to stub server");
    agent
}

/// 空房（0 台 Computer）⇒ `Ok(None)`；且**不挑第一台**的反面：无台可挑。
#[tokio::test]
async fn get_computer_in_office_returns_none_for_empty_room() {
    let url = spawn_list_room_server(0).await;
    let agent = connected_agent(&url, "agent-none", "office-none").await;

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent.get_computer_in_office("office-none"),
    )
    .await
    .expect("get_computer_in_office 必须有界返回")
    .expect("空房查询不得报错");

    assert!(result.is_none(), "房内无 Computer 应返回 None");
}

/// 单台 Computer ⇒ `Ok(Some(_))`，且投影出的就是**那一台**。
#[tokio::test]
async fn get_computer_in_office_returns_the_single_computer() {
    let url = spawn_list_room_server(1).await;
    let agent = connected_agent(&url, "agent-single", "office-single").await;

    let computer = tokio::time::timeout(
        Duration::from_secs(5),
        agent.get_computer_in_office("office-single"),
    )
    .await
    .expect("get_computer_in_office 必须有界返回")
    .expect("单台房查询不得报错")
    .expect("房内恰一台 Computer 应返回 Some");

    assert_eq!(computer.name, "computer-0");
}

/// 多于一台 Computer ⇒ **协议违规错误**（不挑第一台）。
///
/// 这是「不挑第一台」契约唯一的可执行证据：真实服务端被本仓修复钉死为 ≤1 台，仅违规桩可构造。
#[tokio::test]
async fn get_computer_in_office_flags_multi_computer_violation() {
    let url = spawn_list_room_server(2).await;
    let agent = connected_agent(&url, "agent-multi", "office-multi").await;

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        agent.get_computer_in_office("office-multi"),
    )
    .await
    .expect("get_computer_in_office 必须有界返回")
    .expect_err("多于一台 Computer 必须判服务端协议违规");

    match error {
        SmcpAgentError::TooManyComputersInOffice { office_id, count } => {
            assert_eq!(office_id, "office-multi");
            assert_eq!(count, 2);
        }
        other => panic!("expected TooManyComputersInOffice, got {other:?}"),
    }
}
