use super::{
    OAuthBeginRequest, OAuthCallback, OAuthCancellation, OAuthCancellationReason, OAuthError,
    OAuthFlowOutcome, OAuthLaunch,
};
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TERMINAL_OPEN: u8 = 0;
const TERMINAL_COMPLETING: u8 = 1;
const TERMINAL_CANCELLING: u8 = 2;

pub(crate) enum OAuthFlowCommand {
    Complete {
        callback: OAuthCallback,
        response: oneshot::Sender<Result<OAuthFlowOutcome, OAuthError>>,
    },
    CancelCallback {
        cancellation: OAuthCancellation,
        response: oneshot::Sender<Result<OAuthFlowOutcome, OAuthError>>,
    },
}

struct OAuthFlowInner {
    id: Uuid,
    request: OAuthBeginRequest,
    command_tx: mpsc::Sender<OAuthFlowCommand>,
    launch_rx: watch::Receiver<Option<Result<OAuthLaunch, OAuthError>>>,
    terminal_rx: watch::Receiver<Option<Result<OAuthFlowOutcome, OAuthError>>>,
    cancellation: CancellationToken,
    host_cancellation: StdMutex<Option<OAuthCancellationReason>>,
    expected_issuer: StdMutex<Option<String>>,
    terminal_claim: AtomicU8,
}

/// Host-owned handle for one interactive OAuth flow.
///
/// Creating a handle performs no provider I/O. The SDK starts discovery in a detached task after
/// registering the handle, so [`cancel`](Self::cancel) is usable before an [`OAuthLaunch`] exists.
/// Clones refer to the same flow and converge on the same terminal result.
#[derive(Clone)]
pub struct OAuthFlow {
    inner: Arc<OAuthFlowInner>,
}

impl fmt::Debug for OAuthFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlow")
            .field("id", &self.inner.id)
            .field("terminal", &self.is_terminal())
            .finish_non_exhaustive()
    }
}

impl OAuthFlow {
    pub(crate) fn new(request: OAuthBeginRequest) -> (Self, OAuthFlowDriver) {
        let (command_tx, command_rx) = mpsc::channel(2);
        let (launch_tx, launch_rx) = watch::channel(None);
        let (terminal_tx, terminal_rx) = watch::channel(None);
        let cancellation = CancellationToken::new();
        let inner = Arc::new(OAuthFlowInner {
            id: Uuid::new_v4(),
            request,
            command_tx,
            launch_rx,
            terminal_rx,
            cancellation,
            host_cancellation: StdMutex::new(None),
            expected_issuer: StdMutex::new(None),
            terminal_claim: AtomicU8::new(TERMINAL_OPEN),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            OAuthFlowDriver {
                inner,
                command_rx,
                launch_tx,
                terminal_tx,
                finished: false,
            },
        )
    }

    /// Wait until discovery, client setup, PKCE, and authorization URL generation complete.
    pub async fn launch(&self) -> Result<OAuthLaunch, OAuthError> {
        let mut rx = self.inner.launch_rx.clone();
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            rx.changed()
                .await
                .map_err(|_| OAuthError::Protocol(super::OAuthProtocolError::Internal))?;
        }
    }

    /// Submit the browser callback and wait for the single terminal outcome.
    pub async fn complete(&self, callback: OAuthCallback) -> Result<OAuthFlowOutcome, OAuthError> {
        let (response, result) = oneshot::channel();
        if self
            .inner
            .command_tx
            .send(OAuthFlowCommand::Complete { callback, response })
            .await
            .is_err()
        {
            return self.wait_terminal().await;
        }
        match result.await {
            Ok(result) => result,
            Err(_) => self.wait_terminal().await,
        }
    }

    /// Cancel from host lifecycle code, including before [`launch`](Self::launch) completes.
    ///
    /// Provider callback errors must use [`cancel_callback`](Self::cancel_callback), because they
    /// require state and issuer validation. Host cancellation accepts only `Cancelled` or
    /// `Timeout`.
    pub async fn cancel(
        &self,
        reason: OAuthCancellationReason,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        if !matches!(
            reason,
            OAuthCancellationReason::Cancelled | OAuthCancellationReason::Timeout
        ) {
            return Err(OAuthError::InvalidCancellationReason);
        }
        self.terminal().claim_cancellation(reason);
        self.wait_terminal().await
    }

    /// Submit a provider OAuth error callback after validating its opaque state and issuer.
    pub async fn cancel_callback(
        &self,
        cancellation: OAuthCancellation,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        let (response, result) = oneshot::channel();
        if self
            .inner
            .command_tx
            .send(OAuthFlowCommand::CancelCallback {
                cancellation,
                response,
            })
            .await
            .is_err()
        {
            return self.wait_terminal().await;
        }
        match result.await {
            Ok(result) => result,
            Err(_) => self.wait_terminal().await,
        }
    }

    pub(crate) async fn cancel_compat(
        &self,
        cancellation: OAuthCancellation,
    ) -> Result<OAuthFlowOutcome, OAuthError> {
        if matches!(
            cancellation.reason,
            OAuthCancellationReason::Cancelled | OAuthCancellationReason::Timeout
        ) {
            let launch = self.inner.launch_rx.borrow().clone();
            let Some(Ok(launch)) = launch else {
                return Err(OAuthError::StateMismatch);
            };
            if launch.state != cancellation.state {
                return Err(OAuthError::StateMismatch);
            }
            if let Some(issuer) = cancellation.issuer.as_deref() {
                let expected = self
                    .inner
                    .expected_issuer
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if expected.as_deref() != Some(issuer) {
                    return Err(OAuthError::IssuerMismatch);
                }
            }
            return self.cancel(cancellation.reason).await;
        }
        self.cancel_callback(cancellation).await
    }

    pub(crate) async fn wait_terminal(&self) -> Result<OAuthFlowOutcome, OAuthError> {
        let mut rx = self.inner.terminal_rx.clone();
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            rx.changed()
                .await
                .map_err(|_| OAuthError::Protocol(super::OAuthProtocolError::Internal))?;
        }
    }

    pub(crate) fn id(&self) -> Uuid {
        self.inner.id
    }

    pub(crate) fn request(&self) -> &OAuthBeginRequest {
        &self.inner.request
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.inner.terminal_rx.borrow().is_some()
    }

    pub(crate) fn is_cancelling(&self) -> bool {
        self.inner.terminal_claim.load(Ordering::Acquire) == TERMINAL_CANCELLING
    }

    fn terminal(&self) -> OAuthFlowTerminal {
        OAuthFlowTerminal {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Clone)]
pub(crate) struct OAuthFlowTerminal {
    inner: Arc<OAuthFlowInner>,
}

impl OAuthFlowTerminal {
    pub(crate) fn try_claim_completion(&self) -> bool {
        self.inner
            .terminal_claim
            .compare_exchange(
                TERMINAL_OPEN,
                TERMINAL_COMPLETING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn claim_cancellation(&self, reason: OAuthCancellationReason) -> bool {
        let mut current = self
            .inner
            .host_cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self
            .inner
            .terminal_claim
            .compare_exchange(
                TERMINAL_OPEN,
                TERMINAL_CANCELLING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            *current = Some(reason);
            self.inner.cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn cancellation_reason(&self) -> Option<OAuthCancellationReason> {
        (self.inner.terminal_claim.load(Ordering::Acquire) == TERMINAL_CANCELLING).then(|| {
            self.inner
                .host_cancellation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .unwrap_or(OAuthCancellationReason::Cancelled)
        })
    }

    pub(crate) fn claim_non_cancellation_or_reason(&self) -> Option<OAuthCancellationReason> {
        loop {
            match self.inner.terminal_claim.load(Ordering::Acquire) {
                TERMINAL_OPEN => {
                    if self
                        .inner
                        .terminal_claim
                        .compare_exchange(
                            TERMINAL_OPEN,
                            TERMINAL_COMPLETING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return None;
                    }
                }
                TERMINAL_COMPLETING => return None,
                TERMINAL_CANCELLING => {
                    return Some(
                        self.cancellation_reason()
                            .unwrap_or(OAuthCancellationReason::Cancelled),
                    )
                }
                _ => unreachable!("OAuth terminal claim contains an invalid state"),
            }
        }
    }
}

pub(crate) struct OAuthFlowDriver {
    inner: Arc<OAuthFlowInner>,
    command_rx: mpsc::Receiver<OAuthFlowCommand>,
    launch_tx: watch::Sender<Option<Result<OAuthLaunch, OAuthError>>>,
    terminal_tx: watch::Sender<Option<Result<OAuthFlowOutcome, OAuthError>>>,
    finished: bool,
}

impl OAuthFlowDriver {
    pub(crate) fn request(&self) -> &OAuthBeginRequest {
        &self.inner.request
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.inner.cancellation.clone()
    }

    pub(crate) fn terminal(&self) -> OAuthFlowTerminal {
        OAuthFlowTerminal {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn host_cancellation_reason(&self) -> OAuthCancellationReason {
        self.inner
            .host_cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unwrap_or(OAuthCancellationReason::Cancelled)
    }

    pub(crate) async fn next_command(&mut self) -> Option<OAuthFlowCommand> {
        self.command_rx.recv().await
    }

    pub(crate) fn publish_launch(
        &self,
        result: Result<OAuthLaunch, OAuthError>,
        expected_issuer: Option<String>,
    ) -> bool {
        let _cancellation_guard = self
            .inner
            .host_cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.inner.terminal_claim.load(Ordering::Acquire) == TERMINAL_CANCELLING {
            return false;
        }
        *self
            .inner
            .expected_issuer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = expected_issuer;
        self.launch_tx.send_replace(Some(result));
        true
    }

    pub(crate) fn finish(&mut self, result: Result<OAuthFlowOutcome, OAuthError>) {
        if self.finished {
            return;
        }
        if self.launch_tx.borrow().is_none() {
            let launch_result = match &result {
                Err(error) => Err(error.clone()),
                Ok(OAuthFlowOutcome::Terminated { .. }) => Err(OAuthError::AuthorizationCancelled),
                Ok(OAuthFlowOutcome::Authorized { .. }) => {
                    Err(OAuthError::Protocol(super::OAuthProtocolError::Internal))
                }
            };
            self.launch_tx.send_replace(Some(launch_result));
        }
        self.terminal_tx.send_replace(Some(result));
        self.finished = true;
    }
}

impl Drop for OAuthFlowDriver {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Err(OAuthError::Protocol(
                super::OAuthProtocolError::Internal,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::OAuthStatus;

    #[tokio::test]
    async fn cancellation_claim_prevents_late_launch_publication() {
        let (flow, mut driver) = OAuthFlow::new(OAuthBeginRequest {
            redirect_uri: "http://127.0.0.1:9876/callback".to_string(),
            required_scope: None,
        });
        assert!(flow
            .terminal()
            .claim_cancellation(OAuthCancellationReason::Timeout));
        assert!(!driver.publish_launch(
            Ok(OAuthLaunch {
                authorization_url: "https://issuer.example/authorize".to_string(),
                state: "late-state".to_string(),
            }),
            Some("https://issuer.example".to_string()),
        ));
        driver.finish(Ok(OAuthFlowOutcome::Terminated {
            reason: OAuthCancellationReason::Timeout,
            status: OAuthStatus::Unauthorized,
        }));

        assert!(matches!(
            flow.launch().await,
            Err(OAuthError::AuthorizationCancelled)
        ));
        assert_eq!(
            flow.wait_terminal().await.unwrap(),
            OAuthFlowOutcome::Terminated {
                reason: OAuthCancellationReason::Timeout,
                status: OAuthStatus::Unauthorized,
            }
        );
    }
}
