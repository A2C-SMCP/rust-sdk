//! Manual OAuth acceptance driver for the Atlassian Rovo MCP Server.
//!
//! This example intentionally prints only redacted PASS/FAIL records. It never
//! prints authorization URLs, callback codes, state, or stored credentials.

use async_trait::async_trait;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use smcp_computer::inputs::{OsKeyring, SecretStore};
use smcp_computer::mcp_clients::bundle_id::BundleId;
use smcp_computer::mcp_clients::{
    HttpServerConfig, HttpServerParameters, MCPServerConfig, MCPServerManager,
};
use smcp_computer::{
    OAuthBeginRequest, OAuthCallback, OAuthCancellation, OAuthCancellationReason, OAuthClientMode,
    OAuthClientRegistration, OAuthCredentialKey, OAuthCredentialStore, OAuthCredentialStoreError,
    OAuthFlowOutcome, OAuthOptions, OAuthStatus,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::Instant;
use url::Url;

const RESOURCE_URL: &str = "https://mcp.atlassian.com/v1/mcp/authv2";
const BUNDLE_ID: &str = "oauth-atlassian-uat";
const READ_ONLY_TOOL: &str = "getAccessibleAtlassianResources";
const DEFAULT_CALLBACK_PORT: u16 = 3334;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(9 * 60);

type UatResult<T> = Result<T, &'static str>;

struct KeyringOAuthCredentialStore {
    secrets: SecretStore,
}

impl KeyringOAuthCredentialStore {
    fn new() -> UatResult<Self> {
        let secrets = SecretStore::with_backend("a2c-computer-oauth-uat", Box::new(OsKeyring));
        if !secrets.available() {
            return Err("credential-vault-unavailable");
        }
        Ok(Self { secrets })
    }
}

#[async_trait]
impl OAuthCredentialStore for KeyringOAuthCredentialStore {
    async fn load(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<Option<String>, OAuthCredentialStoreError> {
        if !self.secrets.available() {
            return Err(OAuthCredentialStoreError::Unavailable);
        }
        Ok(self.secrets.get(&key.stable_id()))
    }

    async fn save(
        &self,
        key: &OAuthCredentialKey,
        value: &str,
    ) -> Result<(), OAuthCredentialStoreError> {
        self.secrets
            .set(&key.stable_id(), value)
            .then_some(())
            .ok_or(OAuthCredentialStoreError::OperationFailed)
    }

    async fn delete(&self, key: &OAuthCredentialKey) -> Result<(), OAuthCredentialStoreError> {
        if !self.secrets.available() {
            return Err(OAuthCredentialStoreError::Unavailable);
        }
        let id = key.stable_id();
        if self.secrets.get(&id).is_none() || self.secrets.delete(&id) {
            Ok(())
        } else {
            Err(OAuthCredentialStoreError::OperationFailed)
        }
    }
}

enum CallbackEvent {
    Complete(OAuthCallback),
    Cancel(OAuthCancellation),
}

#[tokio::main]
async fn main() {
    let phase = env::args().nth(1).unwrap_or_default();
    let result = match phase.as_str() {
        "authorize" => authorize().await,
        "resume" => resume().await,
        "clear" => clear().await,
        _ => Err("usage: expected authorize, resume, or clear"),
    };

    if let Err(stage) = result {
        eprintln!("UAT_RESULT: FAIL stage={stage}");
        std::process::exit(1);
    }
}

async fn authorize() -> UatResult<()> {
    let port = callback_port()?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| "callback-bind")?;
    let callback_uri = format!("http://127.0.0.1:{port}/callback");
    let (manager, bundle_id) = configured_manager().await?;

    if matches!(
        manager
            .oauth_status(&bundle_id)
            .await
            .map_err(|_| "status-before-authorize")?,
        OAuthStatus::Authorized { .. }
    ) {
        return Err("already-authorized-run-clear-first");
    }

    let launch = manager
        .begin_oauth(
            &bundle_id,
            OAuthBeginRequest {
                redirect_uri: callback_uri,
                required_scope: None,
            },
        )
        .await
        .map_err(|_| "begin-oauth")?;
    open_browser(&launch.authorization_url)?;
    println!("UAT_RESULT: PASS browser-opened");

    let callback = match receive_callback(listener, &launch.state).await {
        Ok(CallbackEvent::Complete(callback)) => callback,
        Ok(CallbackEvent::Cancel(cancellation)) => {
            let cancellation_result =
                cancel_pending_oauth(&manager, &bundle_id, cancellation).await;
            let _ = manager.close().await;
            cancellation_result?;
            return Err("authorization-denied");
        }
        Err("callback-timeout") => {
            let cancellation_result = cancel_pending_oauth(
                &manager,
                &bundle_id,
                OAuthCancellation {
                    state: launch.state,
                    issuer: None,
                    reason: OAuthCancellationReason::Timeout,
                },
            )
            .await;
            let _ = manager.close().await;
            cancellation_result?;
            return Err("callback-timeout");
        }
        Err(stage) => {
            let cancellation_result = cancel_pending_oauth(
                &manager,
                &bundle_id,
                OAuthCancellation {
                    state: launch.state,
                    issuer: None,
                    reason: OAuthCancellationReason::Cancelled,
                },
            )
            .await;
            let _ = manager.close().await;
            cancellation_result?;
            return Err(stage);
        }
    };
    let outcome = manager
        .complete_oauth(&bundle_id, callback)
        .await
        .map_err(|_| "complete-oauth")?;
    validate_authorization_outcome(outcome)?;
    assert_authorized(&manager, &bundle_id).await?;
    println!("UAT_RESULT: PASS authorized");

    validate_read_only_path(&manager, &bundle_id).await?;
    manager.close().await.map_err(|_| "manager-close")?;
    println!("UAT_RESULT: PASS phase=authorize");
    Ok(())
}

fn validate_authorization_outcome(outcome: OAuthFlowOutcome) -> UatResult<()> {
    match outcome {
        OAuthFlowOutcome::Authorized { scopes } if !scopes.is_empty() => Ok(()),
        _ => Err("complete-oauth-outcome"),
    }
}

async fn cancel_pending_oauth(
    manager: &MCPServerManager,
    bundle_id: &BundleId,
    cancellation: OAuthCancellation,
) -> UatResult<()> {
    let expected_reason = cancellation.reason;
    let outcome = manager
        .cancel_oauth(bundle_id, cancellation)
        .await
        .map_err(|_| "cancel-oauth")?;
    validate_cancellation_outcome(outcome, expected_reason)
}

fn validate_cancellation_outcome(
    outcome: OAuthFlowOutcome,
    expected_reason: OAuthCancellationReason,
) -> UatResult<()> {
    match outcome {
        OAuthFlowOutcome::Terminated { reason, status }
            if reason == expected_reason
                && matches!(
                    status,
                    OAuthStatus::Unauthorized | OAuthStatus::Authorized { .. }
                ) =>
        {
            Ok(())
        }
        _ => Err("cancel-oauth-outcome"),
    }
}

async fn resume() -> UatResult<()> {
    let (manager, bundle_id) = configured_manager().await?;
    assert_authorized(&manager, &bundle_id).await?;
    println!("UAT_RESULT: PASS credentials-restored");

    validate_read_only_path(&manager, &bundle_id).await?;
    manager.close().await.map_err(|_| "manager-close")?;
    println!("UAT_RESULT: PASS phase=resume");
    Ok(())
}

async fn clear() -> UatResult<()> {
    let (manager, bundle_id) = configured_manager().await?;
    manager
        .clear_oauth(&bundle_id)
        .await
        .map_err(|_| "clear-oauth")?;
    if !matches!(
        manager
            .oauth_status(&bundle_id)
            .await
            .map_err(|_| "status-after-clear")?,
        OAuthStatus::Unauthorized
    ) {
        return Err("clear-did-not-remove-authorization");
    }
    manager.close().await.map_err(|_| "manager-close")?;
    println!("UAT_RESULT: PASS phase=clear status=Unauthorized");
    Ok(())
}

async fn configured_manager() -> UatResult<(MCPServerManager, BundleId)> {
    let bundle_id = BundleId::try_from(BUNDLE_ID).map_err(|_| "bundle-id")?;
    let store = KeyringOAuthCredentialStore::new()?;
    let manager = MCPServerManager::with_oauth_credential_store(Arc::new(store));
    let mut config = HttpServerConfig::new(
        "Atlassian Rovo OAuth UAT",
        HttpServerParameters {
            url: RESOURCE_URL.to_string(),
            headers: HashMap::new(),
        },
    );
    config.bundle_id = Some(bundle_id.clone());
    config.oauth = Some(OAuthOptions {
        scopes: vec![
            "read:me".to_string(),
            "read:account".to_string(),
            "offline_access".to_string(),
        ],
        client_name: Some("A2C SMCP Rust SDK UAT".to_string()),
        mode: OAuthClientMode::AuthorizationCode {
            registration: OAuthClientRegistration::Dynamic,
        },
    });
    manager
        .add_or_update_server(MCPServerConfig::Http(config))
        .await
        .map_err(|_| "configure-server")?;
    Ok((manager, bundle_id))
}

async fn assert_authorized(manager: &MCPServerManager, bundle_id: &BundleId) -> UatResult<()> {
    if matches!(
        manager
            .oauth_status(bundle_id)
            .await
            .map_err(|_| "oauth-status")?,
        OAuthStatus::Authorized { .. }
    ) {
        Ok(())
    } else {
        Err("status-not-authorized")
    }
}

async fn validate_read_only_path(
    manager: &MCPServerManager,
    bundle_id: &BundleId,
) -> UatResult<()> {
    manager
        .start_client_by_id(bundle_id)
        .await
        .map_err(|_| "initialize")?;
    println!("UAT_RESULT: PASS initialize");

    let tools = manager.list_available_tools().await;
    let expected_exposed_name = format!("{BUNDLE_ID}__{READ_ONLY_TOOL}");
    if !tools
        .iter()
        .any(|tool| tool.name.as_ref() == expected_exposed_name)
    {
        return Err("read-only-tool-not-discovered");
    }
    println!("UAT_RESULT: PASS tools-list count={}", tools.len());

    let result = manager
        .call_tool(
            bundle_id.as_str(),
            READ_ONLY_TOOL,
            serde_json::json!({}),
            Some(Duration::from_secs(30)),
        )
        .await
        .map_err(|_| "read-only-tool-call")?;
    if result.is_error == Some(true) {
        return Err("read-only-tool-returned-error");
    }
    println!("UAT_RESULT: PASS read-only-resource-query");
    Ok(())
}

async fn receive_callback(listener: TcpListener, expected_state: &str) -> UatResult<CallbackEvent> {
    receive_callback_until(listener, expected_state, CALLBACK_TIMEOUT).await
}

async fn receive_callback_until(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> UatResult<CallbackEvent> {
    let deadline = Instant::now() + timeout;
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    let expected_state = expected_state.to_owned();

    loop {
        tokio::select! {
            event = callback_rx.recv() => {
                return event.unwrap_or(Err("callback-response"));
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|_| "callback-accept")?;
                let callback_tx = callback_tx.clone();
                let expected_state = expected_state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        callback_response(
                            request,
                            callback_tx.clone(),
                            expected_state.clone(),
                        )
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false);
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err("callback-timeout");
            }
        }
    }
}

async fn callback_response(
    request: Request<Incoming>,
    callback_tx: mpsc::UnboundedSender<UatResult<CallbackEvent>>,
    expected_state: String,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let callback = parse_callback(&request, &expected_state);
    let (status, message) = match callback {
        Some(Ok(callback)) => {
            let _ = callback_tx.send(Ok(callback));
            (
                StatusCode::OK,
                "Authorization received. You may close this window.",
            )
        }
        Some(Err(_)) => (
            StatusCode::BAD_REQUEST,
            "Authorization callback was malformed. Return to the authorization page.",
        ),
        None => (
            StatusCode::BAD_REQUEST,
            "Authorization callback was invalid. Return to the terminal.",
        ),
    };
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(message.as_bytes())))
        .expect("static callback response must be valid"))
}

fn parse_callback<B>(
    request: &Request<B>,
    expected_state: &str,
) -> Option<UatResult<CallbackEvent>> {
    if request.method() != hyper::Method::GET || request.uri().path() != "/callback" {
        return None;
    }
    let callback_url = Url::parse(&format!("http://localhost{}", request.uri())).ok()?;
    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut issuer = None;
    let mut error_description_seen = false;
    let mut duplicate = false;
    let mut empty_protocol_value = false;
    for (key, value) in callback_url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            "iss" => &mut issuer,
            "error_description" => {
                duplicate |= error_description_seen;
                error_description_seen = true;
                continue;
            }
            _ => continue,
        };
        let value = value.into_owned();
        empty_protocol_value |= value.is_empty();
        if slot.replace(value).is_some() {
            duplicate = true;
        }
    }
    let state = state?;
    if state != expected_state {
        return None;
    }
    if duplicate || empty_protocol_value || (code.is_some() && error.is_some()) {
        return Some(Err("callback-invalid"));
    }
    match (code, error) {
        (Some(code), None) => Some(Ok(CallbackEvent::Complete(OAuthCallback {
            code,
            state,
            issuer,
        }))),
        (None, Some(error)) => Some(Ok(CallbackEvent::Cancel(OAuthCancellation {
            state,
            issuer,
            reason: if error == "access_denied" {
                OAuthCancellationReason::AccessDenied
            } else {
                OAuthCancellationReason::AuthorizationError
            },
        }))),
        (Some(_), Some(_)) => Some(Err("callback-invalid")),
        (None, None) => None,
    }
}

fn callback_port() -> UatResult<u16> {
    match env::var("A2C_OAUTH_UAT_PORT") {
        Ok(value) => value.parse().map_err(|_| "invalid-callback-port"),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_CALLBACK_PORT),
        Err(env::VarError::NotUnicode(_)) => Err("invalid-callback-port"),
    }
}

fn open_browser(authorization_url: &str) -> UatResult<()> {
    let mut command = browser_command(authorization_url).ok_or("browser-launch")?;
    let status = command.stdout(Stdio::null()).stderr(Stdio::null()).status();

    if status.map_err(|_| "browser-launch")?.success() {
        Ok(())
    } else {
        Err("browser-launch")
    }
}

fn browser_command(authorization_url: &str) -> Option<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(authorization_url);
        Some(command)
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", authorization_url]);
        Some(command)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = Command::new("xdg-open");
        command.arg(authorization_url);
        Some(command)
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = authorization_url;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn send_request(port: u16, target: &str) -> String {
        send_request_with_method(port, "GET", target).await
    }

    async fn send_request_with_method(port: u16, method: &str, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(
                format!(
                    "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn loopback_ignores_probe_then_accepts_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let receiver = tokio::spawn(receive_callback(listener, "secret-state"));

        let probe = send_request(port, "/favicon.ico").await;
        assert!(probe.starts_with("HTTP/1.1 400"));

        let wrong_method =
            send_request_with_method(port, "POST", "/callback?code=wrong-code&state=secret-state")
                .await;
        assert!(wrong_method.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let missing_state = send_request(port, "/callback?code=wrong-code").await;
        assert!(missing_state.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let wrong_state = send_request(port, "/callback?code=wrong-code&state=wrong-state").await;
        assert!(wrong_state.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let spoofed_denial =
            send_request(port, "/callback?error=access_denied&state=wrong-state").await;
        assert!(spoofed_denial.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let missing_code = send_request(port, "/callback?state=secret-state").await;
        assert!(missing_code.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        for malformed in [
            "/callback?code=&state=secret-state",
            "/callback?error=&state=secret-state",
            "/callback?code=value&state=secret-state&iss=",
        ] {
            let response = send_request(port, malformed).await;
            assert!(response.starts_with("HTTP/1.1 400"));
            assert!(!receiver.is_finished());
        }

        let duplicate =
            send_request(port, "/callback?code=first&code=second&state=secret-state").await;
        assert!(duplicate.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let conflicting = send_request(
            port,
            "/callback?code=wrong-code&error=access_denied&state=secret-state",
        )
        .await;
        assert!(conflicting.starts_with("HTTP/1.1 400"));
        assert!(!receiver.is_finished());

        let callback_response =
            send_request(port, "/callback?code=secret-code&state=secret-state").await;
        assert!(callback_response.starts_with("HTTP/1.1 200"));

        let callback = receiver.await.unwrap().unwrap();
        let CallbackEvent::Complete(callback) = callback else {
            panic!("valid code callback must complete authorization");
        };
        assert_eq!(callback.code, "secret-code");
        assert_eq!(callback.state, "secret-state");
    }

    #[test]
    fn callback_error_is_structured_without_retaining_description() {
        let request = Request::builder()
            .uri("/callback?error=access_denied&error_description=sensitive&state=secret")
            .body(())
            .unwrap();

        let Some(Ok(CallbackEvent::Cancel(cancellation))) = parse_callback(&request, "secret")
        else {
            panic!("access_denied must produce a structured cancellation");
        };
        assert_eq!(cancellation.reason, OAuthCancellationReason::AccessDenied);
        assert_eq!(cancellation.state, "secret");

        let provider_error = Request::builder()
            .uri("/callback?error=temporarily_unavailable&error_description=sensitive&state=secret")
            .body(())
            .unwrap();
        let Some(Ok(CallbackEvent::Cancel(cancellation))) =
            parse_callback(&provider_error, "secret")
        else {
            panic!("provider errors must produce a structured cancellation");
        };
        assert_eq!(
            cancellation.reason,
            OAuthCancellationReason::AuthorizationError
        );

        let wrong_state = Request::builder()
            .uri("/callback?code=secret-code&state=wrong")
            .body(())
            .unwrap();
        assert!(parse_callback(&wrong_state, "secret").is_none());
    }

    #[test]
    fn callback_rejects_duplicate_or_conflicting_protocol_parameters() {
        for uri in [
            "/callback?code=&state=secret",
            "/callback?error=&state=secret",
            "/callback?code=value&state=secret&iss=",
            "/callback?code=first&code=second&state=secret",
            "/callback?code=value&state=secret&state=secret",
            "/callback?code=value&error=access_denied&state=secret",
            "/callback?error=access_denied&state=secret&iss=https://a.example&iss=https://b.example",
        ] {
            let request = Request::builder().uri(uri).body(()).unwrap();
            assert!(matches!(
                parse_callback(&request, "secret"),
                Some(Err("callback-invalid")) | None
            ));
        }
    }

    #[tokio::test]
    async fn loopback_timeout_is_structured_without_accepting_a_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(matches!(
            receive_callback_until(listener, "secret", Duration::from_millis(10)).await,
            Err("callback-timeout")
        ));
    }

    #[test]
    fn browser_command_keeps_query_metacharacters_in_one_argument() {
        let url = "https://example.test/authorize?client_id=test&state=secret";
        let command = browser_command(url).expect("supported test platform");
        assert_eq!(
            command
                .get_args()
                .last()
                .map(|arg| arg.to_string_lossy().into_owned()),
            Some(url.to_string())
        );
        assert_ne!(
            command.get_program().to_string_lossy().to_ascii_lowercase(),
            "cmd"
        );
        assert_ne!(
            command.get_program().to_string_lossy().to_ascii_lowercase(),
            "cmd.exe"
        );
    }

    #[test]
    fn cancellation_outcome_requires_matching_terminated_reason_and_final_status() {
        assert!(validate_cancellation_outcome(
            OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::Timeout,
                status: OAuthStatus::Unauthorized,
            },
            OAuthCancellationReason::Timeout,
        )
        .is_ok());
        assert!(validate_cancellation_outcome(
            OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::Cancelled,
                status: OAuthStatus::Authorized {
                    scopes: vec!["read".to_string()],
                },
            },
            OAuthCancellationReason::Cancelled,
        )
        .is_ok());
        assert!(validate_cancellation_outcome(
            OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::AccessDenied,
                status: OAuthStatus::Unauthorized,
            },
            OAuthCancellationReason::Timeout,
        )
        .is_err());
        assert!(validate_cancellation_outcome(
            OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::Timeout,
                status: OAuthStatus::AuthorizationPending,
            },
            OAuthCancellationReason::Timeout,
        )
        .is_err());
        assert!(validate_cancellation_outcome(
            OAuthFlowOutcome::Authorized {
                scopes: vec!["read".to_string()],
            },
            OAuthCancellationReason::Timeout,
        )
        .is_err());
    }

    #[test]
    fn authorization_outcome_requires_authorized_with_granted_scopes() {
        assert!(
            validate_authorization_outcome(OAuthFlowOutcome::Authorized {
                scopes: vec!["read:me".to_string()],
            })
            .is_ok()
        );
        assert!(
            validate_authorization_outcome(OAuthFlowOutcome::Authorized { scopes: Vec::new() })
                .is_err()
        );
        assert!(
            validate_authorization_outcome(OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::Cancelled,
                status: OAuthStatus::Unauthorized,
            })
            .is_err()
        );
    }
}
