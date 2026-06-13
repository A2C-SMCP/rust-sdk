//! HS-01 (#21) 服务端版本握手中间件 / Server version-handshake middleware。
//!
//! 一个 [`hyper::service::Service`] 包裹器，置于 socketioxide 服务**之外**（最外层），在 Engine.IO
//! HTTP 握手到达 Socket.IO 业务层**之前**校验连接 URL 的 `a2c_version`：
//! - **缺失** / **非法（无法解析）** → HTTP 400 + flat `ErrorPayload`（`code=400`），**不含**
//!   `X-A2C-Error-Code` header；
//! - **不兼容** → HTTP 400 + flat `ErrorPayload`（`code=4008`，顶层平铺 4 个版本字段）+
//!   `X-A2C-Error-Code: 4008` header；
//! - **兼容** → 透传给内层（socketioxide）。
//!
//! 三态码空间与 header 触发条件**逐字段对齐 Python 参考** `a2c_smcp/server/middleware.py`
//! （`check_a2c_version` + `_mismatch_body` + `_build_rejection`）：missing/invalid 用 `400`，
//! 仅 mismatch 用 `4008` 并附 header。
//!
//! ⚠️ **路径**：gate 的是 Engine.IO 的 **HTTP 挂载路径**（socketioxide 默认 `/socket.io/`），
//! **不是** Socket.IO 应用命名空间 `/smcp`——后者是协议常量，前者是 SDK/部署决定（移植头号坑）。
//!
//! ⚠️ **双 scope**：hyper 下 polling(GET) 与 websocket-upgrade(GET+Upgrade) 都是同一条 HTTP
//! 请求流，本中间件按「路径 + query `a2c_version`」统一拦截，天然覆盖两条入口。由于 hyper 永远
//! 能对 WS 升级请求返回 HTTP 400 denial-response，**服务端无需发 WS close 4900**——`4900` 仅供
//! 无法返回 denial-response 的承载栈（见 [`smcp::WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE`]）。
//!
//! ⚠️ **trait 选择**：实现的是 `hyper::service::Service`（而非 `tower::Service`）——hyper 的
//! `serve_connection` 要求 hyper service，且 socketioxide 的 `SocketIoService` 本身即同时实现
//! tower 与 hyper 两套 Service，本中间件按 hyper 路径包裹它，避免 `TowerToHyperService` 桥接与
//! tower `util` feature 依赖。功能上仍是「socketioxide 之前的一层握手 gate」。
//!
//! 协议依据 / Protocol: `a2c-smcp-protocol` versioning.md §连接握手流程 + error-handling.md §4008。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes};
use hyper::service::Service;
use hyper::{header, Request, Response, StatusCode};

use smcp::{is_compatible, ErrorPayload, ProtocolVersion, PROTOCOL_VERSION};

/// 错误类型擦除后的统一 body 错误 / Type-erased body error。
type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// 中间件统一输出 body（拒绝分支与透传分支共用）/ Unified response body type.
type RespBody = BoxBody<Bytes, BoxError>;

/// 4008 诊断 header 名 / Diagnostic header carrying the 4008 code（仅 mismatch 时附带）。
const X_A2C_ERROR_CODE_HEADER: &str = "x-a2c-error-code";
/// 默认 Engine.IO HTTP 挂载路径 / Default Engine.IO HTTP mount path（socketioxide 默认）。
pub const DEFAULT_SOCKETIO_PATH: &str = "/socket.io/";

/// 版本握手中间件配置 / Version-handshake middleware config。
#[derive(Clone, Debug)]
pub struct VersionHandshakeConfig {
    /// 服务端协议版本（用于兼容性判定与 4008 body 的 `server_version`/min/max 派生）。
    pub server_version: ProtocolVersion,
    /// Engine.IO HTTP 挂载路径前缀（默认 [`DEFAULT_SOCKETIO_PATH`]）；仅此前缀的请求被 gate。
    pub socketio_path: String,
    /// 是否启用（默认 `true`）；`false` 时全部透传。
    pub enabled: bool,
}

impl Default for VersionHandshakeConfig {
    fn default() -> Self {
        Self {
            server_version: ProtocolVersion::parse(PROTOCOL_VERSION)
                .expect("PROTOCOL_VERSION 必须是合法 3 段版本"),
            socketio_path: DEFAULT_SOCKETIO_PATH.to_string(),
            enabled: true,
        }
    }
}

/// 版本握手中间件 service / Version-handshake middleware service。
///
/// 包裹内层 hyper service（通常是 socketioxide 的 `SocketIoService`），实现
/// [`hyper::service::Service`]。用 [`VersionHandshakeService::new`] 构造。
#[derive(Clone, Debug)]
pub struct VersionHandshakeService<S> {
    inner: S,
    config: Arc<VersionHandshakeConfig>,
}

impl<S> VersionHandshakeService<S> {
    /// 用内层 service 与配置构造 / Construct from the inner service and config。
    pub fn new(inner: S, config: VersionHandshakeConfig) -> Self {
        Self {
            inner,
            config: Arc::new(config),
        }
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for VersionHandshakeService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
    ResBody: Body<Data = Bytes> + Send + Sync + 'static,
    ResBody::Error: Into<BoxError>,
{
    type Response = Response<RespBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<ReqBody>) -> Self::Future {
        if let Some(resp) = check_version(&req, &self.config) {
            // 短路拒绝：不调用内层。
            return Box::pin(async move { Ok(resp) });
        }
        // 透传：把内层 body 装箱到统一类型。
        let fut = self.inner.call(req);
        Box::pin(async move {
            let resp = fut.await?;
            Ok(resp.map(|b| b.map_err(|e| e.into()).boxed()))
        })
    }
}

/// 校验请求；返回 `Some(拒绝响应)` 或 `None`（放行）。
fn check_version<B>(req: &Request<B>, cfg: &VersionHandshakeConfig) -> Option<Response<RespBody>> {
    if !cfg.enabled {
        return None;
    }
    // 仅 gate Engine.IO HTTP 路径；其它（/、/health 等）放行。
    if !req.uri().path().starts_with(&cfg.socketio_path) {
        return None;
    }
    let query = req.uri().query().unwrap_or("");
    // 取首个非空 `a2c_version`（空值视为缺失，对齐 Python `if not raw`）。复用协议层
    // `extract_a2c_version`——与会话落库（handler 侧）共用同一套 query 解析，消除重复实现。
    let raw = smcp::utils::handshake::extract_a2c_version(query);
    match raw {
        None => Some(reject(
            &ErrorPayload::new(400, "Missing a2c_version query parameter"),
            false,
        )),
        Some(raw) => match ProtocolVersion::parse(&raw) {
            Err(e) => Some(reject(
                &ErrorPayload::new(400, format!("Invalid a2c_version: {e}")),
                false,
            )),
            Ok(client) => {
                if is_compatible(&client, &cfg.server_version) {
                    None
                } else {
                    Some(reject(
                        &ErrorPayload::version_mismatch(&client, &cfg.server_version),
                        true,
                    ))
                }
            }
        },
    }
}

/// 构造 HTTP 400 拒绝响应 / Build the HTTP 400 rejection response。
///
/// `with_a2c_header` 为 `true`（仅 mismatch / code=4008）时附 `X-A2C-Error-Code: 4008`，
/// 对齐 Python `_build_rejection`（仅 `code == 4008` 时加该 header）。
fn reject(payload: &ErrorPayload, with_a2c_header: bool) -> Response<RespBody> {
    let body = serde_json::to_vec(payload)
        .unwrap_or_else(|_| br#"{"code":500,"message":"error serialization failed"}"#.to_vec());
    let len = body.len();
    let mut builder = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, len.to_string());
    if with_a2c_header {
        builder = builder.header(X_A2C_ERROR_CODE_HEADER, "4008");
    }
    let body: RespBody = Full::new(Bytes::from(body))
        .map_err(|e: std::convert::Infallible| -> BoxError { match e {} })
        .boxed();
    builder.body(body).expect("构造 400 响应不应失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内层桩 service：恒返回 200 "OK"（body 为 `Full<Bytes>`，Error=Infallible: Into<BoxError>）。
    #[derive(Clone, Debug)]
    struct DummyOk;

    impl Service<Request<Full<Bytes>>> for DummyOk {
        type Response = Response<Full<Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn call(&self, _req: Request<Full<Bytes>>) -> Self::Future {
            std::future::ready(Ok(Response::new(Full::new(Bytes::from("OK")))))
        }
    }

    fn svc() -> VersionHandshakeService<DummyOk> {
        VersionHandshakeService::new(DummyOk, VersionHandshakeConfig::default())
    }

    fn req(uri: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn body_json(
        resp: Response<RespBody>,
    ) -> (StatusCode, Option<String>, serde_json::Value) {
        let status = resp.status();
        let hdr = resp
            .headers()
            .get(X_A2C_ERROR_CODE_HEADER)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, hdr, json)
    }

    #[tokio::test]
    async fn test_missing_version_400_no_header() {
        let s = svc();
        let r = s
            .call(req("http://h/socket.io/?EIO=4&transport=polling"))
            .await
            .unwrap();
        let (status, hdr, json) = body_json(r).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], 400);
        assert!(hdr.is_none(), "missing 不应带 X-A2C-Error-Code");
        assert!(json.get("error").is_none(), "flat，无嵌套 envelope");
    }

    #[tokio::test]
    async fn test_invalid_version_400_no_header() {
        let s = svc();
        let r = s
            .call(req("http://h/socket.io/?transport=polling&a2c_version=abc"))
            .await
            .unwrap();
        let (status, hdr, json) = body_json(r).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], 400);
        assert!(hdr.is_none());
    }

    #[tokio::test]
    async fn test_incompatible_version_4008_with_header_and_fields() {
        // server 默认 0.2.0；client 0.1.0 → 不兼容
        let s = svc();
        let r = s
            .call(req(
                "http://h/socket.io/?transport=websocket&a2c_version=0.1.0",
            ))
            .await
            .unwrap();
        let (status, hdr, json) = body_json(r).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], 4008);
        assert_eq!(hdr.as_deref(), Some("4008"));
        assert_eq!(json["server_version"], "0.2.0");
        assert_eq!(json["client_version"], "0.1.0");
        assert_eq!(json["min_supported"], "0.2.0");
        assert_eq!(json["max_supported"], "0.2.999");
        assert!(json.get("error").is_none());
    }

    #[tokio::test]
    async fn test_compatible_version_passthrough() {
        let s = svc();
        // 同 minor 不同 patch 仍兼容
        for v in ["0.2.0", "0.2.5", "0.2.999"] {
            let r = s
                .call(req(&format!(
                    "http://h/socket.io/?transport=polling&a2c_version={v}"
                )))
                .await
                .unwrap();
            let status = r.status();
            let bytes = r.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(status, StatusCode::OK, "{v} 应放行");
            assert_eq!(&bytes[..], b"OK");
        }
    }

    #[tokio::test]
    async fn test_non_socketio_path_passthrough() {
        let s = svc();
        // 非 engine.io 路径即使缺版本也放行
        let r = s.call(req("http://h/health")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_disabled_passthrough() {
        let cfg = VersionHandshakeConfig {
            enabled: false,
            ..Default::default()
        };
        let s = VersionHandshakeService::new(DummyOk, cfg);
        let r = s
            .call(req("http://h/socket.io/?transport=polling"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
}
