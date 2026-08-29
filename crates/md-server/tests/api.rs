//! The byte-level note I/O API (ADR-0028), driven over real HTTP.

use std::net::SocketAddr;

use md_core::Vault;
use md_server::MdServer;
use md_server::config::HttpConfig;

const NOTE: &str = "---\ntitle: Hi\n---\n# Heading\nbody\n";

/// Serve on an ephemeral loopback port with a vault holding `hello.md`.
async fn spawn(token: Option<&str>) -> SocketAddr {
    spawn_with_root(token).await.0
}

/// As [`spawn`], also handing back the vault root for tests that care what
/// landed on disk rather than what the API answered.
async fn spawn_with_root(token: Option<&str>) -> (SocketAddr, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
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
    (addr, root)
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
        404,
        "`*` asserts the note exists, so it must not quietly create one — and \
         there is no version to disagree about, which is what 412 would claim"
    );
    let message = response.text().await.unwrap();
    assert!(
        message.contains("no if-match") || message.contains("without if-match"),
        "this is the first error a caller creating notes in a loop meets, so \
         it has to name the way out — omit the header: {message:?}"
    );
}

#[tokio::test]
async fn an_unquoted_tag_is_a_syntax_error_not_a_stale_one() {
    let addr = spawn(None).await;
    let quoted = current_tag(addr, "hello.md").await;
    let bare = quoted.trim_matches('"').to_string();

    let response = put(addr, "hello.md", "# new\n", Some(&bare)).await;

    assert_eq!(
        response.status(),
        400,
        "the tag is current, so `the note changed` would be a lie that sends \
         the caller hunting a race that never happened"
    );
    let message = response.text().await.unwrap();
    assert!(
        message.contains("quoted"),
        "the repair is quoting, so the error must say so: {message:?}"
    );
    assert_ne!(read_back(addr, "hello.md").await, "# new\n");
}

#[tokio::test]
async fn an_oversize_note_is_refused_in_this_api_s_own_words() {
    let addr = spawn(None).await;

    let response = put(addr, "big.md", &"x".repeat(4 * 1024 * 1024 + 1), None).await;
    assert_eq!(response.status(), 413);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("4194304"),
        "an undocumented limit is learned by bisection; the refusal must name \
         it: {message:?}"
    );

    let fits = put(addr, "big.md", &"x".repeat(4 * 1024 * 1024), None).await;
    assert_eq!(
        fits.status(),
        201,
        "the HTTP limit is MCP's limit: a note one surface accepts, the other \
         must not refuse"
    );
}

#[tokio::test]
async fn a_directory_is_not_a_note() {
    let addr = spawn_with_tree().await;

    for path in ["inbox", "inbox/"] {
        let response = reqwest::get(format!("http://{addr}/api/notes/{path}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path:?} names no note");
        let message = response.text().await.unwrap();
        assert!(
            !message.contains("os error"),
            "an errno is the server's business, not an answer: {message:?}"
        );
    }
}

#[tokio::test]
async fn a_decomposed_name_is_stored_composed() {
    use unicode_normalization::UnicodeNormalization;

    let (addr, root) = spawn_with_root(None).await;
    let decomposed: String = "오픽.md".nfd().collect();
    assert_ne!(decomposed, "오픽.md", "the fixture must actually differ");

    assert_eq!(put(addr, &decomposed, "# x\n", None).await.status(), 201);

    let names: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"오픽.md".to_string()),
        "a client filesystem hands over decomposed names; storing one verbatim \
         leaves the vault holding two spellings of one note. got: {names:?}"
    );
}

/// Seed a second note so a prefix has something to exclude.
async fn spawn_with_tree() -> SocketAddr {
    let addr = spawn(None).await;
    assert_eq!(
        put(addr, "inbox/one.md", "# One\n", None).await.status(),
        201
    );
    assert_eq!(
        put(addr, "inbox/two.md", "# Two\n", None).await.status(),
        201
    );
    addr
}

async fn index_lines(addr: SocketAddr) -> Vec<serde_json::Value> {
    let body = reqwest::get(format!("http://{addr}/api/notes"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

#[tokio::test]
async fn the_index_names_every_note_with_its_tag_and_size() {
    let addr = spawn_with_tree().await;
    let entries = index_lines(addr).await;

    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["hello.md", "inbox/one.md", "inbox/two.md"]);

    let one = entries
        .iter()
        .find(|e| e["path"] == "inbox/one.md")
        .unwrap();
    assert_eq!(one["size"].as_u64().unwrap(), "# One\n".len() as u64);
    assert!(one["etag"].as_str().unwrap().starts_with('"'));
}

#[tokio::test]
async fn a_query_parameter_is_refused_rather_than_ignored() {
    let addr = spawn_with_tree().await;

    for query in ["prefix=inbox", "format=index", "confirm=true", "prefx=x"] {
        let response = reqwest::get(format!("http://{addr}/api/notes?{query}"))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "the index takes no parameters; this API teaches through its \
             errors, and silently ignoring {query:?} would take that away \
             exactly when it is needed"
        );
    }
}

#[tokio::test]
async fn the_index_carries_a_url_a_shell_can_paste_raw() {
    let addr = spawn(None).await;
    assert_eq!(
        put(addr, "inbox/한글 노트.md", "# x\n", None)
            .await
            .status(),
        201
    );

    let entries = index_lines(addr).await;
    let entry = entries
        .iter()
        .find(|e| e["path"] == "inbox/한글 노트.md")
        .expect("the note is indexed");
    let url = entry["url"].as_str().expect("a url field");

    assert!(
        url.is_ascii() && !url.contains(' '),
        "curl rejects a raw space or non-ASCII before it ever sends the \
         request, and the index is the one place a loop gets its paths: {url}"
    );
    assert_eq!(
        reqwest::get(format!("http://{addr}/api/notes/{url}"))
            .await
            .unwrap()
            .status(),
        200,
        "the url must resolve to the note it stands for"
    );
}

#[tokio::test]
async fn an_indexed_tag_is_the_one_the_note_endpoint_serves() {
    let addr = spawn_with_tree().await;
    let entries = index_lines(addr).await;
    let indexed = entries.iter().find(|e| e["path"] == "hello.md").unwrap()["etag"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        indexed,
        current_tag(addr, "hello.md").await,
        "an index whose tags cannot be replayed as If-Match is useless for sync"
    );
}

#[tokio::test]
async fn the_vault_stays_markdown() {
    let addr = spawn(None).await;

    let response = put(addr, "script.sh", "echo hi\n", None).await;

    assert_eq!(
        response.status(),
        400,
        "a file the listing will never show is a ghost: writable, readable by \
         path, and invisible to every pull, so a round trip silently drops it"
    );
    assert_eq!(
        reqwest::get(format!("http://{addr}/api/notes/script.sh"))
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn a_refused_credential_says_what_to_do_about_it() {
    let addr = spawn(Some("secret")).await;

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/api/notes/hello.md"))
        .bearer_auth("stale-or-forged")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("provision_transfer"),
        "a transfer token lapses in ten minutes, so this is the failure a caller \
         meets most often; answering with an empty body makes it the only one \
         that explains nothing: {message:?}"
    );
}

#[tokio::test]
async fn the_index_states_its_size_in_a_header() {
    let addr = spawn_with_tree().await;

    let response = reqwest::get(format!("http://{addr}/api/notes"))
        .await
        .unwrap();

    assert_eq!(
        response.headers()["note-count"],
        "3",
        "a caller deciding whether to loop should not have to count lines to \
         learn how many there are"
    );
}

#[tokio::test]
async fn the_collection_accepts_no_push() {
    let addr = spawn(None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/notes"))
        .body("# anything\n")
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        405,
        "bulk transfer is a loop over the single-note endpoints; an archive \
         parser at an authenticated boundary is a hole habitat this surface \
         no longer keeps"
    );
}

#[tokio::test]
async fn a_protected_path_is_not_served() {
    let addr = spawn(None).await;

    let response = reqwest::get(format!("http://{addr}/api/notes/.md-mcp/journal.md"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        404,
        "the server's own state is not a note, however it is addressed"
    );
}
