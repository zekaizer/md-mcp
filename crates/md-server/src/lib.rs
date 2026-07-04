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
}

impl MdServer {
    /// Build a server over an opened vault.
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self {
            vault: Arc::new(vault),
            lock: Arc::new(RwLock::new(())),
            events: None,
        }
    }

    /// Attach an event journal (ADR-0017).
    #[must_use]
    pub fn with_event_sink(mut self, sink: events::EventSink) -> Self {
        self.events = Some(Arc::new(sink));
        self
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
}

impl MdServer {
    /// The composed tool router across all tool families.
    pub(crate) fn tool_router() -> ToolRouter<Self> {
        Self::read_router() + Self::write_router() + Self::organize_router() + Self::search_router()
    }
}

#[tool_handler]
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
