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

/// Where the recipe parks the collected token. Never interpolated into a
/// command, only read back through `$(cat …)`, so it stays out of shell history
/// and out of `ps`.
const TOKEN_FILE: &str = "/tmp/md-token";

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
    /// Where to trade a live token for a fresh one, before it lapses.
    pub renew: String,
    /// A one-time ticket, not a credential: it is spent by the first redemption
    /// and is worthless afterwards, so what it leaves in this conversation is
    /// dead text.
    pub code: String,
    pub code_expires_in_seconds: u64,
    pub token_expires_in_seconds: u64,
    /// A renewal chain cannot pass this, counted from the first token.
    pub token_max_lifetime_seconds: u64,
    /// Where the recipe leaves the collected token.
    pub token_file: String,
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
            prefix: req.prefix.clone(),
        };
        let code = oauth.issue_transfer_code(scopes.clone());
        let root = origin(&parts);
        let base = format!("{root}/api/notes");
        let redeem = format!("{root}/transfer/redeem");
        let renew = format!("{root}/transfer/renew");

        Ok(Json(ProvisionTransferResponse {
            recipe: recipe(
                &base,
                &redeem,
                &renew,
                &code,
                req.write,
                // A confined credential makes its own directory the example;
                // anything else in the recipe would be refused as printed.
                match &req.prefix {
                    Some(prefix) => Some(prefix.clone()),
                    None => example_dir(self).await,
                }
                .as_deref(),
            ),
            base,
            redeem,
            renew,
            code,
            code_expires_in_seconds: OAuthState::transfer_code_ttl_secs(),
            token_expires_in_seconds: OAuthState::transfer_token_ttl_secs(),
            token_max_lifetime_seconds: OAuthState::transfer_max_lifetime_secs(),
            token_file: TOKEN_FILE.to_string(),
            scopes: scope_names(&scopes),
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

/// The public origin this request arrived on. The tunnel terminates TLS, so the
/// forwarded `Host` is the only thing that names the server a client can reach.
fn origin(parts: &Parts) -> String {
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}")
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
fn recipe(
    base: &str,
    redeem: &str,
    renew: &str,
    code: &str,
    write: bool,
    example_dir: Option<&str>,
) -> Vec<String> {
    // Read back from the file rather than interpolated: the token then never
    // reaches a command line, shell history, or this conversation.
    let auth = format!("-H \"authorization: Bearer $(cat {TOKEN_FILE})\"");
    // A directory this vault does not have would unpack nothing on a pull and
    // push into the wrong place, so the narrowing examples appear only when
    // there is a real one to name.
    let note = example_dir.map_or_else(|| "note.md".to_string(), |dir| format!("{dir}/note.md"));

    let mut recipe = vec![
        format!(
            "# 1. run this first: trade the one-time ticket for the token (single use)\ncurl -sSf -X POST -d \"code={code}\" \"{redeem}\" -o {TOKEN_FILE}"
        ),
        format!(
            "# 2. see what is there and how much of it, without moving any content\ncurl -sS {auth} \"{base}?format=index\""
        ),
    ];
    if let Some(dir) = example_dir {
        recipe.push(format!(
            "# pull one directory into ./vault (the note-count response header says how many arrived)\ncurl -sS {auth} \"{base}?prefix={dir}\" | tar -xf - -C ./vault"
        ));
    }
    recipe.push(format!(
        "# the whole vault, when you do mean all of it\ncurl -sS {auth} \"{base}?all=true\" | tar -xf - -C ./vault"
    ));
    recipe.push(format!(
        "# one note\ncurl -sS {auth} -o note.md \"{base}/{note}\""
    ));
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
    recipe.push(format!(
        "# renew before it lapses; writing to a temp file first so a failed renewal cannot destroy a live token\ncurl -sSf -X POST {auth} \"{renew}\" -o {TOKEN_FILE}.new && mv {TOKEN_FILE}.new {TOKEN_FILE}"
    ));
    recipe
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn the_examples_name_a_directory_this_vault_actually_has() {
        let lines = recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "https://host/transfer/renew",
            "TICKET",
            true,
            Some("00-inbox"),
        );
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
    fn the_recipe_reaches_for_the_whole_vault_last_and_says_so() {
        let lines = recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "https://host/transfer/renew",
            "TICKET",
            false,
            Some("00-inbox"),
        );
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle));

        assert!(
            at("format=index") < at("tar -xf"),
            "knowing the size comes before deciding to move it"
        );
        assert!(
            at("prefix=00-inbox") < at("all=true"),
            "the narrowed pull is the one to reach for first: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .filter(|l| l.contains("tar -xf"))
                .all(|l| l.contains("prefix=") || l.contains("all=true")),
            "a pull with nothing narrowing it and nothing asking for everything \
             is refused by the server, so it must not appear here: {lines:?}"
        );
    }

    #[test]
    fn an_empty_vault_gets_examples_without_a_prefix() {
        let joined = recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "https://host/transfer/renew",
            "TICKET",
            true,
            None,
        )
        .join("\n");
        assert!(
            !joined.contains("prefix=") && !joined.contains("to="),
            "with no directory to name, the examples take the whole vault: {joined}"
        );
    }

    #[test]
    fn the_index_example_does_not_promise_note_tags() {
        let joined = recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "https://host/transfer/renew",
            "TICKET",
            false,
            None,
        )
        .join("\n");
        assert!(
            !joined.contains("tags"),
            "in a notes vault `tags` means frontmatter tags, which this does not \
             return: {joined}"
        );
    }
}
