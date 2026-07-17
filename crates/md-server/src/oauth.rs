//! A minimal, self-hosted OAuth 2.1 authorization server co-located with the MCP
//! resource server, so the claude.ai web/mobile connector (which speaks only OAuth)
//! can authenticate (ADR-0014). Ported from the sibling project's proven design.
//!
//! Scope is deliberately small and single-user: Dynamic Client Registration, an
//! authorization-code flow with PKCE (S256) gated by the static `MD_HTTP_TOKEN`, and
//! rotating opaque access/refresh tokens. Registered clients and issued tokens are
//! persisted to `oauth-state.json` so authorization survives restarts; short-lived auth
//! codes are kept in memory only.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::http::token_matches;

/// Access-token lifetime: short; the connector refreshes silently.
const ACCESS_TTL_SECS: u64 = 60 * 60;
/// Refresh-token lifetime: long, so infrequent use never forces re-auth.
const REFRESH_TTL_SECS: u64 = 90 * 24 * 60 * 60;
/// Authorization codes are single-use and short-lived.
const CODE_TTL_SECS: u64 = 5 * 60;

/// State schema version. Bump on a breaking change; an unreadable/old file starts empty,
/// which forces a one-time re-auth.
const STATE_VERSION: u32 = 1;

/// Bound the persisted store: `/register` is unauthenticated (it must be, per the
/// discovery flow), so cap stored clients (evicting the oldest) and redirect URIs per
/// registration to keep an internet-reachable endpoint from growing the file without end.
const MAX_CLIENTS: usize = 50;
const MAX_REDIRECT_URIS: usize = 8;

/// On-disk, persisted across restarts. Bearer material at rest — see [`create_private`]
/// for how the file is kept owner-only.
#[derive(Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    v: u32,
    /// `client_id` -> registered client.
    clients: HashMap<String, ClientRec>,
    /// access token -> issued-token record.
    access: HashMap<String, TokenRec>,
    /// refresh token -> issued-token record.
    refresh: HashMap<String, TokenRec>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            clients: HashMap::new(),
            access: HashMap::new(),
            refresh: HashMap::new(),
        }
    }
}

/// An issued token: its expiry and the client it was issued to.
#[derive(Clone, Serialize, Deserialize)]
struct TokenRec {
    /// Unix-seconds expiry.
    expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ClientRec {
    redirect_uris: Vec<String>,
    /// Unix-seconds registration time, for eviction order. `0` for legacy records.
    #[serde(default)]
    created_at: u64,
}

/// In-memory only (one-time, short-lived).
struct CodeRec {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    expires_at: u64,
}

/// The authorization-server state: the gate secret, the persisted store, and ephemeral
/// codes.
pub struct OAuthState {
    /// Reused as the Claude Code/CLI bearer and as the `/authorize` ownership gate.
    static_token: String,
    state_file: PathBuf,
    store: Mutex<Persisted>,
    codes: Mutex<HashMap<String, CodeRec>>,
}

impl OAuthState {
    /// Load persisted state (pruning expired tokens); a corrupt/absent file starts empty.
    pub fn load(static_token: String, state_file: PathBuf) -> Arc<Self> {
        let mut store = match std::fs::read(&state_file) {
            Ok(bytes) => serde_json::from_slice::<Persisted>(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "oauth state file unreadable; starting empty");
                Persisted::default()
            }),
            Err(_) => Persisted::default(),
        };
        let now = now_secs();
        store.access.retain(|_, rec| rec.expires_at > now);
        store.refresh.retain(|_, rec| rec.expires_at > now);

        Arc::new(Self {
            static_token,
            state_file,
            store: Mutex::new(store),
            codes: Mutex::new(HashMap::new()),
        })
    }

    /// The static token (CLI bearer + authorize gate).
    fn static_token(&self) -> &str {
        &self.static_token
    }

    /// True if `token` is the static token or a live issued access token.
    pub fn validate_bearer(&self, token: &str) -> bool {
        if token_matches(token, &self.static_token) {
            return true;
        }
        let store = self.store.lock().unwrap();
        store
            .access
            .get(token)
            .is_some_and(|rec| rec.expires_at > now_secs())
    }

    /// Atomically persist the store (owner-only, temp + rename, creating the state dir).
    /// Best-effort; logs on failure.
    fn persist(&self) {
        let bytes = {
            let store = self.store.lock().unwrap();
            match serde_json::to_vec_pretty(&*store) {
                Ok(b) => b,
                Err(error) => {
                    tracing::error!(%error, "failed to serialize oauth state");
                    return;
                }
            }
        };
        if let Some(parent) = self.state_file.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::error!(%error, "failed to create oauth state dir");
            return;
        }
        // Unique temp path per write: concurrent persists must not share one temp file
        // (a shared path lets interleaved truncate/write/rename corrupt the store).
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .state_file
            .with_extension(format!("tmp.{}.{seq}", std::process::id()));
        let write = create_private(&tmp)
            .and_then(|mut f| f.write_all(&bytes))
            .and_then(|()| std::fs::rename(&tmp, &self.state_file));
        if let Err(error) = write {
            tracing::error!(%error, "failed to persist oauth state");
        }
    }

    fn register_client(&self, redirect_uris: Vec<String>) -> String {
        let client_id = random_token();
        {
            let mut store = self.store.lock().unwrap();
            if store.clients.len() >= MAX_CLIENTS
                && let Some(oldest) = store
                    .clients
                    .iter()
                    .min_by_key(|(_, c)| c.created_at)
                    .map(|(id, _)| id.clone())
            {
                store.clients.remove(&oldest);
            }
            store.clients.insert(
                client_id.clone(),
                ClientRec {
                    redirect_uris,
                    created_at: now_secs(),
                },
            );
        }
        self.persist();
        client_id
    }

    fn client_allows_redirect(&self, client_id: &str, redirect_uri: &str) -> bool {
        let store = self.store.lock().unwrap();
        store.clients.get(client_id).is_some_and(|c| {
            c.redirect_uris
                .iter()
                .any(|r| redirect_uri_matches(r, redirect_uri))
        })
    }

    fn issue_code(
        &self,
        client_id: String,
        redirect_uri: String,
        code_challenge: String,
    ) -> String {
        let code = random_token();
        self.codes.lock().unwrap().insert(
            code.clone(),
            CodeRec {
                client_id,
                redirect_uri,
                code_challenge,
                expires_at: now_secs() + CODE_TTL_SECS,
            },
        );
        code
    }

    /// Redeem an auth code for tokens, verifying PKCE and the redirect/client binding.
    fn redeem_code(
        &self,
        code: &str,
        verifier: &str,
        client_id: &str,
        redirect_uri: &str,
    ) -> Option<TokenResponse> {
        let rec = self.codes.lock().unwrap().remove(code)?;
        if rec.expires_at <= now_secs()
            || rec.client_id != client_id
            || rec.redirect_uri != redirect_uri
            || !pkce_matches(verifier, &rec.code_challenge)
        {
            return None;
        }
        Some(self.issue_tokens(Some(rec.client_id)))
    }

    /// Rotate a refresh token for a fresh access/refresh pair, carrying `client_id` forward.
    fn refresh(&self, refresh_token: &str) -> Option<TokenResponse> {
        let client_id = {
            let mut store = self.store.lock().unwrap();
            let rec = store.refresh.remove(refresh_token)?;
            if rec.expires_at <= now_secs() {
                self.persist();
                return None;
            }
            rec.client_id
        };
        Some(self.issue_tokens(client_id))
    }

    fn issue_tokens(&self, client_id: Option<String>) -> TokenResponse {
        let access = random_token();
        let refresh = random_token();
        let now = now_secs();
        {
            let mut store = self.store.lock().unwrap();
            store.access.insert(
                access.clone(),
                TokenRec {
                    expires_at: now + ACCESS_TTL_SECS,
                    client_id: client_id.clone(),
                },
            );
            store.refresh.insert(
                refresh.clone(),
                TokenRec {
                    expires_at: now + REFRESH_TTL_SECS,
                    client_id,
                },
            );
        }
        self.persist();
        TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: ACCESS_TTL_SECS,
            refresh_token: refresh,
        }
    }
}

/// The unauthenticated OAuth routes (discovery + the flow). `/mcp` is guarded separately.
pub fn routes(state: Arc<OAuthState>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize_get).post(authorize_post))
        .route("/token", post(token))
        .with_state(state)
}

// --- discovery ---------------------------------------------------------------

#[derive(Serialize)]
struct ProtectedResourceMeta {
    resource: String,
    authorization_servers: Vec<String>,
    bearer_methods_supported: Vec<&'static str>,
}

async fn protected_resource(headers: HeaderMap) -> Json<ProtectedResourceMeta> {
    let base = base_url(&headers);
    Json(ProtectedResourceMeta {
        resource: format!("{base}/mcp"),
        authorization_servers: vec![base],
        bearer_methods_supported: vec!["header"],
    })
}

#[derive(Serialize)]
struct AuthServerMeta {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    response_types_supported: Vec<&'static str>,
    grant_types_supported: Vec<&'static str>,
    code_challenge_methods_supported: Vec<&'static str>,
    token_endpoint_auth_methods_supported: Vec<&'static str>,
}

async fn authorization_server(headers: HeaderMap) -> Json<AuthServerMeta> {
    let base = base_url(&headers);
    Json(AuthServerMeta {
        issuer: base.clone(),
        authorization_endpoint: format!("{base}/authorize"),
        token_endpoint: format!("{base}/token"),
        registration_endpoint: format!("{base}/register"),
        response_types_supported: vec!["code"],
        grant_types_supported: vec!["authorization_code", "refresh_token"],
        code_challenge_methods_supported: vec!["S256"],
        token_endpoint_auth_methods_supported: vec!["none"],
    })
}

// --- dynamic client registration ---------------------------------------------

#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
}

#[derive(Serialize)]
struct RegisterResponse {
    client_id: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: Vec<&'static str>,
    response_types: Vec<&'static str>,
}

async fn register(
    State(oauth): State<Arc<OAuthState>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    if req.redirect_uris.is_empty() || req.redirect_uris.len() > MAX_REDIRECT_URIS {
        tracing::warn!(
            count = req.redirect_uris.len(),
            "oauth: client registration rejected (redirect_uris empty or over cap)"
        );
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri");
    }
    if !req.redirect_uris.iter().all(|u| is_absolute_http(u)) {
        tracing::warn!("oauth: client registration rejected (non-http(s) redirect_uri)");
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri");
    }
    let client_id = oauth.register_client(req.redirect_uris.clone());
    tracing::info!(client_id = %client_id, redirect_uris = ?req.redirect_uris, "oauth: client registered");
    (
        StatusCode::CREATED,
        Json(RegisterResponse {
            client_id,
            redirect_uris: req.redirect_uris,
            token_endpoint_auth_method: "none",
            grant_types: vec!["authorization_code", "refresh_token"],
            response_types: vec!["code"],
        }),
    )
        .into_response()
}

// --- authorization -----------------------------------------------------------

#[derive(Deserialize)]
struct AuthorizeQuery {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    response_type: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

async fn authorize_get(
    State(oauth): State<Arc<OAuthState>>,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    if let Err(message) = validate_authorize(&oauth, &q) {
        tracing::warn!(client_id = %q.client_id, reason = message, "oauth: authorize rejected");
        return oauth_error(StatusCode::BAD_REQUEST, message);
    }
    Html(authorize_form(&q, None)).into_response()
}

#[derive(Deserialize)]
struct AuthorizeForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    token: String,
}

async fn authorize_post(
    State(oauth): State<Arc<OAuthState>>,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let q = AuthorizeQuery {
        client_id: form.client_id,
        redirect_uri: form.redirect_uri,
        code_challenge: form.code_challenge,
        code_challenge_method: form.code_challenge_method,
        response_type: None,
        state: form.state,
    };
    if let Err(message) = validate_authorize(&oauth, &q) {
        tracing::warn!(client_id = %q.client_id, reason = message, "oauth: authorize rejected");
        return oauth_error(StatusCode::BAD_REQUEST, message);
    }
    if !token_matches(&form.token, oauth.static_token()) {
        // Wrong token: redisplay the gate with an error, never redirect.
        tracing::warn!(client_id = %q.client_id, "oauth: authorization denied (invalid gate token)");
        return (
            StatusCode::UNAUTHORIZED,
            Html(authorize_form(&q, Some("Invalid token."))),
        )
            .into_response();
    }

    tracing::info!(client_id = %q.client_id, redirect_uri = %q.redirect_uri, "oauth: authorization granted");
    let code = oauth.issue_code(
        q.client_id.clone(),
        q.redirect_uri.clone(),
        q.code_challenge.clone(),
    );
    let mut location = format!(
        "{}{}code={}",
        q.redirect_uri,
        if q.redirect_uri.contains('?') {
            '&'
        } else {
            '?'
        },
        code,
    );
    if let Some(state) = &q.state {
        location.push_str("&state=");
        location.push_str(&percent_encode(state));
    }
    Redirect::to(&location).into_response()
}

/// Validate the static, non-secret parts of an authorize request.
fn validate_authorize(oauth: &OAuthState, q: &AuthorizeQuery) -> Result<(), &'static str> {
    if q.code_challenge.is_empty() {
        return Err("missing code_challenge");
    }
    if q.code_challenge_method.as_deref().unwrap_or("S256") != "S256" {
        return Err("unsupported code_challenge_method (S256 only)");
    }
    if q.response_type.as_deref().unwrap_or("code") != "code" {
        return Err("unsupported response_type (code only)");
    }
    if !oauth.client_allows_redirect(&q.client_id, &q.redirect_uri) {
        return Err("unknown client_id or unregistered redirect_uri");
    }
    Ok(())
}

// --- token -------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
}

async fn token(State(oauth): State<Arc<OAuthState>>, Form(req): Form<TokenRequest>) -> Response {
    let issued = match req.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier), Some(client_id), Some(redirect_uri)) = (
                req.code.as_deref(),
                req.code_verifier.as_deref(),
                req.client_id.as_deref(),
                req.redirect_uri.as_deref(),
            ) else {
                tracing::warn!("oauth: token rejected (missing authorization_code params)");
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            oauth.redeem_code(code, verifier, client_id, redirect_uri)
        }
        "refresh_token" => match req.refresh_token.as_deref() {
            Some(rt) => oauth.refresh(rt),
            None => {
                tracing::warn!("oauth: token rejected (missing refresh_token)");
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            }
        },
        other => {
            tracing::warn!(grant = %other, "oauth: token rejected (unsupported grant_type)");
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
        }
    };

    match issued {
        Some(tokens) => {
            tracing::info!(grant = %req.grant_type, "oauth: tokens issued");
            Json(tokens).into_response()
        }
        None => {
            tracing::warn!(grant = %req.grant_type, "oauth: token request rejected (invalid_grant)");
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant")
        }
    }
}

// --- helpers -----------------------------------------------------------------

/// Create `path` for writing, kept as private as the platform allows — it holds bearer
/// material at rest.
///
/// Unix creates it `0600`. Windows has no mode bits, so the file inherits the state
/// directory's ACL: keep `MD_STATE_DIR` under a per-user location (the default
/// `%LOCALAPPDATA%`-style profile paths already deny other users) rather than a shared
/// root.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn oauth_error(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// Public base URL derived from the (tunnel-forwarded, allowlisted) `Host` header; HTTPS
/// by posture (TLS terminates at the tunnel).
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True if `uri` parses as an absolute `http`/`https` URL (DCR redirect sanity check).
fn is_absolute_http(uri: &str) -> bool {
    url::Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// 32 bytes of OS entropy, base64url (no padding) — for client ids, codes, and tokens.
fn random_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS RNG must be available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// PKCE S256: `base64url(sha256(verifier)) == challenge`, compared in constant time.
fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    token_matches(&computed, challenge)
}

/// Exact match, or a loopback redirect (`http://localhost` / `127.0.0.1`) differing only
/// by port — the claude.ai connector's per-session callback port varies (RFC 8252).
///
/// Loopback comparison parses the authority with a real URL parser, never string
/// prefixes: a crafted `http://localhost:1@evil.com/cb` resolves to host `evil.com`
/// (and `http://localhost.evil.com` to that host) and is rejected, so a registered
/// loopback callback can never be coerced into leaking the code to another origin.
fn redirect_uri_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    match (loopback_key(registered), loopback_key(requested)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The port-independent identity `(host, path, query)` of an `http://localhost` or
/// `http://127.0.0.1` URL with **no userinfo**; otherwise `None` (non-loopback or
/// malformed URLs only ever match exactly, via the equality check above).
fn loopback_key(uri: &str) -> Option<(String, String, Option<String>)> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.host_str()? {
        host @ ("localhost" | "127.0.0.1") => Some((
            host.to_string(),
            url.path().to_string(),
            url.query().map(str::to_string),
        )),
        _ => None,
    }
}

/// Minimal percent-encoding for a query-string value (RFC 3986 unreserved pass through).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Escape a string for inclusion in an HTML attribute value.
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The single-input gate page. Carries the OAuth params forward as hidden fields.
fn authorize_form(q: &AuthorizeQuery, error: Option<&str>) -> String {
    let hidden = |name: &str, value: &str| {
        format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            name,
            html_escape(value)
        )
    };
    let error_html = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let method = q.code_challenge_method.as_deref().unwrap_or("S256");
    let state = q.state.as_deref().unwrap_or("");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Authorize md-mcp</title><style>\
         body{{font-family:system-ui,sans-serif;max-width:24rem;margin:6rem auto;padding:0 1rem}}\
         h1{{font-size:1.2rem}}input[type=password]{{width:100%;padding:.5rem;font-size:1rem}}\
         button{{margin-top:1rem;padding:.5rem 1rem;font-size:1rem}}.err{{color:#b00}}\
         </style></head><body><h1>Authorize md-mcp</h1>\
         <p>Paste your access token (MD_HTTP_TOKEN) to connect this client.</p>{error_html}\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"password\" name=\"token\" autofocus autocomplete=\"off\">\
         {ci}{ru}{cc}{cm}{st}\
         <button type=\"submit\">Authorize</button></form></body></html>",
        ci = hidden("client_id", &q.client_id),
        ru = hidden("redirect_uri", &q.redirect_uri),
        cc = hidden("code_challenge", &q.code_challenge),
        cm = hidden("code_challenge_method", method),
        st = hidden("state", state),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_roundtrip() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert!(pkce_matches(verifier, &challenge));
        assert!(!pkce_matches("wrong-verifier", &challenge));
    }

    #[test]
    fn loopback_redirect_is_port_agnostic() {
        assert!(redirect_uri_matches(
            "http://localhost:1234/callback",
            "http://localhost:55999/callback"
        ));
        assert!(redirect_uri_matches(
            "http://127.0.0.1/cb",
            "http://127.0.0.1:8080/cb"
        ));
        // different path must not match
        assert!(!redirect_uri_matches(
            "http://localhost:1/a",
            "http://localhost:1/b"
        ));
        // non-loopback must match exactly
        assert!(redirect_uri_matches(
            "https://claude.ai/cb",
            "https://claude.ai/cb"
        ));
        assert!(!redirect_uri_matches(
            "https://claude.ai/cb",
            "https://evil.example/cb"
        ));
    }

    #[test]
    fn loopback_redirect_rejects_authority_smuggling() {
        let registered = "http://localhost:8723/callback";
        // userinfo moves the real host to evil.com — must NOT match.
        assert!(!redirect_uri_matches(
            registered,
            "http://localhost:53@evil.com/callback"
        ));
        // extra labels / lookalike hosts must NOT match.
        assert!(!redirect_uri_matches(
            registered,
            "http://localhost.evil.com/callback"
        ));
        assert!(!redirect_uri_matches(
            registered,
            "http://localhost:1234.evil.com/callback"
        ));
        assert!(!redirect_uri_matches(
            registered,
            "http://localhostEVIL/callback"
        ));
        // a different loopback path must NOT match.
        assert!(!redirect_uri_matches(
            registered,
            "http://localhost:9/other"
        ));
    }

    /// Bearer material at rest: the store must never be readable beyond its owner.
    #[cfg(unix)]
    #[test]
    fn persisted_state_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("nested/oauth-state.json");
        let oauth = OAuthState::load("gate-token".to_string(), state_file.clone());
        // Any store mutation persists; this one also creates the state dir.
        oauth.register_client(vec!["https://claude.ai/cb".to_string()]);

        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "state file must be owner-only");
    }

    #[test]
    fn percent_encode_escapes_query_chars() {
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_encode("safe-_.~"), "safe-_.~");
    }
}
