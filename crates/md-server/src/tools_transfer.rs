//! The tool that opens the byte-level note API to a client with a shell
//! ([ADR-0028](../../../docs/adr/0028-byte-level-note-api.md)).
//!
//! It answers with a credential and with commands whose values are already
//! filled in, because the aim is not that a model learn an API but that it run
//! a line. The consent was given when the caller's own bearer was authorised,
//! so nothing here asks a second time.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use md_core::text::nfc;

use crate::MdServer;
use crate::oauth::{
    self, OAuthState, Scopes, TRANSFER_CODE_TTL_SECS, TRANSFER_TOKEN_TTL_SECS, percent_encode_path,
};

/// Where the recipe parks the collected token. Never interpolated into a
/// command, only read back through `$(cat …)`, so it stays out of shell history
/// and out of `ps`. Named by what it may do, so collecting a writing grant
/// while a reading one is in use does not silently replace it.
fn token_file(write: bool) -> &'static str {
    if write {
        "/tmp/md-token-rw"
    } else {
        "/tmp/md-token-ro"
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ProvisionTransferRequest {
    /// Grant `notes:write` as well as `notes:read`. Off by default: a token
    /// that only reads cannot damage the vault if it leaks.
    #[serde(default)]
    pub write: bool,
    /// Confine the credential to one directory. Ask for the narrowest one the
    /// task needs: a token that cannot reach the rest of the vault cannot take
    /// or damage it by mistake. Omit only when the task really is vault-wide.
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ProvisionTransferResponse {
    /// Base URL of the transfer API.
    pub base: String,
    /// Where to trade the ticket for the token.
    pub redeem: String,
    /// A one-time ticket, not a credential: it is spent by the first redemption
    /// and is worthless afterwards, so what it leaves in this conversation is
    /// dead text.
    pub code: String,
    pub code_expires_in_seconds: u64,
    /// The whole grant: a transfer token is not renewable. If a job outlives
    /// it, call this tool again for a fresh ticket.
    pub token_expires_in_seconds: u64,
    /// Where the recipe leaves the collected token.
    pub token_file: String,
    pub scopes: Vec<String>,
    /// Runnable as printed, in a shell that has `curl`.
    pub recipe: Vec<String>,
}

#[tool_router(router = transfer_router, vis = "pub(crate)")]
impl MdServer {
    /// Mint a short-lived, scope-reduced credential for the transfer API.
    #[tool(
        description = "Open a byte-level HTTP surface for working on note bytes outside model context, and return a one-time ticket with ready-to-run curl commands. The note tools are how work in this vault is normally done; reach for this when a note is too large to pull through context, or a job is repetitive enough that a shell script plainly beats them — rewriting a large note with sed, processing many notes in a loop. It serves an index (path, etag, size per note) and one-note GET/PUT with If-Match; bulk work is those commands in a loop. Useless without shell access. Pass write:true to write back, and prefix to confine the grant to one directory or one note. Deleting and moving are not part of it: use delete_notes and move_notes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn provision_transfer(
        &self,
        Parameters(req): Parameters<ProvisionTransferRequest>,
        Extension(parts): Extension<Parts>,
    ) -> Result<Json<ProvisionTransferResponse>, ErrorData> {
        // The authorization server is put here by the bearer guard, so its
        // absence means the deployment has no token configured and there is no
        // authority to delegate.
        let Some(oauth) = parts.extensions.get::<std::sync::Arc<OAuthState>>() else {
            return Err(ErrorData::invalid_request(
                "this server runs unauthenticated; the transfer API needs no token here",
                None,
            ));
        };

        let scopes = Scopes {
            read: true,
            write: req.write,
            // `mint` stamps this; naming it here keeps the struct exhaustive.
            delegated: true,
            // Composed here, once: the paths a grant is compared against are
            // composed, so a decomposed spelling would refuse its own notes.
            prefix: req.prefix.as_deref().map(|p| nfc(p).into_owned()),
        };
        let example = example_note(self, req.prefix.as_deref()).await;
        let code = oauth.issue_transfer_code(scopes.clone());
        let root = oauth::base_url(&parts.headers);
        let base = format!("{root}/api/notes");
        let redeem = format!("{root}/transfer/redeem");

        Ok(Json(ProvisionTransferResponse {
            recipe: recipe(&base, &redeem, &code, req.write, example.as_deref()),
            base,
            redeem,
            code,
            code_expires_in_seconds: TRANSFER_CODE_TTL_SECS,
            token_expires_in_seconds: TRANSFER_TOKEN_TTL_SECS,
            token_file: token_file(req.write).to_string(),
            scopes: scope_names(&scopes),
        }))
    }
}

/// The note the examples name, in a value this vault actually holds. A recipe
/// is a contract to be runnable as printed, and a placeholder filename 404s.
/// A grant confined to one note names that note; anything wider names the
/// first note it reaches, or nothing in an empty scope.
async fn example_note(server: &MdServer, confined_to: Option<&str>) -> Option<String> {
    let _guard = server.lock().read().await;
    if let Some(scope) = confined_to
        && !server.vault().is_dir(scope).unwrap_or(false)
    {
        return Some(scope.to_string());
    }
    server
        .vault()
        .list_entries(confined_to.unwrap_or(""), true, None, false)
        .ok()
        .and_then(|entries| entries.into_iter().next())
        .map(|entry| entry.path)
}

fn scope_names(scopes: &Scopes) -> Vec<String> {
    let mut names = Vec::new();
    if scopes.read {
        names.push("notes:read".to_string());
    }
    if scopes.write {
        names.push("notes:write".to_string());
    }
    if let Some(prefix) = &scopes.prefix {
        names.push(format!("under:{prefix}"));
    }
    names
}

/// Commands with the values already substituted: a model should run a line,
/// not assemble one.
fn recipe(base: &str, redeem: &str, code: &str, write: bool, note: Option<&str>) -> Vec<String> {
    let token_file = token_file(write);
    // Read back from the file rather than interpolated: the token then never
    // reaches a command line, shell history, or this conversation.
    let auth = format!("-H \"authorization: Bearer $(cat {token_file})\"");

    let mut recipe = vec![
        format!(
            "# 1. run this first: trade the one-time ticket for the token (single use)\ncurl -sSf -X POST -d \"code={code}\" \"{redeem}\" -o {token_file}"
        ),
        format!(
            "# 2. the index: every note this grant reaches -- path, etag, size -- one JSON object per line, no content.\n#    etag is blake3 of the note's exact bytes, quoted; recompute it locally to find what changed\ncurl -sS --fail-with-body {auth} \"{base}\""
        ),
    ];
    if let Some(note) = note {
        recipe.push(format!(
            "# one note, verbatim, to a file -- work on it with your tools instead of reading it into context\ncurl -sS --fail-with-body {auth} -o note.md \"{base}/{}\"",
            percent_encode_path(note)
        ));
        if write {
            recipe.push(format!(
                "# put it back, only if it did not change since you read it (etag from the index or the GET).\n#    201 created, 204 replaced -- the reply carries the new etag, so a second edit needs no fresh GET -- 412 it changed underneath you, 428 the if-match is missing\ncurl -sS -D - {auth} -X PUT -H \"if-match: <etag>\" --data-binary @note.md \"{base}/{}\"",
                percent_encode_path(note)
            ));
        }
    }
    recipe.push(
        "# many notes are the same two commands in a loop over the index paths. Deleting and \
         moving are not part of this API: use the delete_notes and move_notes tools"
            .to_string(),
    );
    recipe
}

#[cfg(test)]
mod tests {
    use super::recipe;

    fn lines(write: bool, note: Option<&str>) -> Vec<String> {
        recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "TICKET",
            write,
            note,
        )
    }

    #[test]
    fn a_name_with_a_space_never_reaches_the_command_line_raw() {
        let joined = lines(true, Some("00-inbox/Marp 프레젠테이션 테스트.md")).join("\n");

        assert!(
            !joined.contains("Marp 프레젠테이션"),
            "curl rejects a URL with a raw space before it ever sends it: {joined}"
        );
        assert!(joined.contains("Marp%20"), "{joined}");
    }

    #[test]
    fn the_recipe_needs_nothing_but_curl() {
        let joined = lines(true, Some("00-inbox/a real note.md")).join("\n");
        assert!(
            !joined.contains("tar "),
            "there is no archive endpoint to feed; a tar anywhere in here is a \
             leftover of the bulk surface this API no longer has: {joined}"
        );
    }

    #[test]
    fn a_reading_grant_and_a_writing_one_do_not_share_a_token_file() {
        let reading = lines(false, None).join("\n");
        let writing = lines(true, None).join("\n");
        let file = |recipe: &str| {
            recipe
                .split("/tmp/md-token")
                .nth(1)
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
                .expect("a token file")
        };

        assert_ne!(
            file(&reading),
            file(&writing),
            "collecting a writing grant while a reading one is in use would \
             replace it without saying so"
        );
    }

    #[test]
    fn the_examples_name_things_this_vault_actually_holds() {
        let joined = lines(true, Some("00-inbox/a real note.md")).join("\n");

        assert!(
            joined.contains("00-inbox/a%20real%20note.md"),
            "the one-note example names a note that exists, escaped so the \
             command can carry it: {joined}"
        );
        assert!(
            !joined.contains("/note.md\""),
            "no placeholder filename may survive: {joined}"
        );
    }

    #[test]
    fn knowing_what_is_there_comes_before_fetching_any_of_it() {
        let lines = lines(true, Some("00-inbox/a real note.md"));
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle));

        assert!(
            at("\"https://host/api/notes\"") < at("-o note.md"),
            "the index is how a caller learns paths and tags; fetching first \
             is guessing: {lines:?}"
        );
    }

    #[test]
    fn a_reading_grant_is_offered_no_way_to_write() {
        let joined = lines(false, Some("00-inbox/a real note.md")).join("\n");
        assert!(
            !joined.contains("-X PUT"),
            "a command the credential cannot run teaches a caller to fail: {joined}"
        );
    }

    #[test]
    fn a_replacement_carries_its_precondition() {
        let joined = lines(true, Some("00-inbox/a real note.md")).join("\n");
        assert!(
            joined.contains("if-match"),
            "an unconditional replace is the lost-update hole this API exists \
             to close: {joined}"
        );
    }

    #[test]
    fn an_empty_scope_still_teaches_the_loop() {
        let lines = lines(true, None);
        let joined = lines.join("\n");
        assert!(
            !joined.contains("note.md"),
            "with no note to name there is no note line to print: {joined}"
        );
        assert!(
            joined.contains("loop over the index paths"),
            "bulk work has to be discoverable from the recipe alone: {joined}"
        );
    }

    #[test]
    fn the_index_example_does_not_promise_note_tags() {
        let joined = lines(false, None).join("\n");
        assert!(
            !joined.contains("tags"),
            "in a notes vault `tags` means frontmatter tags, which this does not \
             return: {joined}"
        );
    }
}
