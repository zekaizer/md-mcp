//! Streamable HTTP transport (ADR-0013): serve the MCP tool surface over axum.
//!
//! rmcp's [`StreamableHttpService`] is a `tower` service; we mount it at the
//! single path `/mcp` with `route_service` (exact path, so the `Host` header that
//! drives DNS-rebinding validation is never dropped) and optionally guard it with
//! a bearer-token middleware. Every session gets a cheap `MdServer::clone()`, so
//! all sessions share the one vault and the one commit lock (ADR-0008).
//!
//! Two browser-facing guards are wired: the `Host` allowlist (rmcp default:
//! loopback) and the `Origin` allowlist (default here: the loopback origins for
//! the bound port). The Origin guard is what actually stops a malicious web page
//! from driving the tools at `http://127.0.0.1:<port>/mcp` — the Host header alone
//! does not, since loopback is in its own allowlist. Non-browser MCP clients send
//! no `Origin` header and pass the Origin guard unaffected.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use tokio::net::TcpListener;

use crate::MdServer;
use crate::config::HttpConfig;
use crate::oauth::{self, OAuthState};

/// How long an idle MCP session is kept before rmcp reaps it. rmcp's own default
/// is 5 minutes, which a conversational client outlives all the time: the session
/// worker quits on `IdleTimeout`, the session manager drops the entry, and the
/// client's next tool call — still carrying the old `Mcp-Session-Id` — gets the
/// spec-mandated 404, surfacing to the caller as a failed tool rather than as an
/// expired session. An hour covers a normal think-and-type pause; the timeout
/// stays a safety net for sessions whose HTTP connection dropped silently.
const SESSION_KEEP_ALIVE: Duration = Duration::from_secs(3600);

/// The session manager backing `/mcp`, with the idle timeout raised to
/// [`SESSION_KEEP_ALIVE`]. Sessions hold no vault state of their own — each is a
/// cheap `MdServer::clone()` over the shared vault — so a longer-lived one costs
/// only its worker task.
fn session_manager() -> LocalSessionManager {
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(SESSION_KEEP_ALIVE);
    manager
}

/// Build the axum router serving the MCP endpoint at `/mcp`, wiring the Host and
/// Origin guards and (when a token is configured) bearer auth.
pub fn router(server: MdServer, cfg: &HttpConfig) -> Router {
    let mut sh_config = StreamableHttpServerConfig::default();

    // Host guard: None = rmcp's loopback default; Some(empty) (`*`) = disable;
    // Some(list) = restrict.
    match &cfg.allowed_hosts {
        None => {}
        Some(hosts) if hosts.is_empty() => sh_config = sh_config.disable_allowed_hosts(),
        Some(hosts) => sh_config = sh_config.with_allowed_hosts(hosts.clone()),
    }

    // Origin guard: None = the loopback origins for this port (secure default;
    // rmcp's own default is *off*); Some(empty) (`*`) = leave it off; Some(list) =
    // restrict.
    match &cfg.allowed_origins {
        None => sh_config = sh_config.with_allowed_origins(loopback_origins(cfg.addr.port())),
        Some(origins) if origins.is_empty() => {}
        Some(origins) => sh_config = sh_config.with_allowed_origins(origins.clone()),
    }

    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(session_manager()),
        sh_config,
    );

    let mcp = Router::new().route_service("/mcp", service);

    // OAuth is enabled exactly when a static token is set (ADR-0014): the token is the
    // `/authorize` ownership gate and the Claude Code (CLI) bearer. Without it, the
    // loopback-dev default stays unguarded as in ADR-0013.
    // The access log is the outermost /mcp layer (added last), so it also
    // captures auth rejections: one structured line per request with the
    // JSON-RPC method / tool name (small bodies only) and the duration
    // (ADR-0021). It never touches the oauth discovery routes.
    match &cfg.token {
        None => mcp.layer(middleware::from_fn(access_log)),
        Some(token) => {
            let oauth = OAuthState::load(token.clone(), cfg.state_dir.join("oauth-state.json"));
            let guarded = mcp
                .layer(middleware::from_fn_with_state(
                    oauth.clone(),
                    require_bearer,
                ))
                .layer(middleware::from_fn(access_log));
            // OAuth discovery + flow routes stay unauthenticated (they are the auth flow);
            // only `/mcp` is guarded. A single outer Host guard validates the `Host` before
            // any handler derives a public base URL / the 401 challenge from it (ADR-0014).
            let allowed = Arc::new(effective_allowed_hosts(&cfg.allowed_hosts));
            oauth::routes(oauth)
                .merge(guarded)
                .layer(middleware::from_fn_with_state(allowed, host_guard))
        }
    }
}

/// The effective `Host` allowlist for the edge guard: rmcp's loopback set when unset,
/// empty (= allow all, the `*` escape hatch) when disabled, else the configured list.
fn effective_allowed_hosts(configured: &Option<Vec<String>>) -> Vec<String> {
    match configured {
        None => vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
        Some(list) => list.clone(),
    }
}

/// Lowercased host with any IPv6 brackets stripped, for allowlist comparison.
fn normalize_host(h: &str) -> String {
    h.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// Reject any request whose `Host` is not allowlisted, so the OAuth metadata and the 401
/// challenge only ever reflect a validated host. An empty allowlist (`*`) disables it.
async fn host_guard(
    State(allowed): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    if allowed.is_empty() {
        return next.run(request).await;
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|hh| hh.parse::<axum::http::uri::Authority>().ok())
        .map(|a| normalize_host(a.host()));
    match host {
        Some(h) if allowed.iter().any(|a| normalize_host(a) == h) => next.run(request).await,
        _ => (StatusCode::BAD_REQUEST, "Bad Request: Host not allowed").into_response(),
    }
}

/// The loopback origins a same-machine browser would send for `port`. A
/// cross-site page's real `Origin` (e.g. `https://evil.com`) is none of these and
/// is rejected; a request with no `Origin` (every non-browser client) passes.
fn loopback_origins(port: u16) -> Vec<String> {
    vec![
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
        format!("http://[::1]:{port}"),
    ]
}

/// Bind `cfg.addr` and serve until a shutdown signal (Ctrl-C / SIGTERM).
pub async fn serve(server: MdServer, cfg: &HttpConfig) -> Result<()> {
    let listener = TcpListener::bind(cfg.addr)
        .await
        .with_context(|| format!("failed to bind {}", cfg.addr))?;
    let local = listener.local_addr().unwrap_or(cfg.addr);

    // A non-loopback bind is an exposed surface: flag missing auth, and remind
    // that there is no in-process TLS.
    if !local.ip().is_loopback() {
        if cfg.token.is_none() {
            tracing::warn!(
                addr = %local,
                "binding a non-loopback address with no MD_HTTP_TOKEN — the tool surface is exposed without authentication"
            );
        }
        tracing::warn!(
            "non-loopback bind: there is no in-process TLS; terminate TLS upstream (e.g. a reverse proxy / tunnel), or the bearer token travels in clear"
        );
    }

    tracing::info!(
        addr = %local,
        auth = cfg.token.is_some(),
        "md-mcp serving Streamable HTTP at /mcp"
    );
    axum::serve(listener, router(server, cfg))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server error")?;
    Ok(())
}

/// Resolve when the process receives Ctrl-C (SIGINT) or, on Unix, SIGTERM (the
/// signal `kill`/systemd/containers send). If a handler cannot be installed, do
/// **not** resolve — that would shut the server down at startup; stay up instead.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

/// Largest request body the access log will buffer to sniff the JSON-RPC
/// method / tool name; larger bodies stream through unsniffed (their log line
/// just lacks `rpc`/`tool`). Covers every routine call; only huge write
/// batches exceed it.
const ACCESS_SNIFF_MAX: usize = 256 * 1024;

/// One structured log line per `/mcp` request: HTTP method, JSON-RPC method,
/// tool name (for `tools/call`), response status, and duration (ADR-0021).
/// Outermost layer, so 401s are captured too.
///
/// rmcp streams tool results over SSE — response *headers* leave before the
/// tool body runs — so the line is emitted when the response body ends (or the
/// client disconnects), and `duration_ms` is the real request-to-completion
/// wall time, not time-to-first-byte.
async fn access_log(request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let http_method = request.method().clone();

    let (parts, body) = request.into_parts();
    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    let (request, call) = match content_length {
        Some(n) if n <= ACCESS_SNIFF_MAX => match axum::body::to_bytes(body, n).await {
            Ok(bytes) => {
                let call = parse_rpc_call(&bytes);
                (
                    Request::from_parts(parts, axum::body::Body::from(bytes)),
                    call,
                )
            }
            Err(_) => {
                tracing::warn!("access: request body shorter than its Content-Length");
                return (StatusCode::BAD_REQUEST, "bad request body").into_response();
            }
        },
        _ => (Request::from_parts(parts, body), None),
    };

    let response = next.run(request).await;
    let (rpc, tool) = call.map_or((None, None), |(rpc, tool)| (Some(rpc), tool));
    let meta = AccessMeta {
        method: http_method,
        rpc,
        tool,
        status: response.status().as_u16(),
        started,
    };
    let (parts, body) = response.into_parts();
    // An already-empty body (401s, 202 notifications) may be dropped by hyper
    // without ever being polled to EOS; log it as complete now rather than
    // letting Drop mislabel it aborted.
    if http_body::Body::is_end_stream(&body) {
        meta.emit("complete");
        return Response::from_parts(parts, body);
    }
    Response::from_parts(
        parts,
        axum::body::Body::new(AccessLoggedBody {
            inner: body,
            meta: Some(meta),
        }),
    )
}

/// The access-log fields captured up front, emitted once at end-of-stream.
struct AccessMeta {
    method: axum::http::Method,
    rpc: Option<String>,
    tool: Option<String>,
    status: u16,
    started: std::time::Instant,
}

impl AccessMeta {
    fn emit(&self, outcome: &'static str) {
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            target: "md_server::access",
            method = %self.method,
            rpc = self.rpc.as_deref(),
            tool = self.tool.as_deref(),
            status = self.status,
            duration_ms,
            outcome,
            "mcp"
        );
    }
}

/// A response body that emits the access-log line exactly once: at
/// end-of-stream (`complete`), or on drop before that (`aborted` — the client
/// disconnected or the connection died).
struct AccessLoggedBody {
    inner: axum::body::Body,
    meta: Option<AccessMeta>,
}

impl http_body::Body for AccessLoggedBody {
    type Data = <axum::body::Body as http_body::Body>::Data;
    type Error = <axum::body::Body as http_body::Body>::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        if let std::task::Poll::Ready(None) = &poll
            && let Some(meta) = self.meta.take()
        {
            meta.emit("complete");
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for AccessLoggedBody {
    fn drop(&mut self) {
        if let Some(meta) = self.meta.take() {
            meta.emit("aborted");
        }
    }
}

/// The `(json-rpc method, tool name)` of a request body, when it parses as a
/// JSON-RPC call; the tool name only for `tools/call`.
fn parse_rpc_call(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let method = v.get("method")?.as_str()?.to_string();
    let tool = if method == "tools/call" {
        v.pointer("/params/name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
    } else {
        None
    };
    Some((method, tool))
}

/// Reject any request to `/mcp` whose bearer is neither a live issued OAuth access token
/// nor the static `MD_HTTP_TOKEN`. On failure, advertise the protected-resource metadata
/// so the claude.ai connector can start the OAuth flow (ADR-0014).
async fn require_bearer(
    State(oauth): State<Arc<OAuthState>>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer)
        .map(str::to_owned);
    match token {
        Some(token) if oauth.validate_bearer(&token).is_some() => next.run(request).await,
        _ => unauthorized(&request),
    }
}

/// `401` carrying the RFC 9728 `WWW-Authenticate` challenge pointing at this server's
/// protected-resource metadata (host from the tunnel-forwarded `Host`).
fn unauthorized(request: &Request) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let challenge =
        format!("Bearer resource_metadata=\"https://{host}/.well-known/oauth-protected-resource\"");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
    )
        .into_response()
}

/// Extract the credential from an `Authorization` value. Per RFC 7235/6750 the
/// scheme is case-insensitive and the separator is `1*SP`, so `Bearer`, `bearer`,
/// and extra spaces all parse.
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, credential) = value.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim_start())
}

/// Compare two tokens in constant time, without leaking either length: BLAKE3
/// digests are fixed-width (32 bytes) and `blake3::Hash`'s `==` is constant-time.
/// Shared with [`crate::oauth`] for the static-token gate and PKCE compare.
pub(crate) fn token_matches(provided: &str, expected: &str) -> bool {
    blake3::hash(provided.as_bytes()) == blake3::hash(expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{
        SESSION_KEEP_ALIVE, effective_allowed_hosts, loopback_origins, normalize_host,
        parse_bearer, token_matches,
    };

    #[test]
    fn host_allowlist_default_and_normalization() {
        assert_eq!(
            effective_allowed_hosts(&None),
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string()
            ]
        );
        // `*` (Some(empty)) disables the guard (allow all).
        assert!(effective_allowed_hosts(&Some(vec![])).is_empty());
        // A configured list is used verbatim.
        assert_eq!(
            effective_allowed_hosts(&Some(vec!["notes.example.com".into()])),
            vec!["notes.example.com".to_string()]
        );
        // Bracket-stripping + case folding so `[::1]` matches `::1`.
        assert_eq!(normalize_host("[::1]"), "::1");
        assert_eq!(normalize_host("Example.COM"), "example.com");
    }

    #[test]
    fn parse_rpc_call_extracts_method_and_tool() {
        use super::parse_rpc_call;
        let call = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_notes","arguments":{}}}"#;
        assert_eq!(
            parse_rpc_call(call),
            Some(("tools/call".into(), Some("create_notes".into())))
        );
        let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        assert_eq!(parse_rpc_call(init), Some(("initialize".into(), None)));
        assert_eq!(parse_rpc_call(b"not json"), None);
        assert_eq!(parse_rpc_call(br#"{"no":"method"}"#), None);
    }

    #[test]
    fn parse_bearer_accepts_rfc_variants() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER abc"), Some("abc"));
        assert_eq!(parse_bearer("Bearer   abc"), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer"), None);
    }

    #[test]
    fn token_matches_is_exact() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3creT"));
        assert!(!token_matches("s3cret", "s3cre")); // differing length
        assert!(token_matches("", ""));
    }

    #[test]
    fn loopback_origins_cover_the_bound_port() {
        let o = loopback_origins(7654);
        assert!(o.contains(&"http://127.0.0.1:7654".to_string()));
        assert!(o.contains(&"http://localhost:7654".to_string()));
        assert!(o.contains(&"http://[::1]:7654".to_string()));
    }

    #[test]
    fn idle_session_outlives_the_rmcp_default() {
        use rmcp::transport::streamable_http_server::session::local::SessionConfig;

        // The session manager must raise rmcp's idle timeout, not inherit it:
        // at the 5-minute default a client that pauses mid-conversation has its
        // session reaped, and its next tool call 404s.
        let manager = super::session_manager();
        assert_eq!(manager.session_config.keep_alive, Some(SESSION_KEEP_ALIVE));
        assert!(SESSION_KEEP_ALIVE > SessionConfig::DEFAULT_KEEP_ALIVE);
    }
}
