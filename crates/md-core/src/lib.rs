//! md-core — the vault model for md-mcp.
//!
//! This crate holds all pure logic: path-safe vault access, the Markdown
//! document/section parser, frontmatter handling, content hashing, and the
//! multi-file transaction engine. It has **no async or MCP dependencies** so it
//! can be unit-tested without a runtime; the server crate wires these into MCP
//! tools.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_builds() {
        // Smoke test: the crate compiles and the test harness runs.
        assert_eq!(2 + 2, 4);
    }
}
