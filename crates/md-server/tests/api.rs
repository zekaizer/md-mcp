//! The byte-level transfer API (ADR-0028), driven over real HTTP.

use std::net::SocketAddr;

use md_core::Vault;
use md_server::MdServer;
use md_server::config::HttpConfig;

const NOTE: &str = "---\ntitle: Hi\n---\n# Heading\nbody\n";

/// Serve on an ephemeral loopback port with a vault holding `hello.md`.
async fn spawn(token: Option<&str>) -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_atomic("hello.md", NOTE.as_bytes()).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = HttpConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(str::to_string),
        allowed_hosts: Some(Vec::new()),
        allowed_origins: Some(Vec::new()),
        state_dir: state.path().to_path_buf(),
    };
    std::mem::forget(dir);
    std::mem::forget(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = md_server::http::router(MdServer::new(vault), &cfg);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    addr
}

#[tokio::test]
async fn serves_a_note_verbatim_with_an_etag() {
    let addr = spawn(None).await;
    let response = reqwest::get(format!("http://{addr}/api/notes/hello.md"))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let etag = response
        .headers()
        .get("etag")
        .expect("a transfer needs an entity tag to be conditional on")
        .to_str()
        .unwrap()
        .to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "got {etag}");
    assert_eq!(
        response.text().await.unwrap(),
        NOTE,
        "the body is the stored bytes, not an envelope"
    );
}

#[tokio::test]
async fn a_current_entity_tag_is_answered_304() {
    let addr = spawn(None).await;
    let url = format!("http://{addr}/api/notes/hello.md");
    let etag = reqwest::get(&url)
        .await
        .unwrap()
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let response = reqwest::Client::new()
        .get(&url)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 304);
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stale_entity_tag_transfers_the_note() {
    let addr = spawn(None).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/api/notes/hello.md"))
        .header("if-none-match", "\"0000\"")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), NOTE);
}

#[tokio::test]
async fn an_absent_note_is_404() {
    let addr = spawn(None).await;
    let response = reqwest::get(format!("http://{addr}/api/notes/nope.md"))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn the_transfer_api_is_behind_the_bearer_guard() {
    let addr = spawn(Some("secret")).await;
    let url = format!("http://{addr}/api/notes/hello.md");

    assert_eq!(reqwest::get(&url).await.unwrap().status(), 401);

    let response = reqwest::Client::new()
        .get(&url)
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// PUT a body, optionally under a precondition.
async fn put(
    addr: SocketAddr,
    path: &str,
    body: &str,
    if_match: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .put(format!("http://{addr}/api/notes/{path}"))
        .body(body.to_string());
    if let Some(value) = if_match {
        request = request.header("if-match", value);
    }
    request.send().await.unwrap()
}

async fn read_back(addr: SocketAddr, path: &str) -> String {
    reqwest::get(format!("http://{addr}/api/notes/{path}"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn current_tag(addr: SocketAddr, path: &str) -> String {
    reqwest::get(format!("http://{addr}/api/notes/{path}"))
        .await
        .unwrap()
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn creating_a_note_needs_no_precondition() {
    let addr = spawn(None).await;
    let response = put(addr, "fresh.md", "# Fresh\n", None).await;

    assert_eq!(response.status(), 201);
    assert!(
        response.headers().contains_key("etag"),
        "the caller needs the new tag to chain a further edit"
    );
    assert_eq!(read_back(addr, "fresh.md").await, "# Fresh\n");
}

#[tokio::test]
async fn replacing_without_a_precondition_is_refused() {
    let addr = spawn(None).await;
    let response = put(addr, "hello.md", "clobbered\n", None).await;

    assert_eq!(
        response.status(),
        428,
        "an unconditional replace is the lost-update hole this API must not open"
    );
    assert_eq!(read_back(addr, "hello.md").await, NOTE);
}

#[tokio::test]
async fn replacing_under_a_stale_precondition_is_refused() {
    let addr = spawn(None).await;
    let response = put(addr, "hello.md", "clobbered\n", Some("\"0000\"")).await;

    assert_eq!(response.status(), 412);
    assert_eq!(read_back(addr, "hello.md").await, NOTE);
}

#[tokio::test]
async fn replacing_under_the_current_precondition_succeeds() {
    let addr = spawn(None).await;
    let tag = current_tag(addr, "hello.md").await;

    let response = put(addr, "hello.md", "# Replaced\n", Some(&tag)).await;

    assert_eq!(response.status(), 204);
    let new_tag = response.headers().get("etag").unwrap().to_str().unwrap();
    assert_ne!(new_tag, tag, "the tag must move with the bytes");
    assert_eq!(read_back(addr, "hello.md").await, "# Replaced\n");
}

#[tokio::test]
async fn a_wildcard_precondition_overwrites_knowingly() {
    let addr = spawn(None).await;
    let response = put(addr, "hello.md", "# Deliberate\n", Some("*")).await;

    assert_eq!(response.status(), 204);
    assert_eq!(read_back(addr, "hello.md").await, "# Deliberate\n");
}

#[tokio::test]
async fn a_wildcard_precondition_on_an_absent_note_is_refused() {
    let addr = spawn(None).await;
    let response = put(addr, "nowhere.md", "# Nope\n", Some("*")).await;

    assert_eq!(
        response.status(),
        412,
        "`*` asserts the note exists; it must not quietly create one"
    );
}
