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
            recipe: recipe(&base, &token, req.write),
            base,
            token,
            expires_in_seconds: TRANSFER_TTL_SECS,
            scopes: scope_names(scopes),
        }))
    }
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
fn recipe(base: &str, token: &str, write: bool) -> Vec<String> {
    let auth = format!("-H \"authorization: Bearer {token}\"");
    let mut recipe = vec![
        format!(
            "# pull a subtree into ./vault\ncurl -sS {auth} \"{base}?prefix=inbox\" | tar -xf - -C ./vault"
        ),
        format!(
            "# what exists, with tags and sizes, without the content\ncurl -sS {auth} \"{base}?format=index\""
        ),
        format!("# one note\ncurl -sS {auth} -o note.md \"{base}/inbox/note.md\""),
    ];
    if write {
        recipe.push(format!(
            "# push a directory (add &overwrite=true to replace existing notes)\ntar -C ./vault -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}?to=inbox\""
        ));
        recipe.push(format!(
            "# replace one note, only if it has not changed since you read it\ncurl -sS {auth} -X PUT -H \"if-match: <etag>\" --data-binary @note.md \"{base}/inbox/note.md\""
        ));
    }
    recipe
}
