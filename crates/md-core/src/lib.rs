//! md-core — the vault model for md-mcp.
//!
//! This crate holds all pure logic: path-safe vault access, the Markdown
//! document/section parser, frontmatter handling, content hashing, and the
//! multi-file transaction engine. It has **no async or MCP dependencies** so it
//! can be unit-tested without a runtime; the server crate wires these into MCP
//! tools.

pub mod document;
pub mod error;
pub mod text;
pub mod vault;

pub use document::{Document, Heading, OutlineEntry};
pub use error::{Code, Error, Result};
pub use vault::Vault;
