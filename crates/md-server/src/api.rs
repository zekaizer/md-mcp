//! Byte-level HTTP API for note transfer (ADR-0028).
//!
//! Bodies here are note bytes, not an envelope: a client with a filesystem can
//! move a vault around without any of it crossing a model's context. MCP stays
//! the surface a model reads and writes through.

use std::io::Read as _;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
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
use crate::oauth::Scopes;

/// `text/markdown` is the vault's only content type; the charset is explicit so
/// a client does not have to guess at non-ASCII note bodies.
const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// One JSON object per line: a client can stream it, and one unreadable note
/// costs one line rather than the whole listing.
const NDJSON: &str = "application/x-ndjson";

/// `tar` the command exists on every client, so a bulk transfer is a pipe on
/// both ends rather than anything a caller has to build.
const TAR: &str = "application/x-tar";

/// A pushed tar is buffered whole to be parsed, so it is capped well above a
/// vault of notes and well below memory pressure. The default 2 MiB limit that
/// guards every other route would refuse an ordinary bulk import.
const MAX_TAR_BYTES: usize = 32 * 1024 * 1024;

/// How many notes a transfer may carry before the server asks whether that was
/// meant. Gating on whether a prefix was typed measures syntax; a named
/// directory can hold the whole vault and an unnamed one can hold five notes.
/// Reading is where this matters — a copy cannot be taken back, while a bad
/// write is in git.
const MAX_UNCONFIRMED_NOTES: usize = 50;

/// How many notes an archive carries, so a caller can tell "got nothing" from
/// "got something" without unpacking it.
const NOTE_COUNT: axum::http::HeaderName = axum::http::HeaderName::from_static("note-count");

/// `/api/notes/...`, mounted beside `/mcp` and behind the same bearer guard.
pub(crate) fn routes(server: MdServer) -> Router {
    Router::new()
        .route(
            "/api/notes",
            get(get_collection)
                .post(post_collection)
                .layer(DefaultBodyLimit::max(MAX_TAR_BYTES)),
        )
        .route("/api/notes/{*path}", get(get_note).put(put_note))
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

/// Where a pushed tar lands, and whether it may replace what is there.
#[derive(serde::Deserialize)]
struct PushQuery {
    #[serde(default)]
    to: Option<String>,
    /// Off by default: a bulk push cannot express a per-note precondition, so
    /// replacing has to be said out loud the way `If-Match: *` says it.
    #[serde(default)]
    overwrite: bool,
}

/// Write every note in a tar, reporting one line per entry.
///
/// Unlike an MCP batch (ADR-0007) this is not all-or-nothing: a bulk push is
/// re-runnable, and one rejected note should not undo the rest.
async fn post_collection(
    State(server): State<MdServer>,
    scopes: Option<Extension<Scopes>>,
    Query(query): Query<PushQuery>,
    body: Bytes,
) -> Response {
    let authority = authority(scopes);
    if !authority.write {
        return forbidden("notes:write");
    }

    let _guard = server.lock().write().await;
    let mut archive = tar::Archive::new(body.as_ref());
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("not a tar stream: {error}\n"),
            )
                .into_response();
        }
    };

    let mut report = String::new();
    let mut ops = Vec::new();
    for entry in entries {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.push_str(&line(serde_json::json!({ "error": error.to_string() })));
                continue;
            }
        };
        // Directories are structure, not content. Anything else that is not a
        // regular file — a link, a device node — has no meaning in a vault and
        // is refused rather than followed.
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            report.push_str(&line(serde_json::json!({ "error": "not a regular file" })));
            continue;
        }

        let Ok(name) = entry.path().map(|p| p.to_string_lossy().into_owned()) else {
            report.push_str(&line(
                serde_json::json!({ "error": "unreadable entry path" }),
            ));
            continue;
        };
        // `tar -C dir -cf - .` names every entry `./…`.
        let path = prefixed(query.to.as_deref(), name.trim_start_matches("./"));

        let mut bytes = Vec::new();
        if let Err(error) = entry.read_to_end(&mut bytes) {
            report.push_str(&line(
                serde_json::json!({ "path": path, "error": error.to_string() }),
            ));
            continue;
        }
        if bytes.len() > MAX_WRITE_BYTES {
            report.push_str(&line(serde_json::json!({
                "path": path,
                "error": format!("note is {} bytes, over the {MAX_WRITE_BYTES} limit", bytes.len()),
            })));
            continue;
        }

        if !authority.permits(&path) {
            report.push_str(&line(serde_json::json!({
                "path": path,
                "error": "outside this credential's directory",
            })));
            continue;
        }
        let existed = server.vault().exists(&path).unwrap_or(false);
        if existed && !query.overwrite {
            report.push_str(&line(serde_json::json!({
                "path": path,
                "error": "note exists; pass overwrite=true to replace it",
            })));
            continue;
        }
        // Containment is the vault jail's job (ADR-0006): a `..` or an absolute
        // name in the tar is rejected there, not by string inspection here.
        match server.vault().create_note(&path, &bytes, true) {
            Ok(()) => {
                ops.push(if existed {
                    EventOp::Write { path: path.clone() }
                } else {
                    EventOp::Create { path: path.clone() }
                });
                report.push_str(&line(serde_json::json!({
                    "path": path,
                    "written": true,
                    "replaced": existed,
                })));
            }
            Err(error) => report.push_str(&line(
                serde_json::json!({ "path": path, "error": error.message }),
            )),
        }
    }

    server.emit_event("post_notes", None, &ops);
    server
        .auto_commit(
            "post_notes",
            &ops,
            &serde_json::json!({ "to": query.to, "overwrite": query.overwrite }),
        )
        .await;
    ([(header::CONTENT_TYPE, NDJSON.to_string())], report).into_response()
}

/// Join a pushed entry's name onto the destination prefix.
fn prefixed(to: Option<&str>, name: &str) -> String {
    let joined = match to {
        Some(to) if !to.trim_matches('/').is_empty() => {
            format!("{}/{name}", to.trim_matches('/'))
        }
        _ => name.to_string(),
    };
    nfc(&joined).into_owned()
}

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

/// A subtree as one tar, so a whole vault crosses the wire in one round trip.
fn tar_of(server: &MdServer, paths: &[String]) -> Response {
    let count = paths.len();
    let mut builder = tar::Builder::new(Vec::new());
    for path in paths {
        // A transfer that silently drops a note it could not read would read as
        // a deletion on the far side, so one failure fails the export.
        let note = match server.vault().read_note(path) {
            Ok(note) => note,
            Err(error) => return export_failed(path, &error.message),
        };
        let mut header = tar::Header::new_gnu();
        header.set_size(note.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        if let Err(error) = builder.append_data(&mut header, path, note.as_bytes()) {
            return export_failed(path, &error.to_string());
        }
    }
    match builder.into_inner() {
        // A tar of nothing is 1024 bytes of padding and looks exactly like a tar
        // of something, so the count is stated rather than left to be parsed.
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, TAR.to_string()),
                (NOTE_COUNT, count.to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => export_failed("", &error.to_string()),
    }
}

fn export_failed(path: &str, why: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("export aborted at {path:?}: {why}\n"),
    )
        .into_response()
}

/// What the collection endpoint should answer with, and over what subtree.
#[derive(serde::Deserialize)]
struct CollectionQuery {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    format: Option<String>,
    /// Acknowledge a transfer large enough that the server asks first.
    #[serde(default)]
    confirm: bool,
}

async fn get_collection(
    State(server): State<MdServer>,
    scopes: Option<Extension<Scopes>>,
    Query(query): Query<CollectionQuery>,
) -> Response {
    let authority = authority(scopes);
    if !authority.read {
        return forbidden("notes:read");
    }
    // A credential confined to one subtree narrows an unqualified request to
    // what it can see, rather than refusing it: it is already narrowed.
    let asked = query
        .prefix
        .as_deref()
        .filter(|prefix| !prefix.trim_matches('/').is_empty());
    if let Some(asked) = asked
        && !authority.permits(asked)
    {
        return outside_grant(asked);
    }
    let prefix = asked
        .map(str::to_owned)
        .or_else(|| authority.prefix.clone());

    // One guard over listing and reading, so a transfer is a consistent
    // snapshot rather than a walk through a moving vault.
    let _guard = server.lock().read().await;
    let entries = match entries_under(&server, prefix.as_deref()) {
        Ok(entries) => entries,
        Err(refusal) => return refusal.into_response(),
    };

    match query.format.as_deref() {
        // The index moves no content, and is how a caller learns the size it is
        // being asked to confirm. Gating it would discourage the one step worth
        // encouraging.
        Some("index") => index(&server, &entries),
        None | Some("tar") => {
            if entries.len() > MAX_UNCONFIRMED_NOTES && !query.confirm {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "this would transfer {} notes, over the {MAX_UNCONFIRMED_NOTES} that go \
                         without asking; pass confirm=true to take them, or narrow it with \
                         prefix=<directory>\n",
                        entries.len()
                    ),
                )
                    .into_response();
            }
            tar_of(&server, &entries)
        }
        Some(other) => (
            StatusCode::BAD_REQUEST,
            format!("unsupported format {other:?}; use tar or index\n"),
        )
            .into_response(),
    }
}

/// The note paths a request covers, or why there are none to speak of. An empty
/// archive and a mistyped prefix look identical to a caller, and one of them
/// means the command they ran did nothing.
///
/// A prefix may name one note rather than a directory: the narrowest useful
/// grant is a single note, and a credential confined to one still has a
/// collection — it is that note.
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
    if server.vault().is_dir(prefix).unwrap_or(false) {
        return listing(prefix);
    }
    if !Vault::is_internal_path(prefix) && server.vault().exists(prefix).unwrap_or(false) {
        return Ok(vec![prefix.to_string()]);
    }
    Err((
        StatusCode::NOT_FOUND,
        format!("nothing at {prefix:?} in this vault\n"),
    ))
}

/// Every note's path, entity tag and size, without its content — so a caller
/// can work out what changed and fetch only that. The tags are the same values
/// the note endpoint serves, and so can be replayed as `If-Match`.
fn index(server: &MdServer, paths: &[String]) -> Response {
    let mut body = String::new();
    for path in paths {
        // A note that cannot be read costs its own line, never the listing: a
        // silently dropped path reads to a syncing client as a deletion.
        let line = match server.vault().read_note(path) {
            Ok(note) => serde_json::json!({
                "path": path,
                "etag": entity_tag(note.as_bytes()),
                "size": note.len(),
            }),
            Err(error) => serde_json::json!({
                "path": path,
                "error": error.message,
            }),
        };
        body.push_str(&line.to_string());
        body.push('\n');
    }
    ([(header::CONTENT_TYPE, NDJSON.to_string())], body).into_response()
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
    let authority = authority(scopes);
    if !authority.write {
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
