use super::*;
use crate::inputs::{InputResolutionError, SecretValueResolver};
use crate::mcp_clients::model::{
    HttpAuthPolicy, HttpServerConfig, HttpServerParameters, MCPServerConfig,
};
use crate::mcp_clients::{BundleId, MCPServerInput};
use crate::oauth::{OAuthClientMode, OAuthOptions, OAuthStatus};
use async_trait::async_trait;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

const TEST_EC_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgp9PptiYIX1DoplcU
CrXJICvftS6mTCVk+I+JynptjaShRANCAAT54hAudKCxTrTPlQUCSAHZtmOxl6fL
hSEGx6f7gFfatuW4qJ/SM6W4Yt7BxI4gJ30LDd0WPiDGALXZQYff2g7l
-----END PRIVATE KEY-----"#;

struct JwtSecretResolver;

#[async_trait]
impl SecretValueResolver for JwtSecretResolver {
    async fn resolve_secret(
        &self,
        def: &MCPServerInput,
    ) -> Result<Option<String>, InputResolutionError> {
        Ok((def.id() == "oauth-auto-jwt-private-key").then(|| TEST_EC_PRIVATE_KEY.to_string()))
    }
}

#[derive(Default)]
struct TlsOAuthState {
    anonymous_initializes: AtomicUsize,
    authorized_initializes: AtomicUsize,
    token_requests: AtomicUsize,
}

fn json_response(value: serde_json::Value) -> Response<Full<Bytes>> {
    Response::builder()
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(value.to_string())))
        .unwrap()
}

async fn tls_oauth_handler(
    request: Request<hyper::body::Incoming>,
    base_url: String,
    state: Arc<TlsOAuthState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let authorized = request
        .headers()
        .get("authorization")
        .is_some_and(|value| value == "Bearer oauth-auto-jwt-token");
    let body = request.into_body().collect().await.unwrap().to_bytes();

    let response = match (method, path.as_str()) {
        (Method::GET, "/.well-known/oauth-protected-resource/mcp")
        | (Method::GET, "/.well-known/oauth-protected-resource") => {
            json_response(serde_json::json!({
                "resource": format!("{base_url}/mcp"),
                "authorization_servers": [&base_url],
                "scopes_supported": ["tools.read"]
            }))
        }
        (Method::GET, "/.well-known/oauth-authorization-server")
        | (Method::GET, "/.well-known/oauth-authorization-server/mcp") => {
            json_response(serde_json::json!({
                "issuer": base_url.clone(),
                "authorization_endpoint": format!("{base_url}/authorize"),
                "token_endpoint": format!("{base_url}/token"),
                "grant_types_supported": ["client_credentials"],
                "token_endpoint_auth_methods_supported": ["private_key_jwt"],
                "token_endpoint_auth_signing_alg_values_supported": ["ES256"]
            }))
        }
        (Method::POST, "/token") => {
            let form: HashMap<String, String> =
                url::form_urlencoded::parse(&body).into_owned().collect();
            assert_eq!(
                form.get("grant_type").map(String::as_str),
                Some("client_credentials")
            );
            assert_eq!(
                form.get("client_assertion_type").map(String::as_str),
                Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
            );
            assert!(form
                .get("client_assertion")
                .is_some_and(|assertion| assertion.split('.').count() == 3));
            let expected_resource = format!("{base_url}/mcp");
            assert_eq!(
                form.get("resource").map(String::as_str),
                Some(expected_resource.as_str())
            );
            assert!(!form.contains_key("client_id"));
            state.token_requests.fetch_add(1, Ordering::SeqCst);
            json_response(serde_json::json!({
                "access_token": "oauth-auto-jwt-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "tools.read"
            }))
        }
        (Method::POST, "/mcp") => {
            let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let rpc_method = rpc.get("method").and_then(serde_json::Value::as_str);
            let startup_initialize = rpc_method == Some("initialize")
                && rpc["params"]["clientInfo"]["name"] == "a2c-smcp-rust";
            if startup_initialize {
                if authorized {
                    state.authorized_initializes.fetch_add(1, Ordering::SeqCst);
                } else {
                    state.anonymous_initializes.fetch_add(1, Ordering::SeqCst);
                }
            }
            if !authorized {
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(
                        "WWW-Authenticate",
                        format!(
                            "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource/mcp\""
                        ),
                    )
                    .body(Full::new(Bytes::new()))
                    .unwrap()
            } else if rpc_method.is_some_and(|method| method.starts_with("notifications/")) {
                Response::builder()
                    .status(StatusCode::ACCEPTED)
                    .body(Full::new(Bytes::new()))
                    .unwrap()
            } else {
                let id = rpc.get("id").cloned().unwrap_or(serde_json::json!(0));
                json_response(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "auto-jwt-mcp", "version": "1.0.0"}
                    }
                }))
            }
        }
        (Method::GET, "/mcp") => Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::new()))
            .unwrap(),
        (Method::DELETE, "/mcp") => Response::new(Full::new(Bytes::new())),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::new()))
            .unwrap(),
    };
    Ok(response)
}

async fn spawn_tls_oauth_mcp() -> (String, Arc<TlsOAuthState>) {
    const TLS_CERT_PEM: &[u8] = include_bytes!("../../tests/fixtures/oauth_tls_cert.pem");
    const TLS_KEY_PEM: &[u8] = include_bytes!("../../tests/fixtures/oauth_tls_key.pem");

    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut certificate_reader = std::io::BufReader::new(TLS_CERT_PEM);
    let certificate_chain = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut key_reader = std::io::BufReader::new(TLS_KEY_PEM);
    let private_key = rustls_pemfile::private_key(&mut key_reader)
        .unwrap()
        .unwrap();
    let tls_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificate_chain, private_key)
        .unwrap();
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let state = Arc::new(TlsOAuthState::default());
    let server_base_url = base_url.clone();
    let server_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = tls_acceptor.clone();
            let base_url = server_base_url.clone();
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |request| {
                    tls_oauth_handler(request, base_url.clone(), Arc::clone(&state))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (base_url, state)
}

#[tokio::test]
async fn auto_private_key_jwt_starts_in_one_call_over_real_tls() {
    const TLS_CA_CERT_PEM: &[u8] = include_bytes!("../../tests/fixtures/oauth_tls_ca_cert.pem");
    let (base_url, state) = spawn_tls_oauth_mcp().await;
    let bundle_id = BundleId::try_from("oauth-auto-private-key-jwt").unwrap();
    let manager = MCPServerManager::new();
    manager
        .set_test_http_root_certificates(vec![
            reqwest::Certificate::from_pem(TLS_CA_CERT_PEM).unwrap()
        ])
        .await;
    manager
        .set_secret_resolver(Some(Arc::new(JwtSecretResolver)))
        .await;
    let mut config = HttpServerConfig::new(
        "oauth-auto-private-key-jwt",
        HttpServerParameters {
            url: format!("{base_url}/mcp"),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.auth_policy = Some(HttpAuthPolicy::Auto);
    config.oauth = Some(OAuthOptions {
        resource: None,
        scopes: vec!["tools.read".to_string()],
        client_name: None,
        mode: OAuthClientMode::ClientCredentialsPrivateKeyJwt {
            client_id: "oauth-auto-jwt-client".to_string(),
            private_key_input: "oauth-auto-jwt-private-key".to_string(),
            algorithm: "ES256".to_string(),
            token_endpoint_audience: None,
        },
    });
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .unwrap();

    manager.start_client_by_id(&bundle_id).await.unwrap();

    assert_eq!(state.anonymous_initializes.load(Ordering::SeqCst), 1);
    assert_eq!(state.authorized_initializes.load(Ordering::SeqCst), 1);
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);
    assert!(matches!(
        manager.oauth_status(&bundle_id).await.unwrap(),
        OAuthStatus::Authorized { .. }
    ));
    let runtime = manager.get_server_status().await;
    assert_eq!(runtime.len(), 1);
    assert!(runtime[0].2);
    manager.close().await.unwrap();
}
