//! Server configuration, read from the environment (fail-closed).

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Runtime configuration for the md-mcp server.
#[derive(Debug, Clone)]
pub struct Config {
    /// The vault root directory.
    pub vault_dir: PathBuf,
}

impl Config {
    /// Read configuration from the environment. `MD_VAULT` is required.
    pub fn from_env() -> Result<Self> {
        let vault_dir = std::env::var("MD_VAULT")
            .context("MD_VAULT must be set to the vault directory")?
            .into();
        Ok(Self { vault_dir })
    }
}
