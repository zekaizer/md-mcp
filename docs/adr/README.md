# Architecture Decision Records

This directory holds the Architecture Decision Records (ADRs) for md-mcp, in the
[Michael Nygard format](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).
See [ADR-0001](0001-record-architecture-decisions.md) for the process itself.

Each ADR is one decision in a file named `NNNN-kebab-title.md`. Accepted ADRs are
immutable; a changed decision supersedes the old one with a new ADR.

## Index

- [0001](0001-record-architecture-decisions.md) — Record architecture decisions
- [0002](0002-implementation-language-and-stack.md) — Implementation language, runtime, MCP transport, and core libraries
- [0003](0003-document-parser-and-section-model.md) — Document parser and section model
- [0004](0004-frontmatter-parse-and-typed-splice.md) — Frontmatter: typed parse and byte-preserving splice
- [0005](0005-content-hash.md) — Per-section content hash
- [0006](0006-vault-path-jail-and-atomic-write.md) — Vault path jail and atomic write
- [0007](0007-multi-file-transaction.md) — Multi-file transaction: journal, backup, and crash recovery
- [0008](0008-concurrency-and-isolation.md) — Concurrency and isolation
- [0009](0009-delete-recovery-and-move-validation.md) — Delete recovery model and move validation
- [0010](0010-search-strategy.md) — Search and listing strategy
- [0011](0011-error-envelope-and-structured-output.md) — Error envelope and structured output
- [0012](0012-testing-and-benchmarking.md) — Testing and benchmarking
- [0013](0013-http-transport.md) — HTTP transport (Streamable HTTP), stdio optional
- [0014](0014-oauth-authentication.md) — Authentication: co-hosted OAuth 2.1 for the claude.ai connector
- [0015](0015-stdio-end-to-end-suite.md) — stdio end-to-end test suite
- [0016](0016-git-sync-integration.md) — Git sync integration: coexistence hardening and the `sync_vault` tool
- [0017](0017-event-journal-and-hook.md) — Event journal and commit hook
