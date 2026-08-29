//! The byte-level transfer API (ADR-0028), driven over real HTTP.

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
        412,
        "`*` asserts the note exists; it must not quietly create one"
    );
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

async fn index_lines(addr: SocketAddr, query: &str) -> Vec<serde_json::Value> {
    let body = reqwest::get(format!("http://{addr}/api/notes?{query}"))
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
    let entries = index_lines(addr, "format=index").await;

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
async fn a_prefix_narrows_the_index() {
    let addr = spawn_with_tree().await;
    let paths: Vec<String> = index_lines(addr, "format=index&prefix=inbox")
        .await
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(paths, ["inbox/one.md", "inbox/two.md"]);
}

#[tokio::test]
async fn an_indexed_tag_is_the_one_the_note_endpoint_serves() {
    let addr = spawn_with_tree().await;
    let entries = index_lines(addr, "format=index").await;
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
async fn an_unsupported_collection_format_is_refused() {
    let addr = spawn(None).await;
    let response = reqwest::get(format!("http://{addr}/api/notes?format=zip"))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

/// Build a tar holding `(path, body)` pairs, as `tar -cf -` would.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, body.as_bytes())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// Every regular file in a tar, as `(path, body)`.
fn untar(bytes: &[u8]) -> Vec<(String, String)> {
    let mut archive = tar::Archive::new(bytes);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut body = String::new();
            std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
            (path, body)
        })
        .collect()
}

async fn post_tar(addr: SocketAddr, query: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}/api/notes?{query}"))
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_subtree_is_served_as_one_tar() {
    let addr = spawn_with_tree().await;
    let response = reqwest::get(format!("http://{addr}/api/notes?prefix=inbox"))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/x-tar",
        "a client pipes this straight into `tar -xf -`"
    );
    let files = untar(&response.bytes().await.unwrap());
    assert_eq!(
        files,
        vec![
            ("inbox/one.md".to_string(), "# One\n".to_string()),
            ("inbox/two.md".to_string(), "# Two\n".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_posted_tar_creates_every_note_in_one_round_trip() {
    let addr = spawn(None).await;
    let body = tar_of(&[("a.md", "# A\n"), ("b.md", "# B\n")]);

    let response = post_tar(addr, "to=imported", body).await;

    assert_eq!(response.status(), 200);
    assert_eq!(read_back(addr, "imported/a.md").await, "# A\n");
    assert_eq!(read_back(addr, "imported/b.md").await, "# B\n");
}

#[tokio::test]
async fn a_posted_tar_does_not_clobber_by_default() {
    let addr = spawn(None).await;
    let body = tar_of(&[("hello.md", "clobbered\n")]);

    let response = post_tar(addr, "", body).await;

    assert_eq!(response.status(), 207, "refusing an entry is not a success");
    let report = response.text().await.unwrap();
    assert!(
        report.contains("hello.md") && report.contains("error"),
        "the untouched note has to be reported, not silently skipped: {report}"
    );
    assert_eq!(read_back(addr, "hello.md").await, NOTE);
}

#[tokio::test]
async fn a_posted_tar_replaces_when_told_to() {
    let addr = spawn(None).await;
    let body = tar_of(&[("hello.md", "# Deliberate\n")]);

    let response = post_tar(addr, "overwrite=true", body).await;

    assert_eq!(response.status(), 200);
    assert_eq!(read_back(addr, "hello.md").await, "# Deliberate\n");
}

/// A tar whose entry names bypass `Builder`'s own refusal to write `..`, so the
/// server is tested against what a hostile client can actually send rather than
/// against what the writing library permits.
fn hostile_tar(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut header = tar::Header::new_old();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let raw = header.as_old_mut();
        raw.name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, body.as_bytes()).unwrap();
    }
    builder.into_inner().unwrap()
}

#[tokio::test]
async fn a_posted_tar_cannot_escape_the_vault() {
    let (addr, root) = spawn_with_root(None).await;
    let body = hostile_tar(&[("../escaped.md", "# Nope\n"), ("ok.md", "# Ok\n")]);

    let response = post_tar(addr, "", body).await;

    assert_eq!(response.status(), 207, "refusing an entry is not a success");
    let report = response.text().await.unwrap();
    assert!(
        report.contains("error"),
        "a traversing entry must be refused: {report}"
    );
    assert!(
        !root.parent().unwrap().join("escaped.md").exists(),
        "a tar entry wrote outside the vault root"
    );
    assert_eq!(
        read_back(addr, "ok.md").await,
        "# Ok\n",
        "one refused entry must not abandon the rest of the push"
    );
}

#[tokio::test]
async fn an_unknown_prefix_is_not_an_empty_result() {
    let addr = spawn_with_tree().await;

    // The vault holds `inbox`, not `00-inbox` — the shape of a real mistype.
    let tar = reqwest::get(format!("http://{addr}/api/notes?prefix=00-inbox"))
        .await
        .unwrap();
    assert_eq!(
        tar.status(),
        404,
        "a mistyped prefix must not come back as a silently empty archive"
    );

    let index = reqwest::get(format!(
        "http://{addr}/api/notes?format=index&prefix=00-inbox"
    ))
    .await
    .unwrap();
    assert_eq!(index.status(), 404);
}

#[tokio::test]
async fn a_directory_holding_no_notes_is_an_empty_archive() {
    let (addr, root) = spawn_with_root(None).await;
    std::fs::create_dir(root.join("drafts")).unwrap();

    let response = reqwest::get(format!("http://{addr}/api/notes?prefix=drafts"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "a real directory that happens to hold nothing is not an error"
    );
    assert_eq!(response.headers()["note-count"], "0");
}

#[tokio::test]
async fn an_archive_says_how_many_notes_it_carries() {
    let addr = spawn_with_tree().await;
    let response = reqwest::get(format!("http://{addr}/api/notes?prefix=inbox/"))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["note-count"],
        "2",
        "a caller should not have to parse the archive to know it got anything"
    );
}

#[tokio::test]
async fn the_index_needs_no_such_asking() {
    let addr = spawn_with_tree().await;

    let response = reqwest::get(format!("http://{addr}/api/notes?format=index"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "learning the size of the vault moves no content, and is exactly what a \
         caller should do before deciding to take it all"
    );
    assert_eq!(response.text().await.unwrap().lines().count(), 3);
}

/// Seed `count` notes so a transfer is big enough to be worth questioning.
async fn spawn_with_many(count: usize) -> SocketAddr {
    let (addr, root) = spawn_with_root(None).await;
    std::fs::create_dir_all(root.join("bulk")).unwrap();
    for i in 0..count {
        std::fs::write(root.join(format!("bulk/note-{i:03}.md")), "# x\n").unwrap();
    }
    addr
}

#[tokio::test]
async fn a_small_transfer_needs_no_ceremony() {
    let addr = spawn_with_tree().await;

    let response = reqwest::get(format!("http://{addr}/api/notes"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "three notes is not a bulk exfiltration; gating on whether a prefix was \
         typed measures the wrong thing"
    );
    assert_eq!(response.headers()["note-count"], "3");
}

#[tokio::test]
async fn a_large_transfer_states_its_size_and_asks() {
    let addr = spawn_with_many(60).await;

    let response = reqwest::get(format!("http://{addr}/api/notes"))
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("61") && message.contains("confirm=true"),
        "the refusal has to name the real size and the one way forward, so the \
         caller needs no second guess: {message}"
    );
}

#[tokio::test]
async fn a_narrowed_transfer_is_gated_by_its_own_size() {
    let addr = spawn_with_many(60).await;

    let response = reqwest::get(format!("http://{addr}/api/notes?prefix=bulk"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        400,
        "naming a directory says nothing about how much is in it"
    );
}

#[tokio::test]
async fn a_confirmed_large_transfer_is_served() {
    let addr = spawn_with_many(60).await;

    let response = reqwest::get(format!("http://{addr}/api/notes?confirm=true"))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["note-count"], "61");
}

#[tokio::test]
async fn the_index_is_never_gated_however_large_the_vault() {
    let addr = spawn_with_many(60).await;

    let response = reqwest::get(format!("http://{addr}/api/notes?format=index"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "the index moves no content and is how a caller learns the size it is \
         being asked to confirm"
    );
    assert_eq!(response.text().await.unwrap().lines().count(), 61);
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
async fn a_pushed_tar_refuses_what_is_not_a_note_and_keeps_the_rest() {
    let addr = spawn(None).await;
    let body = tar_of(&[("keep.md", "# Keep\n"), ("evil.sh", "echo hi\n")]);

    let response = post_tar(addr, "", body).await;
    let report = response.text().await.unwrap();

    assert!(
        report.contains("evil.sh") && report.contains("error"),
        "{report}"
    );
    assert_eq!(read_back(addr, "keep.md").await, "# Keep\n");
}

#[tokio::test]
async fn a_reported_path_can_be_used_as_given() {
    let addr = spawn(None).await;
    let body = hostile_tar(&[("/absolute.md", "# Abs\n")]);

    let report = post_tar(addr, "to=landing", body)
        .await
        .text()
        .await
        .unwrap();

    assert!(
        !report.contains("//"),
        "a script feeds the reported path into its next request; a doubled \
         separator makes that a different path: {report}"
    );
    assert!(report.contains("landing/absolute.md"), "{report}");
}

#[tokio::test]
async fn a_push_that_refused_something_does_not_report_success() {
    let addr = spawn(None).await;

    let all_written = post_tar(addr, "", tar_of(&[("fresh.md", "# F\n")])).await;
    assert_eq!(all_written.status(), 200);

    let partly_refused = post_tar(
        addr,
        "",
        tar_of(&[("other.md", "# O\n"), ("hello.md", "# clobber\n")]),
    )
    .await;

    assert_eq!(
        partly_refused.status(),
        207,
        "`curl -sSf … && echo ok` on a push that wrote nothing must not print ok"
    );
    assert_eq!(read_back(addr, "hello.md").await, NOTE);
    assert_eq!(read_back(addr, "other.md").await, "# O\n");
}

#[tokio::test]
async fn a_mistyped_parameter_is_refused_rather_than_ignored() {
    let addr = spawn_with_tree().await;

    let response = reqwest::get(format!("http://{addr}/api/notes?prefx=inbox"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        400,
        "silently ignoring it would answer with the whole vault while the \
         caller believes it narrowed the request"
    );
}

#[tokio::test]
async fn a_protected_directory_is_not_a_prefix() {
    let (addr, root) = spawn_with_root(None).await;
    std::fs::create_dir_all(root.join(".md-mcp")).unwrap();

    let response = reqwest::get(format!("http://{addr}/api/notes?prefix=.md-mcp"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        404,
        "answering 200 with an empty archive is exactly the confusion this \
         endpoint refuses everywhere else: the caller cannot tell a directory \
         it may not have from one that is empty"
    );
}

#[tokio::test]
async fn a_directory_entry_is_skipped_without_counting_as_a_refusal() {
    let addr = spawn(None).await;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, "sub/", &[][..]).unwrap();
    let mut note = tar::Header::new_gnu();
    note.set_size(7);
    note.set_mode(0o644);
    note.set_cksum();
    builder
        .append_data(&mut note, "sub/a.md", &b"# A\n\nx"[..])
        .unwrap();

    let response = post_tar(addr, "", builder.into_inner().unwrap()).await;

    assert_eq!(
        response.status(),
        200,
        "`tar -cf - .` always carries directories; counting them as refusals \
         would make every ordinary push look partly failed"
    );
    assert!(!response.text().await.unwrap().contains("error"));
}
