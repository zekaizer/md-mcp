//! The tool that opens the byte-level transfer API to a client with a shell
//! ([ADR-0028](../../../docs/adr/0028-bulk-transfer-http-api.md)).
//!
//! It answers with a credential and with commands whose values are already
//! filled in, because the aim is not that a model learn an API but that it run
//! a line. The consent was given when the caller's own bearer was authorised,
//! so nothing here asks a second time.

use axum::http::header;
use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::MdServer;
use crate::oauth::{OAuthState, Scopes};

/// Long enough for a bulk import, short enough that the copy left in a
/// conversation transcript is stale before anyone could read it back.
const TRANSFER_TTL_SECS: u64 = 10 * 60;

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ProvisionTransferRequest {
    /// Grant `notes:write` as well as `notes:read`. Off by default: a token
    /// that only reads cannot damage the vault if it leaks.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ProvisionTransferResponse {
    /// Base URL of the transfer API.
    pub base: String,
    pub token: String,
    pub expires_in_seconds: u64,
    pub scopes: Vec<String>,
    /// Runnable as printed, in a shell that has `curl` and `tar`.
    pub recipe: Vec<String>,
}

#[tool_router(router = transfer_router, vis = "pub(crate)")]
impl MdServer {
    /// Mint a short-lived, scope-reduced credential for the transfer API.
    #[tool(
        description = "Open the bulk-transfer HTTP API and return a short-lived token with ready-to-run curl commands. Use this instead of reading or writing notes one at a time when you need to move many notes, or to process notes with scripts rather than pulling their content into context. Requires shell access to be of any use. Pass write:true to push notes back.",
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
        };
        let token = oauth.mint(scopes, TRANSFER_TTL_SECS);
        let base = base_url(&parts);

        Ok(Json(ProvisionTransferResponse {
            recipe: recipe(&base, &token, req.write, example_dir(self).await.as_deref()),
            base,
            token,
            expires_in_seconds: TRANSFER_TTL_SECS,
            scopes: scope_names(scopes),
        }))
    }
}

/// A directory this vault actually has, to fill the examples with. A recipe
/// naming a directory that is not there unpacks nothing on a pull and creates a
/// stray one on a push, and reports success either way.
async fn example_dir(server: &MdServer) -> Option<String> {
    let _guard = server.lock().read().await;
    server
        .vault()
        .list_entries("", false, None, true)
        .ok()?
        .into_iter()
        .find(|entry| entry.is_dir)
        .map(|entry| entry.path.trim_end_matches('/').to_string())
}

/// The public base this request arrived on. The tunnel terminates TLS, so the
/// forwarded `Host` is the only thing that names the server a client can reach.
fn base_url(parts: &Parts) -> String {
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}/api/notes")
}

fn scope_names(scopes: Scopes) -> Vec<String> {
    let mut names = Vec::new();
    if scopes.read {
        names.push("notes:read".to_string());
    }
    if scopes.write {
        names.push("notes:write".to_string());
    }
    names
}

/// Commands with the values already substituted: a model should run a line,
/// not assemble one.
fn recipe(base: &str, token: &str, write: bool, example_dir: Option<&str>) -> Vec<String> {
    let auth = format!("-H \"authorization: Bearer {token}\"");
    // With no directory to name, every example takes the whole vault rather than
    // inventing a path that would unpack nothing and push into the wrong place.
    let scope = example_dir.map_or_else(String::new, |dir| format!("?prefix={dir}"));
    let note = example_dir.map_or_else(|| "note.md".to_string(), |dir| format!("{dir}/note.md"));

    let mut recipe = vec![
        format!(
            "# pull into ./vault (note-count in the response headers says how many arrived)\ncurl -sS {auth} \"{base}{scope}\" | tar -xf - -C ./vault"
        ),
        format!(
            "# what exists, with each note's hash and size, without the content\ncurl -sS {auth} \"{base}?format=index\""
        ),
        format!("# one note\ncurl -sS {auth} -o note.md \"{base}/{note}\""),
    ];
    if write {
        let destination = example_dir.map_or_else(String::new, |dir| format!("?to={dir}"));
        let separator = if destination.is_empty() { "?" } else { "&" };
        recipe.push(format!(
            "# push a directory (add {separator}overwrite=true to replace existing notes)\ntar -C ./vault -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}{destination}\""
        ));
        recipe.push(format!(
            "# replace one note, only if it has not changed since you read it\ncurl -sS {auth} -X PUT -H \"if-match: <etag>\" --data-binary @note.md \"{base}/{note}\""
        ));
    }
    recipe
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn the_examples_name_a_directory_this_vault_actually_has() {
        let lines = recipe("https://host/api/notes", "T0KEN", true, Some("00-inbox"));
        let joined = lines.join("\n");
        assert!(
            joined.contains("prefix=00-inbox"),
            "the pull example must be runnable as printed: {joined}"
        );
        assert!(
            joined.contains("to=00-inbox"),
            "a push to a directory that is not there quietly creates a stray one: {joined}"
        );
        assert!(
            !joined.contains("/inbox/") && !joined.contains("=inbox"),
            "no invented directory may survive in the recipe: {joined}"
        );
    }

    #[test]
    fn an_empty_vault_gets_examples_without_a_prefix() {
        let joined = recipe("https://host/api/notes", "T0KEN", true, None).join("\n");
        assert!(
            !joined.contains("prefix=") && !joined.contains("to="),
            "with no directory to name, the examples take the whole vault: {joined}"
        );
    }

    #[test]
    fn the_index_example_does_not_promise_note_tags() {
        let joined = recipe("https://host/api/notes", "T0KEN", false, None).join("\n");
        assert!(
            !joined.contains("tags"),
            "in a notes vault `tags` means frontmatter tags, which this does not \
             return: {joined}"
        );
    }
}
