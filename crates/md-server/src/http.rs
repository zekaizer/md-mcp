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
        Arc::new(LocalSessionManager::default()),
        sh_config,
    );

    let mcp = Router::new().route_service("/mcp", service);

    // OAuth is enabled exactly when a static token is set (ADR-0014): the token is the
    // `/authorize` ownership gate and the Claude Code (CLI) bearer. Without it, the
    // loopback-dev default stays unguarded as in ADR-0013.
    match &cfg.token {
        None => mcp,
        Some(token) => {
            let oauth = OAuthState::load(token.clone(), cfg.state_dir.join("oauth-state.json"));
            let guarded = mcp.layer(middleware::from_fn_with_state(
                oauth.clone(),
                require_bearer,
            ));
            // OAuth discovery + flow routes stay unauthenticated (they are the auth flow);
            // only `/mcp` is guarded.
            oauth::routes(oauth).merge(guarded)
        }
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
        Some(token) if oauth.validate_bearer(&token) => next.run(request).await,
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
    use super::{loopback_origins, parse_bearer, token_matches};

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
}
