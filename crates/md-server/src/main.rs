//! md-server — the md-mcp MCP server binary (stdio transport).

use anyhow::Result;
use md_core::Vault;
use md_server::MdServer;
use md_server::config::Config;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the JSON-RPC channel — all logs go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let vault = Vault::open(&config.vault_dir)?;
    let server = MdServer::new(vault);

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
