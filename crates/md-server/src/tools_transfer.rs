//! The tool that opens the byte-level transfer API to a client with a shell
//! ([ADR-0028](../../../docs/adr/0028-bulk-transfer-http-api.md)).
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
    self, OAuthState, Scopes, TRANSFER_CODE_TTL_SECS, TRANSFER_MAX_LIFETIME_SECS,
    TRANSFER_TOKEN_TTL_SECS, percent_encode, percent_encode_path,
};

/// What the examples are written around, in values this vault actually holds.
/// A recipe is a contract to be runnable as printed: a placeholder filename
/// 404s, and a directory that is the whole grant is not "the whole vault".
enum Example {
    /// Unconfined, with no directory worth naming.
    Vault { note: Option<String> },
    Directory {
        path: String,
        note: Option<String>,
        /// The grant reaches nothing outside this directory, so it has no
        /// wider pull to offer and nothing to narrow from.
        confined: bool,
    },
    /// The grant is exactly one note.
    Note(String),
}

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
        description = "Open the bulk-transfer HTTP API and return a one-time ticket with ready-to-run curl commands. Use this instead of reading or writing notes one at a time when you need to move many notes, or to process notes with scripts rather than pulling their content into context. Requires shell access to be of any use. Pass write:true to push notes back, and prefix to confine the grant to one directory or note. It reads, creates and replaces notes; deleting is not part of it, so use delete_notes for that.",
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
        let example = example_scope(self, req.prefix.as_deref()).await;
        let code = oauth.issue_transfer_code(scopes.clone());
        let root = oauth::base_url(&parts.headers);
        let base = format!("{root}/api/notes");
        let redeem = format!("{root}/transfer/redeem");
        let renew = format!("{root}/transfer/renew");

        Ok(Json(ProvisionTransferResponse {
            recipe: recipe(&base, &redeem, &renew, &code, req.write, &example),
            base,
            redeem,
            renew,
            code,
            code_expires_in_seconds: TRANSFER_CODE_TTL_SECS,
            token_expires_in_seconds: TRANSFER_TOKEN_TTL_SECS,
            token_max_lifetime_seconds: TRANSFER_MAX_LIFETIME_SECS,
            token_file: TOKEN_FILE.to_string(),
            scopes: scope_names(&scopes),
        }))
    }
}

/// The values to write the examples around. A confined grant describes its own
/// scope; otherwise a directory this vault has, since naming one it does not
/// unpacks nothing on a pull and creates a stray one on a push, reporting
/// success either way.
async fn example_scope(server: &MdServer, confined_to: Option<&str>) -> Example {
    let _guard = server.lock().read().await;
    let first_note = |directory: &str| {
        server
            .vault()
            .list_entries(directory, true, None, false)
            .ok()
            .and_then(|entries| entries.into_iter().next())
            .map(|entry| entry.path)
    };

    if let Some(scope) = confined_to {
        return if server.vault().is_dir(scope).unwrap_or(false) {
            Example::Directory {
                note: first_note(scope),
                path: scope.to_string(),
                confined: true,
            }
        } else {
            Example::Note(scope.to_string())
        };
    }
    match server
        .vault()
        .list_entries("", false, None, true)
        .ok()
        .and_then(|entries| entries.into_iter().find(|entry| entry.is_dir))
        .map(|entry| entry.path.trim_end_matches('/').to_string())
    {
        Some(path) => Example::Directory {
            note: first_note(&path),
            path,
            confined: false,
        },
        None => Example::Vault {
            note: first_note(""),
        },
    }
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
    example: &Example,
) -> Vec<String> {
    // Read back from the file rather than interpolated: the token then never
    // reaches a command line, shell history, or this conversation.
    let auth = format!("-H \"authorization: Bearer $(cat {TOKEN_FILE})\"");
    let (directory, note, confined) = match example {
        Example::Vault { note } => (None, note.as_deref(), false),
        Example::Directory {
            path,
            note,
            confined,
        } => (Some(path.as_str()), note.as_deref(), *confined),
        Example::Note(note) => (None, Some(note.as_str()), true),
    };

    let mut recipe = vec![
        format!(
            "# 1. run this first: trade the one-time ticket for the token (single use)\ncurl -sSf -X POST -d \"code={code}\" \"{redeem}\" -o {TOKEN_FILE}"
        ),
        format!(
            "# 2. see what is there and how much of it, without moving any content.\n#    etag is blake3 of the note's exact bytes, quoted -- recompute it locally to find what changed\ncurl -sS {auth} \"{base}?format=index\""
        ),
    ];
    if let Some(directory) = directory {
        recipe.push(format!(
            "# pull this directory into ./vault (the note-count response header says how many arrived)\ncurl -sS {auth} \"{base}?prefix={}\" | tar -xf - -C ./vault",
            percent_encode(directory)
        ));
    }
    // A confined grant has no wider pull to offer: everything it can reach is
    // already what the line above fetched.
    if !confined {
        recipe.push(format!(
            "# the whole vault (past a few dozen notes the server asks you to confirm, and says how many)\ncurl -sS {auth} \"{base}\" | tar -xf - -C ./vault"
        ));
    }
    if let Some(note) = note {
        recipe.push(format!(
            "# one note\ncurl -sS {auth} -o note.md \"{base}/{}\"",
            percent_encode_path(note)
        ));
    }

    if write {
        if directory.is_some() || !confined {
            let destination =
                directory.map_or_else(String::new, |dir| format!("?to={}", percent_encode(dir)));
            let separator = if destination.is_empty() { "?" } else { "&" };
            let and = if destination.is_empty() { "?" } else { "&" };
            recipe.push(format!(
                "# check the push first -- `tar -cf - .` sends whatever the directory holds, not only what you meant\ntar -C ./vault -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}{destination}{and}dry_run=true\""
            ));
            recipe.push(format!(
                "# then make it (add {separator}overwrite=true to replace existing notes; 207 means some entries were refused, and the lines above say which)\ntar -C ./vault -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}{destination}\" -w \"\\nHTTP %{{http_code}}\\n\""
            ));
        }
        if let Some(note) = note {
            recipe.push(format!(
                "# replace that note, only if it has not changed since you read it\ncurl -sS {auth} -X PUT -H \"if-match: <etag>\" --data-binary @note.md \"{base}/{}\"",
            percent_encode_path(note)
            ));
        }
    }
    recipe.push(format!(
        "# renew before it lapses; into a temp file first because curl -o truncates before it writes. The old token keeps working for a minute, so a lost replacement can be asked for again\ncurl -sSf -X POST {auth} \"{renew}\" -o {TOKEN_FILE}.new && mv {TOKEN_FILE}.new {TOKEN_FILE}"
    ));
    recipe
}

#[cfg(test)]
mod tests {
    use super::{Example, recipe};

    #[test]
    fn a_name_with_a_space_never_reaches_the_command_line_raw() {
        let joined = lines(
            true,
            &Example::Directory {
                path: "00-inbox".to_string(),
                note: Some("00-inbox/Marp 프레젠테이션 테스트.md".to_string()),
                confined: true,
            },
        )
        .join("\n");

        assert!(
            !joined.contains("Marp 프레젠테이션"),
            "curl rejects a URL with a raw space before it ever sends it: {joined}"
        );
        assert!(joined.contains("Marp%20"), "{joined}");
    }

    fn lines(write: bool, example: &Example) -> Vec<String> {
        recipe(
            "https://host/api/notes",
            "https://host/transfer/redeem",
            "https://host/transfer/renew",
            "TICKET",
            write,
            example,
        )
    }

    fn example_directory(confined: bool) -> Example {
        Example::Directory {
            path: "00-inbox".to_string(),
            note: Some("00-inbox/a real note.md".to_string()),
            confined,
        }
    }

    #[test]
    fn the_examples_name_things_this_vault_actually_holds() {
        let joined = lines(true, &example_directory(false)).join("\n");

        assert!(joined.contains("prefix=00-inbox"));
        assert!(joined.contains("to=00-inbox"));
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
    fn the_recipe_reaches_for_the_narrow_thing_first() {
        let lines = lines(false, &example_directory(false));
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle));

        assert!(
            at("format=index") < at("tar -xf"),
            "knowing the size comes before deciding to move it"
        );
        assert!(
            at("prefix=00-inbox") < lines.iter().rposition(|l| l.contains("tar -xf")),
            "the narrowed pull is the one to reach for first: {lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.contains("confirm=")),
            "the server names the flag only when a transfer is large enough to \
             need it; printing it here would make it a reflex: {lines:?}"
        );
    }

    #[test]
    fn a_directory_confined_grant_offers_no_wider_pull() {
        let lines = lines(false, &example_directory(true));

        assert_eq!(
            lines.iter().filter(|l| l.contains("tar -xf")).count(),
            1,
            "everything this grant can reach is the one directory; a second \
             line calling that the whole vault says something untrue: {lines:?}"
        );
    }

    #[test]
    fn a_grant_that_is_one_note_names_that_note_and_nothing_else() {
        let joined = lines(true, &Example::Note("00-inbox/README.md".to_string())).join("\n");

        assert!(
            joined.contains("\"https://host/api/notes/00-inbox/README.md\""),
            "the one-note example is the note itself: {joined}"
        );
        assert!(
            !joined.contains("README.md/note.md"),
            "treating the note as a directory invents a path that cannot \
             exist: {joined}"
        );
        assert!(
            !joined.contains("prefix=") && !joined.contains("to=") && !joined.contains("tar -xf"),
            "a grant that is one note has no subtree to narrow to or push \
             into: {joined}"
        );
    }

    #[test]
    fn an_empty_vault_gets_examples_without_a_prefix() {
        let joined = lines(true, &Example::Vault { note: None }).join("\n");
        assert!(
            !joined.contains("prefix=") && !joined.contains("to="),
            "with nothing to name, the examples take the whole vault: {joined}"
        );
    }

    #[test]
    fn the_index_example_does_not_promise_note_tags() {
        let joined = lines(false, &Example::Vault { note: None }).join("\n");
        assert!(
            !joined.contains("tags"),
            "in a notes vault `tags` means frontmatter tags, which this does not \
             return: {joined}"
        );
    }
}
