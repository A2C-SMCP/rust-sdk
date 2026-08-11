# Computer OAuth host integration contract

This guide is the normative browser and callback integration contract for clients embedding
`smcp-computer`. It applies to Authorization Code + PKCE flows for protected Streamable HTTP MCP
servers. The same SDK protocol core supports local Desktop/CLI and headless cloud hosts; the host
selects and implements the flow driver.

The SDK never opens a browser, binds a callback port, starts a Web server, connects to a host
Socket, or waits for a callback. `create_oauth_flow` registers a cancellable flow before provider
I/O begins; [`OAuthFlow::launch`] waits for discovery, client setup, PKCE, CSRF state, and the
authorization URL.

## Standards-derived OAuth context

OAuth has no public server-configuration fields. The canonical RFC 8707 resource, authorization
server, scopes, and dynamic client registration are derived from the challenged MCP endpoint and
validated RFC 9728/RFC 8414 or OIDC metadata. The SDK binds admission to the exact
`resource_metadata` URL from the Bearer challenge and verifies that the document describes the
challenged endpoint. It does not support proactive or cross-resource configuration.

The removed `oauth`, `authPolicy`, and `auth_policy` fields are rejected with an actionable
configuration validation error instead of being ignored. A static `Authorization` header selects
static-only authentication; rejection is surfaced as
`HttpAuthenticationError::StaticCredentialsRejected` and never falls back to OAuth.

## Automatic OAuth admission

Streamable HTTP without a static `Authorization` header is always anonymous-first. After automatic
admission, the same connection attempt restores usable persisted Authorization Code credentials
and performs one authenticated initialize. A host must expose an authorization action only when
that bounded startup transaction returns `HttpAuthenticationError::OAuthRequired`; this means
interactive user authorization is still required and all of the following checks succeeded:

1. the server returned a syntactically valid Bearer challenge containing `resource_metadata`;
2. that exact RFC 9728 protected-resource metadata URL was fetched and validated for the
   challenged MCP endpoint;
3. issuer-bearing RFC 8414 or OIDC authorization-server metadata was discovered and validated;
   endpoints derived by rmcp's legacy fallback are not accepted for automatic admission.

Before admission, `oauth_status` and `create_oauth_flow` return `OAuthError::NotConfigured` and do
not perform discovery, registration, or token requests. Basic, Digest, unknown challenges, Bearer
without metadata, failed discovery, bare 401, and ordinary 403 are separate
`HttpAuthenticationError` values. A valid `403 insufficient_scope` can request step-up only when a
validated OAuth coordinator already exists. Automatic startup never polls or infers runtime state
from OAuth status: it performs at most one anonymous initialize and one challenge-triggered OAuth
initialize, while OAuth status and MCP runtime status remain independent.

An accepted `start_client_by_id` is retained as
`MCPServerActivationState::Started` even when that call returns
`HttpAuthenticationError::OAuthRequired`; the orthogonal connection state is then
`MCPServerConnectionState::AuthorizationRequired`. Only an explicit stop clears activation.
Clearing OAuth credentials retires the connected data plane while retaining activation, producing
the same `Started + AuthorizationRequired` lifecycle as a cold start that needs authorization.
Hosts should use `get_server_runtime_statuses` for this distinction. The legacy tuple returned by
`get_server_status` projects `is_active` from activation and `state` from connection state.

rmcp 2.2 currently exposes only the first value when a server sends multiple independent
`WWW-Authenticate` header fields. Consequently, `Basic` in the first field and `Bearer` in a later
field is classified from the first field only. A combined field containing both challenges works.
Fixing separate-field handling requires rmcp to preserve `headers().get_all()`; hosts should track
that upstream limitation when integrating servers that advertise multiple schemes.

## Responsibility matrix

| Capability | `smcp-computer` OAuth core | Host flow driver | Authorization server |
|---|---|---|---|
| Protected-resource and authorization-server discovery | Owns | Does not reimplement | Publishes metadata |
| Dynamic client registration | Owns | Does not configure client identity or secrets | Registers the client |
| PKCE verifier, CSRF state, generation, and pending TTL | Owns | Treats returned state as opaque | Returns the state unchanged |
| Redirect URI | Validates and sends the exact host value | Prepares the callback entry before `create_oauth_flow` | Redirects to the registered URI |
| Authorization URL | Generates and returns [`OAuthLaunch`] | Delivers only to the intended user; never logs or persists it | Presents authentication and consent |
| Browser/UI | No dependency or abstraction | Opens the browser or renders the user entry point | Presents its own pages |
| Callback listener/gateway | No dependency or abstraction | Owns listener, HTTPS route, deadline, response page, and shutdown | Sends success or OAuth error parameters |
| Callback validation | Validates active state, issuer, generation, and pending expiry | Rejects malformed/duplicate parameters and routes only by opaque state | Supplies `code/state/iss` or `error/state/iss` |
| Token exchange and refresh | Owns | Never receives tokens through the callback channel | Issues tokens |
| Credential persistence | Uses the injected [`OAuthCredentialStore`] | Selects Keychain/DB/Vault policy and trusted tenant namespace | Does not participate |
| User-visible result and retry | Returns structured outcome/status/error | Displays a safe result and starts a new flow when required | May allow the user to retry consent |

## Three state domains

These state domains have different trust and lifecycle rules and must not be merged:

1. SDK pending state: PKCE verifier, CSRF state, generation, and TTL. It is process-local and is
   used only for protocol validation.
2. Host callback route: opaque `state -> tenant + cli_session + computer_id + bundle_id`. A cloud
   host stores it with a short TTL and consumes it once. The tenant and routing identities come
   from authenticated host context, never callback query parameters.
3. [`OAuthCredentialStore`]: client registration and token credentials after authorization. Local
   hosts may inject a Keychain-backed store; cloud hosts may inject a tenant/principal-scoped
   DB/Vault store.

Process restart does not restore SDK pending state or host callback routes. It may restore completed
credentials if the host injected persistent credential storage.

## Starting a flow

Prepare the callback entry before calling `create_oauth_flow`. The redirect URI is a runtime value and
must not be copied into persistent MCP configuration.

```rust,no_run
# use smcp_computer::{OAuthBeginRequest, OAuthError};
# use smcp_computer::computer::{Computer, Session};
# use smcp_computer::mcp_clients::BundleId;
# async fn begin<S: Session>(
#     computer: &Computer<S>,
#     bundle_id: &BundleId,
#     redirect_uri: String,
# ) -> Result<(), OAuthError> {
let flow = computer
    .create_oauth_flow(
        bundle_id,
        OAuthBeginRequest {
            redirect_uri,
            required_scope: None,
        },
    )
    .await?;
let launch = flow.launch().await?;

// A cloud host must make callback routing reachable before the user can authorize.
// A local or mobile host has already prepared its listener/link handler.
register_opaque_callback_route(&launch.state)?;
// The host now drives browser delivery and callback receipt. Do not log either field.
deliver_authorization_url_to_target_user(&launch.authorization_url);
# Ok(())
# }
# fn deliver_authorization_url_to_target_user(_: &str) {}
# fn register_opaque_callback_route(_: &str) -> Result<(), OAuthError> { Ok(()) }
```

An identical concurrent `create_oauth_flow` request returns a clone of the active [`OAuthFlow`]. A
conflicting request returns [`OAuthError::AuthorizationAlreadyPending`]. A host must not start a
second listener or route for the reused flow.

The handle exists before discovery or DCR returns. Browser-open failure, user cancellation, host
deadline, server replacement/removal, and Computer shutdown therefore cancel through
[`OAuthFlow::cancel`] without waiting for the provider's HTTP timeout:

```rust,no_run
# use smcp_computer::{OAuthCancellationReason, OAuthError, OAuthFlow};
# async fn cancel(flow: &OAuthFlow) -> Result<(), OAuthError> {
let outcome = flow.cancel(OAuthCancellationReason::Timeout).await?;
# let _ = outcome;
# Ok(())
# }
```

`Computer::begin_oauth`, `complete_oauth`, and `cancel_oauth` remain compatibility facades. They
preserve the previous signatures and post-launch behavior, but only the handle API can express
cancellation before an OAuth state exists. After launch, compatibility `cancel_oauth` calls with
`Cancelled` or `Timeout` also use the immediate host-cancellation path and do not queue behind an
in-flight token exchange.

For a cloud host, route registration must succeed before the authorization URL is delivered. If
registration fails, do not send the URL; cancel the pending SDK flow or retry route registration
within the host deadline.

## Callback input and outcomes

The host parses the callback query before calling the SDK:

| Callback shape | Host action |
|---|---|
| Exactly one `code`, one `state`, optional one `iss`, and no `error` | Call `flow.complete` with [`OAuthCallback`] |
| Exactly one `error=access_denied`, one `state`, optional one `iss`, and no `code` | Call `flow.cancel_callback` with [`OAuthCancellationReason::AccessDenied`] |
| Any other OAuth `error`, one `state`, optional one `iss`, and no `code` | Call `flow.cancel_callback` with [`OAuthCancellationReason::AuthorizationError`] |
| User closes/cancels the host flow before a callback | Call `flow.cancel` with [`OAuthCancellationReason::Cancelled`] |
| The host's total callback deadline expires | Call `flow.cancel` with [`OAuthCancellationReason::Timeout`] |
| Duplicate `code`, `state`, `iss`, or `error`; both `code` and `error`; missing state | Reject as malformed and keep waiting within the same total deadline |

`error_description` is untrusted display text. Do not place it in an SDK input, exception, log,
metric label, or user-visible page. If a product chooses to show provider text, it must apply its
own escaping and disclosure policy outside the SDK.

[`OAuthFlow::complete`], [`OAuthFlow::cancel_callback`], and the compatibility facades return
[`OAuthFlowOutcome`]:

- [`OAuthFlowOutcome::Authorized`] contains the non-secret granted scopes.
- [`OAuthFlowOutcome::Terminated`] contains the normalized termination reason and the resulting
  [`OAuthStatus`]. A cancelled scope upgrade can therefore report that earlier credentials remain
  authorized.

After a call accepts the active state and issuer, the SDK owns the terminal transition.
Dropping or aborting the caller future does not cancel that transition; a subsequent `oauth_status`
waits for the exchange or cancellation cleanup and returns the converged status. Complete/cancel
races have one terminal winner. A candidate token and client registration remain isolated until
completion wins the serialized commit, so cancellation or failed scope step-up preserves the
previous credential and scopes.

Protocol failures remain typed errors:

- [`OAuthError::StateMismatch`] means the callback is not for the active flow. It does not consume
  a different valid pending flow.
- [`OAuthError::IssuerMismatch`] means `iss` is invalid, is missing when the authorization server
  requires it, or does not match discovery. It does not consume the valid pending flow.
- [`OAuthError::AuthorizationExpired`] means the pending generation or PKCE/CSRF state is no longer
  valid. The SDK removes the stale pending flow; the host must start a new one.

After any returned outcome or terminal error, use the returned status or `oauth_status` to render a
safe result. Do not infer authorization from the browser redirect alone.

## Status events and resynchronization

Hosts subscribe once through [`crate::computer::Computer::subscribe_events`]. OAuth transitions arrive as
[`crate::status::ComputerEvent::OAuthStatusChanged`] with the stable `bundle_id` and complete, non-secret
[`OAuthStatus`]. The event covers interactive begin/complete/cancel, credential clear, background
refresh failure, protected-resource 401, and `403 insufficient_scope`. Equal consecutive statuses
for the same bundle are suppressed.

The channel is bounded. A slow receiver can get `RecvError::Lagged`, which reports only how many
events were skipped, not their bundle IDs. Before continuing, the host must call
[`crate::computer::Computer::oauth_status`] for every OAuth bundle whose UI state it maintains.
`shutdown` emits its final lifecycle event and then gates all OAuth emissions, so hosts must not use
a polling loop as a substitute for the event stream.

## Local loopback flow driver

Use loopback HTTP only for a listener bound to a loopback address. Bind first so the exact,
ephemeral port can be supplied to `create_oauth_flow`.

```text
bind 127.0.0.1:<ephemeral>
  -> create_oauth_flow(redirect_uri=http://127.0.0.1:<ephemeral>/callback)
  -> flow.launch()
  -> open the returned authorization URL
  -> authorization server redirects to loopback
  -> host validates method, path, cardinality, and callback shape
  -> flow.complete or flow.cancel_callback
  -> render a non-sensitive result page
  -> close listener
```

Use one total deadline for the flow. Requests for `/favicon.ico`, the wrong path or method, missing
parameters, duplicate parameters, and a wrong state are invalid probes; respond safely and keep
the listener alive without extending the deadline. Accept only the first well-formed callback for
the active state. At the deadline, call `flow.cancel(Timeout)` and close the listener.

Do not log request URIs: they can contain authorization code, state, issuer, and provider error
text. Pass the authorization URL to the OS browser command as one argument, never through a shell.

## Mobile flow driver

Prefer an app-claimed HTTPS universal/app link when the platform and authorization server support
it. A native host may instead use an RFC 8252-style reverse-domain private-use URI such as
`com.example.app:/oauth/callback`. The SDK accepts that form only when the scheme has at least two
reverse-domain labels, the URI has no authority/host, and it uses a non-root single-slash path.
Generic schemes such as `custom:/callback`, `file:` URIs, and authority-bearing
`com.example.app://host/callback` values are rejected.

Register the link handler with the operating system before `create_oauth_flow`, deliver the returned URL
to the intended user, and pass the received `code/state/iss` or normalized OAuth error through the
same callback contract. A replacement app process cannot complete a flow whose process-local PKCE
state was lost; it must begin a fresh flow.

## Headless cloud flow driver

When the CLI and user browser are on different machines, user-side localhost is not a valid
callback target. Use a stable HTTPS Callback Gateway URI registered for the OAuth client.

```text
prepare stable HTTPS callback route
  -> CLI calls create_oauth_flow(redirect_uri=https://gateway.example/oauth/callback)
  -> flow.launch()
  -> register one-time opaque state route for authenticated tenant/CLI/Computer/bundle
  -> send authorization URL through a private event to the target user only
  -> authorization server calls the HTTPS Gateway
  -> Gateway consumes the route by state only
  -> send code/state/iss or normalized OAuth error to the original CLI only
  -> original live CLI calls flow.complete or flow.cancel_callback
  -> send sanitized OAuthStatus to the target user
```

The route value is trusted server state. Ignore `tenant`, `cli_session`, `computer_id`, `bundle_id`,
room, or user identifiers supplied by the callback request. The authorization code must never
enter a UI broadcast, shared room, durable event stream, analytics event, or callback access log.

Route lookup and consumption should be atomic. Unknown, replayed, and expired state must not be
delivered to another coordinator. When a route expires while the originating coordinator is still
alive, the host calls `flow.cancel(Timeout)` so the SDK pending state terminates immediately.

If the initiating CLI exits, remove its route. Its process-local PKCE verifier is gone, so an old
callback cannot be completed by a replacement CLI or another coordinator. The replacement starts a
new flow and receives a new state.

This contract is event-driven: the browser redirect, Gateway request, and private host message
advance the flow. Hosts must not add a status polling loop to compensate for missing callback
routing.

## Redaction rules

Never log, persist as ordinary configuration, or expose through ordinary `Debug`:

- the complete authorization URL;
- authorization code;
- CSRF state or PKCE verifier;
- access or refresh token;
- client secret or private key;
- raw callback URI or `error_description`.

[`OAuthLaunch`], [`OAuthCallback`], and [`OAuthCancellation`] redact sensitive fields in their
`Debug` implementations. This is defense in depth, not permission to attach those values to custom
tracing fields. Host route logs should use a generated non-secret correlation ID rather than state
or business identifiers.

Underlying rmcp/provider failures cross the public API only as the stable
[`OAuthProtocolError`] category carried by [`OAuthError::Protocol`]. Its `Display` and `Debug`
representations and error chain never contain the provider response body or upstream error string.

## Host completion checklist

- Callback entry exists before `create_oauth_flow`.
- Local mode uses loopback HTTP; remote mode uses stable HTTPS.
- Authorization URL reaches only the intended user.
- Callback routes only by opaque state and is consumed once.
- Code reaches only the original live coordinator.
- Invalid probes do not extend the total deadline.
- Denial calls `flow.cancel_callback`; host cancellation and timeout call `flow.cancel`.
- Route expiry and process exit terminate the old flow; retry starts a new flow.
- Browser, listener, Gateway, Socket, result page, and retries remain outside the SDK.
- Logs and `Debug` output follow the redaction rules above.
