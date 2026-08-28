//! Byte-level HTTP API for note transfer (ADR-0028).
//!
//! Bodies here are note bytes, not an envelope: a client with a filesystem can
//! move a vault around without any of it crossing a model's context. MCP stays
//! the surface a model reads and writes through.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use md_core::Code;

use crate::MdServer;
use crate::oauth::Scopes;

/// `text/markdown` is the vault's only content type; the charset is explicit so
/// a client does not have to guess at non-ASCII note bodies.
const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// `/api/notes/...`, mounted beside `/mcp` and behind the same bearer guard.
pub(crate) fn routes(server: MdServer) -> Router {
    Router::new()
        .route("/api/notes/{*path}", get(get_note))
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
