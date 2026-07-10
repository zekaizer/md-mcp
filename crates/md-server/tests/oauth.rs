//! End-to-end test of the co-hosted OAuth 2.1 authorization server (ADR-0014): a raw
//! HTTP client drives the full connector flow (discovery → DCR → authorize gate → token
//! exchange with PKCE), then an rmcp client calls `/mcp` with the issued access token.

use std::net::SocketAddr;

use md_core::Vault;
use md_server::MdServer;
use md_server::config::HttpConfig;
use rmcp::serve_client;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

/// RFC 7636 Appendix B PKCE pair (so the test needs no sha2/base64 itself).
const PKCE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const PKCE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT_URI: &str = "https://claude.ai/api/mcp/auth_callback";
const TOKEN: &str = "owner-secret";

async fn spawn() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_atomic("hello.md", b"# Hi\nbody\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = HttpConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: Some(TOKEN.to_string()),
        allowed_hosts: None,
        allowed_origins: None,
        state_dir: state.path().to_path_buf(),
    };
    std::mem::forget(dir);
    std::mem::forget(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = md_server::http::router(MdServer::new(vault), &cfg);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle)
}

/// Non-redirect client so the 303 from `/authorize` is observable.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn code_from_location(location: &str) -> String {
    location
        .split("code=")
        .nth(1)
        .expect("Location has a code")
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn full_oauth_flow_then_mcp_call() {
    let (addr, handle) = spawn().await;
    let base = format!("http://{addr}");
    let http = client();

    // Discovery: protected-resource metadata points at the MCP resource + this AS.
    let prm: Value = http
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(prm["resource"].as_str().unwrap().ends_with("/mcp"));
    assert!(!prm["authorization_servers"].as_array().unwrap().is_empty());

    // AS metadata advertises S256 PKCE.
    let asm: Value = http
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        asm["code_challenge_methods_supported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "S256")
    );

    // Dynamic Client Registration.
    let reg = http
        .post(format!("{base}/register"))
        .json(&json!({ "redirect_uris": [REDIRECT_URI] }))
        .send()
        .await
        .unwrap();
    assert_eq!(reg.status(), reqwest::StatusCode::CREATED);
    let reg: Value = reg.json().await.unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();

    let authorize_form = |token: &str| {
        vec![
            ("client_id", client_id.clone()),
            ("redirect_uri", REDIRECT_URI.to_string()),
            ("code_challenge", PKCE_CHALLENGE.to_string()),
            ("code_challenge_method", "S256".to_string()),
            ("token", token.to_string()),
        ]
    };

    // Wrong gate token: rejected, never redirected.
    let denied = http
        .post(format!("{base}/authorize"))
        .form(&authorize_form("wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Correct gate token: 3xx redirect to the registered callback with a code.
    let granted = http
        .post(format!("{base}/authorize"))
        .form(&authorize_form(TOKEN))
        .send()
        .await
        .unwrap();
    assert!(
        granted.status().is_redirection(),
        "status {}",
        granted.status()
    );
    let location = granted
        .headers()
        .get(reqwest::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(location.starts_with(REDIRECT_URI));
    let code = code_from_location(location);

    // Token exchange with PKCE verifier.
    let tok: Value = http
        .post(format!("{base}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", PKCE_VERIFIER),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access_token = tok["access_token"].as_str().unwrap().to_string();
    assert_eq!(tok["token_type"], "Bearer");

    // A no-bearer /mcp request is 401 and advertises the protected-resource metadata.
    let unauth = http.post(format!("{base}/mcp")).send().await.unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    let challenge = unauth
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        challenge.contains("resource_metadata="),
        "challenge: {challenge}"
    );
    assert!(challenge.contains("/.well-known/oauth-protected-resource"));

    // The issued access token authenticates a real MCP session.
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"))
            .auth_header(access_token),
    );
    let mcp = serve_client((), transport)
        .await
        .expect("oauth-token handshake");
    assert_eq!(mcp.list_all_tools().await.expect("list tools").len(), 11);

    mcp.cancel().await.ok();
    handle.abort();
}

/// A reused authorization code must not mint a second token (one-time codes).
#[tokio::test]
async fn authorization_code_is_single_use() {
    let (addr, handle) = spawn().await;
    let base = format!("http://{addr}");
    let http = client();

    let client_id = http
        .post(format!("{base}/register"))
        .json(&json!({ "redirect_uris": [REDIRECT_URI] }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_string();

    let granted = http
        .post(format!("{base}/authorize"))
        .form(&[
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("code_challenge", PKCE_CHALLENGE),
            ("code_challenge_method", "S256"),
            ("token", TOKEN),
        ])
        .send()
        .await
        .unwrap();
    let location = granted
        .headers()
        .get(reqwest::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    let code = code_from_location(location);

    let exchange = |code: String| {
        http.post(format!("{base}/token")).form(&[
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("code_verifier", PKCE_VERIFIER.to_string()),
            ("client_id", client_id.clone()),
            ("redirect_uri", REDIRECT_URI.to_string()),
        ])
    };

    assert_eq!(
        exchange(code.clone()).send().await.unwrap().status(),
        reqwest::StatusCode::OK,
        "first exchange succeeds"
    );
    assert_eq!(
        exchange(code).send().await.unwrap().status(),
        reqwest::StatusCode::BAD_REQUEST,
        "replayed code is rejected"
    );

    handle.abort();
}
