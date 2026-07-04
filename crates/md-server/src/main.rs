//! md-server — the md-mcp MCP server binary (Streamable HTTP by default, stdio
//! optional; see ADR-0013).

use anyhow::Result;
use md_core::Vault;
use md_server::MdServer;
use md_server::config::{Config, Transport};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs always go to stderr: under stdio, stdout is the JSON-RPC channel and
    // any stray write corrupts it; under HTTP, stderr is simply the safe default.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::from_env_and_args(&args)?;
    let vault = Vault::open(&config.vault_dir)?;
    // Keep .md-mcp/ out of any git repo the vault lives in (ADR-0016).
    md_server::sync::ensure_git_exclude(&config.vault_dir);
    let mut server = MdServer::new(vault);
    if config.events.enabled {
        let hook_tx = config
            .events
            .hook
            .clone()
            .map(md_server::events::spawn_hook);
        let sink = md_server::events::EventSink::open(&config.vault_dir, hook_tx)?;
        server = server.with_event_sink(sink);
    }
    if config.git.sync {
        // Precondition failure disables sync with a warning (ADR-0016), it
        // never fails startup: the vault itself is fully usable without git.
        match md_server::sync::GitSync::preflight(&config.vault_dir).await {
            Ok(git) => server = server.with_git_sync(git),
            Err(warning) => tracing::warn!("{warning}"),
        }
    }

    match config.transport {
        Transport::Stdio => {
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http(http) => {
            md_server::http::serve(server, &http).await?;
        }
    }
    Ok(())
}
