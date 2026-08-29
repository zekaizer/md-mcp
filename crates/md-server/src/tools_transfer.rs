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
    self, OAuthState, Scopes, TRANSFER_CODE_TTL_SECS, TRANSFER_TOKEN_TTL_SECS, percent_encode,
    percent_encode_path,
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
/// and out of `ps`. Named by what it may do, so collecting a writing grant
/// while a reading one is in use does not silently replace it.
fn token_file(write: bool) -> &'static str {
    if write {
        "/tmp/md-token-rw"
    } else {
        "/tmp/md-token-ro"
    }
}

/// Where a pull unpacks to, and where a push is staged from. Deliberately not
/// the same directory: a recipe run top to bottom would otherwise push back
/// everything it had just pulled.
const PULL_DIR: &str = "./vault";
const PUSH_DIR: &str = "./outbox";

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
    /// Runnable as printed, in a shell that has `curl` and `tar`.
    pub recipe: Vec<String>,
}

#[tool_router(router = transfer_router, vis = "pub(crate)")]
impl MdServer {
    /// Mint a short-lived, scope-reduced credential for the transfer API.
    #[tool(
        description = "Open an HTTP surface for bulk note transfer and return a one-time ticket with ready-to-run curl commands. The note tools are how work in this vault is normally done; reach for this only when the job is bulky or repetitive enough that a shell would plainly beat them — importing a directory, rewriting hundreds of notes with a script, or moving content that has no reason to pass through context. Useless without shell access. Pass write:true to push notes back, and prefix to confine the grant to one directory or one note. It reads, creates and replaces notes; deleting is not part of it, so use delete_notes for that.",
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

        Ok(Json(ProvisionTransferResponse {
            recipe: recipe(&base, &redeem, &code, req.write, &example),
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
    code: &str,
    write: bool,
    example: &Example,
) -> Vec<String> {
    let token_file = token_file(write);
    // Read back from the file rather than interpolated: the token then never
    // reaches a command line, shell history, or this conversation.
    let auth = format!("-H \"authorization: Bearer $(cat {token_file})\"");
    // Never `curl … | tar -xf -`: a refusal is a plain-text explanation, and
    // piping it into tar turns "pass confirm=true" into "this does not look
    // like a tar archive". Landing it in a file keeps the message readable.
    let fetch = format!("curl -sS --fail-with-body {auth}");
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
            "# 1. run this first: trade the one-time ticket for the token (single use)\ncurl -sSf -X POST -d \"code={code}\" \"{redeem}\" -o {token_file}"
        ),
        format!(
            "# 2. see what is there and how much of it, without moving any content.\n#    etag is blake3 of the note's exact bytes, quoted -- recompute it locally to find what changed\ncurl -sS {auth} \"{base}?format=index\""
        ),
    ];
    if let Some(directory) = directory {
        recipe.push(format!(
            "# pull this directory. A refusal lands in notes.tar as plain text and stops the extraction, so read it there\n{fetch} -o notes.tar \"{base}?prefix={}\" && mkdir -p {PULL_DIR} && tar -xf notes.tar -C {PULL_DIR}",
            percent_encode(directory)
        ));
    }
    // A confined grant has no wider pull to offer: everything it can reach is
    // already what the line above fetched.
    if !confined {
        recipe.push(format!(
            "# the whole vault. Past a few dozen notes this is refused until you say you mean it; the refusal lands in notes.tar and says how many there are and how to ask\n{fetch} -o notes.tar \"{base}\" && mkdir -p {PULL_DIR} && tar -xf notes.tar -C {PULL_DIR}"
        ));
    }
    if let Some(note) = note {
        recipe.push(format!(
            "# one note\n{fetch} -o note.md \"{base}/{}\"",
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
                "# stage what you mean to send in {PUSH_DIR} -- NOT {PULL_DIR}, which holds what you just pulled -- then check it\n#    `tar -cf - .` sends whatever the directory holds, so run this before the line below\nmkdir -p {PUSH_DIR} && tar -C {PUSH_DIR} -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}{destination}{and}dry_run=true\""
            ));
            recipe.push(format!(
                "# then make it. {separator}overwrite=true replaces existing notes with no version check -- unlike the single-note PUT below, a bulk push cannot ask whether they changed since you read them.\n#    200 all landed, 207 some did and the lines say which, 422 none did. Deleting is not part of this API: use the delete_notes tool\ntar -C {PUSH_DIR} -cf - . | curl -sS {auth} -X POST --data-binary @- \"{base}{destination}\" -w \"\\nHTTP %{{http_code}}\\n\""
            ));
        }
        if let Some(note) = note {
            recipe.push(format!(
                "# replace that note, only if it has not changed since you read it. The reply carries the new etag, so a second edit needs no fresh GET\ncurl -sS -D - {auth} -X PUT -H \"if-match: <etag>\" --data-binary @note.md \"{base}/{}\"",
                percent_encode_path(note)
            ));
        }
    }
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
    fn a_refusal_reaches_the_caller_instead_of_reaching_tar() {
        let joined = lines(false, &example_directory(false)).join("\n");

        assert!(
            !joined.contains("| tar -xf"),
            "a refusal is a plain-text explanation; piping it into tar turns \
             `pass confirm=true` into `this does not look like a tar archive` \
             and loses the one thing the caller needed: {joined}"
        );
        assert!(
            joined.contains("--fail-with-body"),
            "the body has to survive the failure it explains: {joined}"
        );
    }

    #[test]
    fn a_push_is_staged_somewhere_other_than_what_was_pulled() {
        let lines = lines(true, &example_directory(false));
        let pull = lines
            .iter()
            .find(|l| l.contains("tar -xf"))
            .expect("a pull line");
        let push = lines
            .iter()
            .find(|l| l.contains("-cf -") && !l.contains("dry_run"))
            .expect("a push line");

        let target = if pull.contains("./vault") {
            "./vault"
        } else {
            ""
        };
        assert!(!target.is_empty(), "{pull}");
        assert!(
            !push.contains(&format!("-C {target} -cf")),
            "running the recipe in order would push back everything it just \
             pulled: {push}"
        );
    }

    #[test]
    fn a_reading_grant_and_a_writing_one_do_not_share_a_token_file() {
        let reading = lines(false, &Example::Vault { note: None }).join("\n");
        let writing = lines(true, &Example::Vault { note: None }).join("\n");
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
