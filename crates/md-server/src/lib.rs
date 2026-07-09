//! The md-mcp MCP server: thin rmcp tool wiring over the [`md_core`] vault.
//!
//! Tool bodies stay thin and call into `md-core`. Writes are serialized and made
//! exclusive with reads by an in-process readers-writer lock; reads run
//! concurrently.

pub mod config;
pub mod envelope;
pub mod events;
pub mod http;
pub mod oauth;
pub mod sync;
pub mod tools_organize;
pub mod tools_read;
pub mod tools_search;
pub mod tools_sync;
pub mod tools_write;

use std::sync::Arc;

use md_core::Vault;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool_handler};
use tokio::sync::RwLock;

/// The MCP server handler. Cheap to clone (shared `Arc`s).
#[derive(Clone)]
pub struct MdServer {
    vault: Arc<Vault>,
    /// Read guard for reads; write guard for the commit step (ADR-0008).
    lock: Arc<RwLock<()>>,
    /// Event journal, when enabled (ADR-0017).
    events: Option<Arc<events::EventSink>>,
    /// Git driver, when sync is enabled (ADR-0016).
    git: Option<Arc<sync::GitSync>>,
    /// Per-batch auto-commit (ADR-0018); meaningful only with `git` set.
    auto_commit: bool,
    /// Signals the debounced auto-push task after each auto-commit (ADR-0018).
    push_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Outcome of the most recent push/sync attempt; `Some` while failing
    /// (ADR-0019). Std mutex: only ever held for a field read/write.
    sync_health: Arc<std::sync::Mutex<Option<sync::SyncHealth>>>,
    /// Vault-relative path of an introduction note advertised in the server
    /// instructions (`MD_INTRO_NOTE`), so agents read the vault's own guide
    /// before working in it.
    intro_note: Option<String>,
    /// The advertised tool set: the base families, plus `sync_vault` when git
    /// sync is enabled.
    tool_router: ToolRouter<Self>,
}

impl MdServer {
    /// Build a server over an opened vault.
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self {
            vault: Arc::new(vault),
            lock: Arc::new(RwLock::new(())),
            events: None,
            git: None,
            auto_commit: false,
            push_tx: None,
            sync_health: Arc::new(std::sync::Mutex::new(None)),
            intro_note: None,
            tool_router: Self::base_router(),
        }
    }

    /// Attach an event journal (ADR-0017).
    #[must_use]
    pub fn with_event_sink(mut self, sink: events::EventSink) -> Self {
        self.events = Some(Arc::new(sink));
        self
    }

    /// Attach the git driver and expose the `sync_vault` tool (ADR-0016).
    #[must_use]
    pub fn with_git_sync(mut self, git: sync::GitSync) -> Self {
        self.git = Some(Arc::new(git));
        self.tool_router = Self::base_router() + Self::sync_router();
        self
    }

    /// Commit every write batch as it lands (ADR-0018); requires a git driver.
    #[must_use]
    pub fn with_auto_commit(mut self) -> Self {
        self.auto_commit = true;
        self
    }

    /// Push `debounce` after the most recent auto-commit (ADR-0018); spawns
    /// the background task, so a tokio runtime must be running. Failed pushes
    /// retry on a capped exponential backoff, and one synthetic signal is sent
    /// at startup so a backlog stranded by a previous run drains without
    /// waiting for new writes (ADR-0019).
    #[must_use]
    pub fn with_auto_push(mut self, debounce: std::time::Duration) -> Self {
        /// First retry delay after a failed push (doubles per failure).
        const RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(15);
        /// Backoff ceiling: a dead remote is probed this often at most.
        const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(()); // startup nudge: drain any stranded backlog
        self.push_tx = Some(tx);
        tokio::spawn(sync::auto_push_task(
            self.clone(),
            rx,
            debounce,
            RETRY_INITIAL,
            RETRY_CAP,
        ));
        self
    }

    /// Run a full sync every `every` (ADR-0018); spawns the background task,
    /// so a tokio runtime must be running.
    #[must_use]
    pub fn with_sync_interval(self, every: std::time::Duration) -> Self {
        tokio::spawn(sync::interval_sync_task(self.clone(), every));
        self
    }

    /// Advertise `path` (vault-relative) as the vault's introduction note in
    /// the server instructions.
    #[must_use]
    pub fn with_intro_note(mut self, path: String) -> Self {
        self.intro_note = Some(path);
        self
    }

    /// The git driver, when sync is enabled.
    pub(crate) fn git_sync(&self) -> Option<&sync::GitSync> {
        self.git.as_deref()
    }

    /// The vault handle (used by tool implementations).
    #[must_use]
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// The readers-writer coordination lock.
    #[must_use]
    pub fn lock(&self) -> &RwLock<()> {
        &self.lock
    }

    /// Record a failed push/sync attempt (ADR-0019). The start of the failure
    /// streak is kept across repeats so the warning shows total duration.
    pub(crate) fn report_sync_failure(&self, ahead: u64, reason: String) {
        let mut health = self.sync_health.lock().unwrap();
        let since = health
            .as_ref()
            .map_or_else(std::time::Instant::now, |h| h.since);
        *health = Some(sync::SyncHealth {
            since,
            ahead,
            reason,
        });
    }

    /// Record a successful push/sync attempt: the vault is on the remote.
    pub(crate) fn report_sync_ok(&self) {
        self.sync_health.lock().unwrap().take();
    }

    /// The warning carried on write responses while sync is failing (ADR-0019);
    /// `None` when healthy (the field is then omitted from the JSON).
    pub(crate) fn sync_warning(&self) -> Option<String> {
        let health = self.sync_health.lock().unwrap();
        health.as_ref().map(|h| {
            format!(
                "{} local commit(s) not on the remote ({}; failing for {}s)",
                h.ahead,
                h.reason,
                h.since.elapsed().as_secs()
            )
        })
    }

    /// Record a successful mutation in the event journal, if one is attached.
    /// Emission is best-effort: a journal failure is logged, never surfaced —
    /// the vault write has already durably committed.
    pub(crate) fn emit_event(&self, tool: &str, batch_id: Option<&str>, ops: &[events::EventOp]) {
        let Some(sink) = &self.events else { return };
        if ops.is_empty() {
            return;
        }
        if let Err(e) = sink.emit(tool, batch_id, ops) {
            tracing::warn!(error = %e, "event journal append failed");
        }
    }

    /// Commit a batch's touched paths as one git commit (ADR-0018), when
    /// auto-commit is on. Called while the write guard is still held, so the
    /// commit snapshot is exactly the batch's result; path-scoped staging
    /// keeps concurrent external edits out. A git failure is logged, never
    /// surfaced — the vault, not git, is the durability layer.
    ///
    /// `args` is the invoking tool call (request struct or its items); a
    /// condensed rendering — long strings truncated, total size capped — goes
    /// into the commit body so `git log` shows what command produced each
    /// commit without replicating whole note contents.
    pub(crate) async fn auto_commit(
        &self,
        tool: &str,
        ops: &[events::EventOp],
        args: &(impl serde::Serialize + Sync),
    ) {
        if !self.auto_commit || ops.is_empty() {
            return;
        }
        let Some(git) = self.git_sync() else { return };
        let paths: Vec<String> = ops
            .iter()
            .flat_map(events::EventOp::touched_paths)
            .map(str::to_string)
            .collect();
        let subject = format!("mcp({tool}): {} notes", ops.len());
        let message = match serde_json::to_value(args) {
            Ok(v) => format!("{subject}\n\n{tool} {}", summarize_tool_args(&v)),
            Err(_) => subject,
        };
        let flock = match self.vault().exclusive_lock() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "auto-commit skipped, cannot take vault lock");
                return;
            }
        };
        match git.commit_paths(&message, &paths).await {
            Ok(true) => {
                // Wake the debounced push task, if configured.
                if let Some(tx) = &self.push_tx {
                    let _ = tx.send(());
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, tool, "auto-commit failed"),
        }
        drop(flock);
    }
}

/// Longest string kept verbatim inside a condensed tool call (chars).
const ARG_STR_MAX: usize = 80;
/// Total size cap of one condensed tool call (chars).
const ARG_TOTAL_MAX: usize = 800;

/// Truncate every long string in `v` to [`ARG_STR_MAX`] chars, in place,
/// marking how much was dropped — note contents must not bloat commit bodies.
fn condense_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let chars = s.chars().count();
            if chars > ARG_STR_MAX {
                let head: String = s.chars().take(ARG_STR_MAX).collect();
                *s = format!("{head}…(+{} chars)", chars - ARG_STR_MAX);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(condense_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(condense_json),
        _ => {}
    }
}

/// One-line condensed rendering of a tool call's arguments for the
/// auto-commit body: compact JSON with long strings truncated and an overall
/// size cap.
fn summarize_tool_args(args: &serde_json::Value) -> String {
    let mut v = args.clone();
    condense_json(&mut v);
    let s = serde_json::to_string(&v).unwrap_or_default();
    if s.chars().count() > ARG_TOTAL_MAX {
        let head: String = s.chars().take(ARG_TOTAL_MAX).collect();
        format!("{head}…")
    } else {
        s
    }
}

impl MdServer {
    /// The composed tool router across the always-on tool families.
    pub(crate) fn base_router() -> ToolRouter<Self> {
        Self::read_router() + Self::write_router() + Self::organize_router() + Self::search_router()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MdServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo/Implementation are #[non_exhaustive]: mutate defaults.
        let mut info = ServerInfo::default();
        info.server_info.name = "md-mcp".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // The version is in server_info too, but most clients never surface
        // that to the model — instructions are what an agent actually reads.
        let mut instructions = concat!(
            "md-mcp v",
            env!("CARGO_PKG_VERSION"),
            " manages a single vault of pure-Markdown notes (.md + YAML frontmatter). \
             Address notes by vault-relative path; read large notes via read_outlines \
             then read_sections. Destructive batches are all-or-nothing."
        )
        .to_string();
        if let Some(intro) = &self.intro_note {
            instructions.push_str(&format!(
                " Before working in this vault, read \"{intro}\" (via read_notes): it \
                 introduces the vault's purpose, structure, and conventions."
            ));
        }
        info.instructions = Some(instructions);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> MdServer {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        // Keep the temp dir alive for the test's lifetime by leaking it.
        std::mem::forget(dir);
        MdServer::new(vault)
    }

    #[test]
    fn advertises_tools_capability_and_instructions() {
        let info = server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised"
        );
        let instructions = info.instructions.unwrap_or_default();
        assert!(
            instructions.contains(concat!("md-mcp v", env!("CARGO_PKG_VERSION"))),
            "instructions must carry the server version: {instructions}"
        );
    }

    #[test]
    fn intro_note_is_advertised_in_instructions() {
        let with = server()
            .with_intro_note("meta/start-here.md".into())
            .get_info()
            .instructions
            .unwrap_or_default();
        assert!(with.contains("meta/start-here.md"), "got: {with}");
        assert!(with.contains("read_notes"), "got: {with}");

        let without = server().get_info().instructions.unwrap_or_default();
        assert!(!without.contains("start-here"), "got: {without}");
    }

    #[test]
    fn identifies_itself_as_md_mcp() {
        // Clients show serverInfo in UIs/logs; it must not be the framework's.
        let info = server().get_info();
        assert_eq!(info.server_info.name, "md-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn tools_carry_behavioral_annotations() {
        // Clients use these hints to auto-approve reads and warn on writes.
        // Every exposed tool must classify itself; the classes below are the
        // contract, so a new unannotated tool fails the exhaustiveness check.
        let read_only = ["list_notes", "search_notes", "read_notes", "read_outlines", "read_sections"];
        let additive = ["create_notes", "append_notes"];
        let destructive = [
            "edit_sections", "edit_properties", "delete_notes", "rename_notes",
            "relocate_notes", "sync_vault",
        ];
        // Only sync_vault reaches an external git remote.
        let open_world = ["sync_vault"];

        let tools = MdServer::base_router() + MdServer::sync_router();
        let tools = tools.list_all();
        for name in read_only.iter().chain(&additive).chain(&destructive) {
            let t = tools
                .iter()
                .find(|t| t.name == *name)
                .unwrap_or_else(|| panic!("tool {name} not registered"));
            let a = t
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool {name} has no annotations"));

            let expect_read_only = read_only.contains(name);
            assert_eq!(
                a.read_only_hint,
                Some(expect_read_only),
                "{name} read_only_hint"
            );
            // destructive_hint is meaningful only for writers.
            if !expect_read_only {
                let expect_destructive = destructive.contains(name);
                assert_eq!(
                    a.destructive_hint,
                    Some(expect_destructive),
                    "{name} destructive_hint"
                );
            }
            assert_eq!(
                a.open_world_hint,
                Some(open_world.contains(name)),
                "{name} open_world_hint"
            );
        }
    }
}
