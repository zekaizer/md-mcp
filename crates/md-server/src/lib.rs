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
    /// the background task, so a tokio runtime must be running.
    #[must_use]
    pub fn with_auto_push(mut self, debounce: std::time::Duration) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.push_tx = Some(tx);
        tokio::spawn(sync::auto_push_task(self.clone(), rx, debounce));
        self
    }

    /// Run a full sync every `every` (ADR-0018); spawns the background task,
    /// so a tokio runtime must be running.
    #[must_use]
    pub fn with_sync_interval(self, every: std::time::Duration) -> Self {
        tokio::spawn(sync::interval_sync_task(self.clone(), every));
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

    /// Record a successful mutation in the event journal, if one is attached.
    /// Emission is best-effort: a journal failure is logged, never surfaced —
    /// the vault write has already durably committed.
    pub(crate) fn emit_event(&self, tool: &str, batch_id: Option<&str>, ops: &[events::EventOp]) {
        let Some(sink) = &self.events else { return };
        if ops.is_empty() {
            return;
        }
        if let Err(e) = sink.emit(tool, batch_id, ops) {
            tracing::warn!("event journal append failed: {e}");
        }
    }

    /// Commit a batch's touched paths as one git commit (ADR-0018), when
    /// auto-commit is on. Called while the write guard is still held, so the
    /// commit snapshot is exactly the batch's result; path-scoped staging
    /// keeps concurrent external edits out. A git failure is logged, never
    /// surfaced — the vault, not git, is the durability layer.
    pub(crate) async fn auto_commit(&self, tool: &str, ops: &[events::EventOp]) {
        if !self.auto_commit || ops.is_empty() {
            return;
        }
        let Some(git) = self.git_sync() else { return };
        let paths: Vec<String> = ops
            .iter()
            .flat_map(events::EventOp::touched_paths)
            .map(str::to_string)
            .collect();
        let message = format!("mcp({tool}): {} notes", ops.len());
        let flock = match self.vault().exclusive_lock() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("auto-commit skipped, cannot take vault lock: {e}");
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
            Err(e) => tracing::warn!("auto-commit failed: {e}"),
        }
        drop(flock);
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
        info.instructions = Some(
            "md-mcp manages a single vault of pure-Markdown notes (.md + YAML frontmatter). \
             Address notes by vault-relative path; read large notes via read_outlines then \
             read_sections. Destructive batches are all-or-nothing."
                .into(),
        );
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
            instructions.contains("md-mcp"),
            "instructions: {instructions}"
        );
    }

    #[test]
    fn identifies_itself_as_md_mcp() {
        // Clients show serverInfo in UIs/logs; it must not be the framework's.
        let info = server().get_info();
        assert_eq!(info.server_info.name, "md-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    }
}
