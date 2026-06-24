//! The md-mcp MCP server: thin rmcp tool wiring over the [`md_core`] vault.
//!
//! Tool bodies stay thin and call into `md-core`. Writes are serialized and made
//! exclusive with reads by an in-process readers-writer lock; reads run
//! concurrently.

pub mod config;
pub mod envelope;
pub mod tools_read;
pub mod tools_write;

use std::sync::Arc;

use md_core::Vault;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::{ServerHandler, tool_handler};
use tokio::sync::RwLock;

/// The MCP server handler. Cheap to clone (shared `Arc`s).
#[derive(Clone)]
pub struct MdServer {
    vault: Arc<Vault>,
    /// Read guard for reads; write guard for the commit step (ADR-0008).
    lock: Arc<RwLock<()>>,
}

impl MdServer {
    /// Build a server over an opened vault.
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self {
            vault: Arc::new(vault),
            lock: Arc::new(RwLock::new(())),
        }
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
}

impl MdServer {
    /// The composed tool router across all tool families.
    pub(crate) fn tool_router() -> ToolRouter<Self> {
        Self::read_router() + Self::write_router()
    }
}

#[tool_handler(
    instructions = "md-mcp manages a single vault of pure-Markdown notes (.md + YAML frontmatter). Address notes by vault-relative path; read large notes via read_outlines then read_sections. Destructive batches are all-or-nothing."
)]
impl ServerHandler for MdServer {}

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
}
