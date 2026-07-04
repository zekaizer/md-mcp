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
    let server = MdServer::new(vault);

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
