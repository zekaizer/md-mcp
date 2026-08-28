//! Byte-level HTTP API for note transfer (ADR-0028).
//!
//! Bodies here are note bytes, not an envelope: a client with a filesystem can
//! move a vault around without any of it crossing a model's context. MCP stays
//! the surface a model reads and writes through.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use md_core::Code;
use md_core::text::nfc;

use crate::MdServer;
use crate::envelope::MAX_WRITE_BYTES;
use crate::events::EventOp;
use crate::oauth::Scopes;

/// `text/markdown` is the vault's only content type; the charset is explicit so
/// a client does not have to guess at non-ASCII note bodies.
const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// `/api/notes/...`, mounted beside `/mcp` and behind the same bearer guard.
pub(crate) fn routes(server: MdServer) -> Router {
    Router::new()
        .route("/api/notes/{*path}", get(get_note).put(put_note))
        .with_state(server)
}

async fn get_note(
    State(server): State<MdServer>,
    Path(path): Path<String>,
    scopes: Option<Extension<Scopes>>,
    headers: HeaderMap,
) -> Response {
    if !authority(scopes).read {
        return forbidden("notes:read");
    }
    let path = nfc(&path);
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
    body: Bytes,
) -> Response {
    if !authority(scopes).write {
        return forbidden("notes:write");
    }
    if body.len() > MAX_WRITE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "note is {} bytes, over the {MAX_WRITE_BYTES} limit\n",
                body.len()
            ),
        )
            .into_response();
    }

    // A client filesystem may spell a name decomposed; the vault stores one
    // composed spelling so the same note never lands under two paths (ADR-0028).
    let path = nfc(&path).into_owned();

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
            if !if_match_satisfied(condition, &entity_tag(note.as_bytes())) {
                return precondition_failed("the note changed since it was read");
            }
        }
        // `If-Match` asserts a version of something that exists; on an absent
        // note it fails rather than quietly creating one.
        (None, Some(_)) => return precondition_failed("no such note to match against"),
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

/// `If-Match` uses strong comparison, so a weak tag never satisfies it.
fn if_match_satisfied(condition: &str, etag: &str) -> bool {
    condition
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate == etag)
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

fn forbidden(scope: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!("this token does not hold {scope}\n"),
    )
        .into_response()
}

fn core_error(error: &md_core::Error) -> Response {
    let status = match error.code {
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, format!("{}\n", error.message)).into_response()
}
