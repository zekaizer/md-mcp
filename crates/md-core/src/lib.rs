//! md-core — the vault model for md-mcp.
//!
//! This crate holds all pure logic: path-safe vault access, the Markdown
//! document/section parser, frontmatter handling, content hashing, and the
//! multi-file transaction engine. It has **no async or MCP dependencies** so it
//! can be unit-tested without a runtime; the server crate wires these into MCP
//! tools.

pub mod document;
pub mod error;
pub mod frontmatter;
pub mod links;
pub mod listing;
pub mod patch;
pub mod path;
pub mod relink;
pub mod replace;
pub mod section;
pub mod text;
pub mod transaction;
pub mod vault;

pub use document::{Document, Heading, OutlineEntry};
pub use error::{Code, Error, Result};
pub use links::{LinkKind, LinkOccurrence, extract_links};
pub use path::{join_segments, split_rel};
pub use patch::{Destination, Edit, Operation, Position, patch_sections};
pub use relink::{MoveMap, resolve_dest, rewrite_body};
pub use replace::{Hit, Replacement, replace_text};
pub use section::Scope;
pub use transaction::{CommitReceipt, Op, OpOutcome};
pub use vault::{Vault, VaultLock};
