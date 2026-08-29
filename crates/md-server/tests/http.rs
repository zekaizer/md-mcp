//! End-to-end tests over the real Streamable HTTP transport (ADR-0013): rmcp's
//! HTTP client drives the axum server on an ephemeral loopback port — covering
//! the tool surface, bearer auth, and the cross-session concurrency invariant
//! (all sessions share one vault and one write lock; ADR-0008).

use std::net::SocketAddr;
use std::time::Duration;

use md_core::Vault;
use md_server::MdServer;
use md_server::config::HttpConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::serve_client;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

/// Start the HTTP server on an ephemeral loopback port; returns its address and
/// the serving task handle. The vault is seeded with `hello.md`.
async fn spawn_server(token: Option<&str>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault
        .write_atomic("hello.md", b"---\ntitle: Hi\n---\n# Heading\nbody\n")
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    let state_dir = state.path().to_path_buf();
    std::mem::forget(dir); // keep the temp dirs alive for the server's lifetime
    std::mem::forget(state);
    let server = MdServer::new(vault);

    let cfg = HttpConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(str::to_string),
        allowed_hosts: None,
        allowed_origins: None,
        state_dir,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = md_server::http::router(server, &cfg);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle)
}

fn create_args(path: &str, body: &str) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    args.insert("notes".into(), json!([{ "path": path, "content": body }]));
    args
}

fn append_args(path: &str, body: &str) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    args.insert("appends".into(), json!([{ "path": path, "content": body }]));
    args
}

#[tokio::test]
async fn http_client_lists_tools_and_calls_one() {
    let (addr, handle) = spawn_server(None).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client = serve_client((), transport).await.expect("client handshake");

    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(
        tools.len(),
        12,
        "an unguarded server has no bearer to delegate, so no provision_transfer"
    );

    let mut args = serde_json::Map::new();
    args.insert("paths".into(), json!(["hello.md", "missing.md"]));
    let result = client
        .call_tool(CallToolRequestParams::new("read_notes").with_arguments(args))
        .await
        .expect("call read_notes");
    let structured: Value = result.structured_content.expect("structured content");
    let notes = structured["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["frontmatter"]["title"], json!("Hi"));
    assert_eq!(notes[1]["exists"], json!(false));

    client.cancel().await.ok();
    handle.abort();
}

#[tokio::test]
async fn http_requires_bearer_when_token_set() {
    let (addr, handle) = spawn_server(Some("s3cret")).await;
    let url = format!("http://{addr}/mcp");

    // No bearer → the initialize handshake is rejected (401).
    let anon = StreamableHttpClientTransport::from_uri(url.clone());
    let handshake = tokio::time::timeout(Duration::from_secs(10), serve_client((), anon)).await;
    assert!(
        matches!(handshake, Ok(Err(_))),
        "handshake without a bearer token must be rejected, got {handshake:?}"
    );

    // Correct bearer → handshake succeeds and the tool surface is available.
    let authed = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header("s3cret"),
    );
    let client = serve_client((), authed).await.expect("authed handshake");
    assert_eq!(client.list_all_tools().await.expect("list tools").len(), 13);

    client.cancel().await.ok();
    handle.abort();
}

/// Two independent HTTP sessions must share ONE vault and ONE write lock. The two
/// sessions concurrently append to the SAME note — a read-modify-write that, if
/// the sessions held independent locks (or none), would drop updates under
/// contention. Every appended line surviving proves the commit lock serializes
/// writes across sessions (ADR-0008), and session 2 reading session 1's note
/// proves the shared `Arc<Vault>`. Runs on a multi-thread runtime so the appends
/// truly race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_share_one_vault_and_serialize_writes() {
    let (addr, handle) = spawn_server(None).await;
    let url = format!("http://{addr}/mcp");

    let s1 = serve_client((), StreamableHttpClientTransport::from_uri(url.clone()))
        .await
        .expect("session 1 handshake");
    let s2 = serve_client((), StreamableHttpClientTransport::from_uri(url.clone()))
        .await
        .expect("session 2 handshake");

    // Session 1 seeds a shared note.
    s1.call_tool(
        CallToolRequestParams::new("create_notes")
            .with_arguments(create_args("shared.md", "# Shared\n")),
    )
    .await
    .expect("create shared.md");

    // Both sessions hammer the same note concurrently, round after round.
    const ROUNDS: usize = 8;
    for i in 0..ROUNDS {
        let a = s1.call_tool(
            CallToolRequestParams::new("append_notes")
                .with_arguments(append_args("shared.md", &format!("s1-{i}\n"))),
        );
        let b = s2.call_tool(
            CallToolRequestParams::new("append_notes")
                .with_arguments(append_args("shared.md", &format!("s2-{i}\n"))),
        );
        let (ra, rb) = tokio::join!(a, b);
        assert_ne!(ra.expect("s1 append").is_error, Some(true));
        assert_ne!(rb.expect("s2 append").is_error, Some(true));
    }

    // Session 2 reads back the note session 1 created and both appended to.
    let mut args = serde_json::Map::new();
    args.insert("paths".into(), json!(["shared.md"]));
    let read = s2
        .call_tool(CallToolRequestParams::new("read_notes").with_arguments(args))
        .await
        .expect("read shared.md");
    let notes = read.structured_content.expect("structured")["notes"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(
        notes[0]["exists"],
        json!(true),
        "shared note seen by session 2"
    );
    let content = notes[0]["content"].as_str().unwrap_or_default();
    for i in 0..ROUNDS {
        assert!(
            content.contains(&format!("s1-{i}")),
            "lost s1-{i}: {content:?}"
        );
        assert!(
            content.contains(&format!("s2-{i}")),
            "lost s2-{i}: {content:?}"
        );
    }

    s1.cancel().await.ok();
    s2.cancel().await.ok();
    handle.abort();
}

/// Ask for a transfer grant over MCP and return the raw MCP answer.
async fn grant(addr: SocketAddr, write: bool) -> Value {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header("s3cret"),
    );
    let client = serve_client((), transport).await.expect("client handshake");
    let mut args = serde_json::Map::new();
    args.insert("write".into(), json!(write));
    let result = client
        .call_tool(CallToolRequestParams::new("provision_transfer").with_arguments(args))
        .await
        .expect("call provision_transfer");
    result.structured_content.expect("structured content")
}

/// Trade a ticket for the token it stands for, as the recipe's first line does.
async fn redeem(addr: SocketAddr, code: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}/transfer/redeem"))
        .form(&[("code", code)])
        .send()
        .await
        .unwrap()
}

/// The whole first step of the recipe: provision, then collect.
async fn provision(addr: SocketAddr, write: bool) -> (String, Value) {
    let grant = grant(addr, write).await;
    let code = grant["code"].as_str().expect("a ticket").to_string();
    let token = redeem(addr, &code)
        .await
        .text()
        .await
        .unwrap()
        .trim()
        .to_string();
    (token, grant["scopes"].clone())
}

#[tokio::test]
async fn the_grant_itself_carries_no_credential() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let grant = grant(addr, true).await;

    assert!(
        grant.get("token").is_none(),
        "a credential in the answer would sit in the conversation for good: {grant}"
    );
    assert!(grant["code"].as_str().is_some_and(|c| c.len() > 20));
}

#[tokio::test]
async fn a_ticket_is_spent_by_its_first_use() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let grant = grant(addr, false).await;
    let code = grant["code"].as_str().unwrap().to_string();

    let first = redeem(addr, &code).await;
    assert_eq!(first.status(), 200);
    assert!(!first.text().await.unwrap().trim().is_empty());

    let second = redeem(addr, &code).await;
    assert_eq!(
        second.status(),
        400,
        "a ticket left behind in a transcript has to be worthless"
    );
}

#[tokio::test]
async fn an_unknown_ticket_buys_nothing() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    assert_eq!(redeem(addr, "not-a-ticket").await.status(), 400);
}

#[tokio::test]
async fn a_provisioned_credential_reads_but_cannot_write() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let (token, scopes) = provision(addr, false).await;
    assert_eq!(scopes, json!(["notes:read"]), "writing is opt-in");

    let http = reqwest::Client::new();
    let read = http
        .get(format!("http://{addr}/api/notes/hello.md"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 200);

    let write = http
        .put(format!("http://{addr}/api/notes/new.md"))
        .bearer_auth(&token)
        .body("# New\n")
        .send()
        .await
        .unwrap();
    assert_eq!(
        write.status(),
        403,
        "a token minted weaker than its parent has to actually be weaker"
    );
}

#[tokio::test]
async fn a_provisioned_credential_writes_when_asked_to() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let (token, scopes) = provision(addr, true).await;
    assert_eq!(scopes, json!(["notes:read", "notes:write"]));

    let write = reqwest::Client::new()
        .put(format!("http://{addr}/api/notes/new.md"))
        .bearer_auth(&token)
        .body("# New\n")
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 201);
}

#[tokio::test]
async fn the_static_token_is_not_what_gets_handed_out() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let (token, _) = provision(addr, true).await;
    assert_ne!(
        token, "s3cret",
        "handing back the parent bearer would defeat the whole grant"
    );
}

#[tokio::test]
async fn a_collected_token_renews_itself_and_the_old_one_stops_working() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let (token, _) = provision(addr, false).await;
    let http = reqwest::Client::new();
    let note = format!("http://{addr}/api/notes/hello.md");

    let response = http
        .post(format!("http://{addr}/transfer/renew"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let fresh = response.text().await.unwrap().trim().to_string();
    assert_ne!(fresh, token);

    assert_eq!(
        http.get(&note)
            .bearer_auth(&fresh)
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "a long job renews rather than dying halfway"
    );
    assert_eq!(
        http.get(&note)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "renewal replaces a credential; it must not multiply them"
    );
}

#[tokio::test]
async fn the_connector_bearer_cannot_renew_through_the_transfer_path() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/transfer/renew"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        401,
        "letting the parent bearer in here would launder it into an endless chain"
    );
}

#[tokio::test]
async fn the_grant_can_be_asked_for_with_no_arguments_at_all() {
    let (addr, _handle) = spawn_server(Some("s3cret")).await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header("s3cret"),
    );
    let client = serve_client((), transport).await.expect("client handshake");

    // Every field has a default, so a caller that passes nothing is asking for
    // the default grant, not making a mistake.
    let bare = client
        .call_tool(CallToolRequestParams::new("provision_transfer"))
        .await
        .expect("a tool whose arguments are all optional must accept none");
    assert!(
        bare.structured_content.expect("structured content")["code"]
            .as_str()
            .is_some()
    );

    let empty = client
        .call_tool(
            CallToolRequestParams::new("provision_transfer").with_arguments(serde_json::Map::new()),
        )
        .await
        .expect("an empty argument object is the same request");
    assert!(
        empty.structured_content.expect("structured content")["code"]
            .as_str()
            .is_some()
    );
}
