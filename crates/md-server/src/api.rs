//! Byte-level HTTP API for note I/O (ADR-0028).
//!
//! Bodies here are note bytes, not an envelope: a client with a shell works on
//! a note without any of it crossing a model's context. One note per request;
//! bulk work is the same requests in a loop over the index. MCP stays the
//! surface a model reads and writes through.

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::Uri;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use md_core::Code;
use md_core::text::nfc;

use md_core::Vault;

use crate::MdServer;
use crate::envelope::MAX_WRITE_BYTES;
use crate::events::EventOp;
use crate::oauth::{Scopes, percent_encode_path};

/// `text/markdown` is the vault's only content type; the charset is explicit so
/// a client does not have to guess at non-ASCII note bodies.
const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// One JSON object per line: a client can stream it, and one unreadable note
/// costs one line rather than the whole listing.
const NDJSON: &str = "application/x-ndjson";

/// How many notes the index lists, so a caller deciding whether to loop can
/// read one header instead of counting lines.
pub(crate) const NOTE_COUNT: axum::http::HeaderName =
    axum::http::HeaderName::from_static("note-count");

/// `/api/notes/...`, mounted beside `/mcp` and behind the same bearer guard.
pub(crate) fn routes(server: MdServer) -> Router {
    Router::new()
        .route("/api/notes", get(get_collection))
        .route(
            "/api/notes/{*path}",
            get(get_note)
                .put(put_note)
                // MCP's write limit, not axum's default 2 MiB: a note one
                // surface accepts, the other must not refuse.
                .layer(DefaultBodyLimit::max(MAX_WRITE_BYTES)),
        )
        .with_state(server)
}

async fn get_note(
    State(server): State<MdServer>,
    Path(path): Path<String>,
    scopes: Option<Extension<Scopes>>,
    headers: HeaderMap,
) -> Response {
    let authority = authority(scopes);
    if !authority.read {
        return forbidden("notes:read");
    }
    let path = nfc(&path);
    if !authority.permits(&path) {
        return outside_grant(&path);
    }
    let _guard = server.lock().read().await;
    let note = match server.vault().read_note(&path) {
        Ok(note) => note,
        Err(error) => return core_error(&error),
    };

    let etag = entity_tag(note.as_bytes());
    if none_match(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CONTENT_TYPE, MARKDOWN.to_string()),
        ],
        note,
    )
        .into_response()
}

/// One NDJSON line, written straight into the index body.
fn push_line(report: &mut String, value: &serde_json::Value) {
    use std::fmt::Write as _;
    let _ = writeln!(report, "{value}");
}

/// The collection is its index: every note this credential reaches — path,
/// entity tag and size, one JSON object per line, no content. Bulk transfer
/// is the single-note endpoints in a loop over these paths (ADR-0028), so
/// this is the one collection representation there is.
async fn get_collection(
    State(server): State<MdServer>,
    scopes: Option<Extension<Scopes>>,
    uri: Uri,
) -> Response {
    // Nothing a caller can pass: the grant already decides what the index
    // covers. A parameter is refused in this API's words, not serde's — it
    // teaches through its errors, and a silently dropped `prefx=` would take
    // that away exactly when it is needed.
    if uri.query().is_some_and(|q| !q.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            "the index takes no parameters; the grant already decides what it covers\n",
        )
            .into_response();
    }
    let authority = authority(scopes);
    if !authority.read {
        return forbidden("notes:read");
    }
    // One guard over listing and hashing, so the index is a consistent
    // snapshot rather than a walk through a moving vault.
    let _guard = server.lock().read().await;
    match entries_under(&server, authority.prefix.as_deref()) {
        Ok(entries) => index(&server, &entries),
        Err(refusal) => refusal.into_response(),
    }
}

/// The note paths a credential covers today. A confinement that matches
/// nothing yet — a directory the writing grant is about to create — is an
/// empty universe, and the truthful index of an empty universe is the empty
/// one, not a refusal that kills the caller's pipeline on its first line.
///
/// A prefix may name one note rather than a directory: the narrowest useful
/// grant is a single note, and a credential confined to one still has a
/// collection — it is that note. A protected prefix also answers empty, which
/// tells whoever holds such a grant nothing about what the server keeps there.
fn entries_under(
    server: &MdServer,
    prefix: Option<&str>,
) -> Result<Vec<String>, (StatusCode, String)> {
    let listing = |directory: &str| {
        server
            .vault()
            .list_entries(directory, true, None, false)
            .map(|entries| entries.into_iter().map(|entry| entry.path).collect())
            .map_err(|error| core_status(&error))
    };

    let prefix = prefix.map(|p| p.trim_matches('/')).unwrap_or_default();
    if prefix.is_empty() {
        return listing("");
    }
    if !Vault::is_internal_path(prefix) {
        if server.vault().is_dir(prefix).unwrap_or(false) {
            return listing(prefix);
        }
        if server.vault().exists(prefix).unwrap_or(false) {
            return Ok(vec![prefix.to_string()]);
        }
    }
    Ok(Vec::new())
}

/// Every note's path, entity tag and size, without its content — so a caller
/// can work out what changed and fetch only that. The tags are the same values
/// the note endpoint serves, and so can be replayed as `If-Match`. `url` is
/// the path percent-encoded: the one failure the server cannot teach through
/// an error is the request `curl` refuses to send, so the index hands every
/// loop a path that pastes into a URL raw.
fn index(server: &MdServer, paths: &[String]) -> Response {
    let mut body = String::new();
    for path in paths {
        // A note that cannot be read costs its own line, never the listing: a
        // silently dropped path reads to a syncing client as a deletion.
        let entry = match server.vault().read_note(path) {
            Ok(note) => serde_json::json!({
                "path": path,
                "url": percent_encode_path(path),
                "etag": entity_tag(note.as_bytes()),
                "size": note.len(),
            }),
            Err(error) => serde_json::json!({
                "path": path,
                "url": percent_encode_path(path),
                "error": error.message,
            }),
        };
        push_line(&mut body, &entry);
    }
    (
        [
            (header::CONTENT_TYPE, NDJSON.to_string()),
            (NOTE_COUNT, paths.len().to_string()),
        ],
        body,
    )
        .into_response()
}

/// Replace or create one note from the request body, verbatim.
///
/// A note is edited from a conversation while a script holds a copy of it, so a
/// replacement must say which version it was built from. Creation has nothing to
/// race against and needs no condition.
async fn put_note(
    State(server): State<MdServer>,
    Path(path): Path<String>,
    scopes: Option<Extension<Scopes>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let authority = authority(scopes);
    if !authority.write {
        return forbidden("notes:write");
    }
    let body = match body {
        Ok(body) => body,
        Err(rejection) => {
            // The cap fails inside the extractor, so the framework's words
            // would be the answer; a caller bisecting an undocumented limit
            // deserves the number instead.
            let response = rejection.into_response();
            if response.status() != StatusCode::PAYLOAD_TOO_LARGE {
                return response;
            }
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("this note is over the {MAX_WRITE_BYTES}-byte limit for a single write\n"),
            )
                .into_response();
        }
    };

    // Composed for the containment comparison below, which is a string test.
    // What lands on disk is composed by the vault (`Vault::resolve_rel`).
    let path = nfc(&path).into_owned();
    if !authority.permits(&path) {
        return outside_grant(&path);
    }

    // Held across the read-check-write so a concurrent writer in this process
    // cannot slip between the precondition and the write (ADR-0008).
    let _guard = server.lock().write().await;

    let current = match server.vault().read_note(&path) {
        Ok(note) => Some(note),
        Err(error) if error.code == Code::NotFound => None,
        Err(error) => return core_error(&error),
    };
    let condition = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok());

    match (&current, condition) {
        (Some(_), None) => return precondition_required(),
        (Some(note), Some(condition)) => {
            match if_match_check(condition, &entity_tag(note.as_bytes())) {
                IfMatch::Match => {}
                // Told apart from a stale tag: answering "the note changed" to
                // a tag that is merely unquoted sends a caller hunting a race
                // that never happened.
                IfMatch::Malformed => {
                    return (
                        StatusCode::BAD_REQUEST,
                        "an entity-tag is quoted: if-match: \"<etag>\" or *, exactly as the \
                         index and GET serve it\n",
                    )
                        .into_response();
                }
                IfMatch::Stale => {
                    return precondition_failed("the note changed since it was read");
                }
            }
        }
        // `If-Match` asserts a version of something that exists. There is no
        // version to disagree about here, and 412 would read as "your tag is
        // stale" and send a caller looking for a fresh one.
        (None, Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                format!(
                    "no note at {path:?} to match against; creating a new note takes no \
                     if-match header\n"
                ),
            )
                .into_response();
        }
        (None, None) => {}
    }

    if let Err(error) = server.vault().create_note(&path, &body, true) {
        return core_error(&error);
    }

    let created = current.is_none();
    let ops = [if created {
        EventOp::Create { path: path.clone() }
    } else {
        EventOp::Write { path: path.clone() }
    }];
    server.emit_event("put_note", None, &ops);
    server
        .auto_commit(
            "put_note",
            &ops,
            &serde_json::json!({ "path": path, "bytes": body.len() }),
        )
        .await;

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    (status, [(header::ETAG, entity_tag(&body))]).into_response()
}

/// The caller's authority. `require_bearer` attaches it when a token is
/// configured; an unguarded loopback server (ADR-0013) has no guard to consult,
/// so it is not restricted by one either.
fn authority(scopes: Option<Extension<Scopes>>) -> Scopes {
    scopes.map_or_else(Scopes::full, |Extension(scopes)| scopes)
}

/// A strong entity tag over exactly the bytes served, so it changes with
/// frontmatter and line endings too (ADR-0028).
fn entity_tag(bytes: &[u8]) -> String {
    format!("\"{}\"", blake3::hash(bytes).to_hex())
}

/// Whether `If-None-Match` already names the current representation. Accepts a
/// list, `*`, and the weak-comparison prefix, per RFC 9110.
fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.trim_start_matches("W/") == etag.trim_start_matches("W/")
    })
}

/// What an `If-Match` header amounts to against the current tag.
enum IfMatch {
    Match,
    /// Syntactically an entity-tag, but not this note's: the copy is stale.
    Stale,
    /// Not an entity-tag at all — usually a tag whose quotes were stripped.
    Malformed,
}

/// `If-Match` uses strong comparison, so a weak tag never satisfies it — but
/// it is still an entity-tag, where an unquoted hash is not one.
fn if_match_check(condition: &str, etag: &str) -> IfMatch {
    let quoted = |tag: &str| tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"');
    let mut any_valid = false;
    for candidate in condition.split(',').map(str::trim) {
        if candidate == "*" || candidate == etag {
            return IfMatch::Match;
        }
        any_valid |= quoted(candidate.strip_prefix("W/").unwrap_or(candidate));
    }
    if any_valid {
        IfMatch::Stale
    } else {
        IfMatch::Malformed
    }
}

fn precondition_required() -> Response {
    (
        StatusCode::PRECONDITION_REQUIRED,
        "replacing a note needs If-Match: its current ETag, or * to overwrite knowingly\n",
    )
        .into_response()
}

fn precondition_failed(why: &str) -> Response {
    (StatusCode::PRECONDITION_FAILED, format!("{why}\n")).into_response()
}

/// Refused for reaching past what the credential was confined to. Distinct
/// from a missing scope: nothing the caller can pass widens it.
fn outside_grant(path: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!("{path:?} is outside this credential's directory\n"),
    )
        .into_response()
}

fn forbidden(scope: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!("this token does not hold {scope}\n"),
    )
        .into_response()
}

fn core_error(error: &md_core::Error) -> Response {
    core_status(error).into_response()
}

fn core_status(error: &md_core::Error) -> (StatusCode, String) {
    let status = match error.code {
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, format!("{}\n", error.message))
}
