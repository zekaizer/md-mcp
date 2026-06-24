# md-mcp

> Provisional working name.

An MCP server that exposes a single vault of pure-Markdown notes (`.md` + optional
YAML frontmatter) to AI agents, with structure-aware tools — outline reads,
section-level edits, frontmatter properties, safe batch writes — instead of
treating the vault as a flat pile of files.

**Status:** the full 12-tool surface is implemented and tested (read, search,
section/property edits, safe batch moves and deletes) over a crash-safe
transaction engine. Built foundations-first (path safety → parser → frontmatter →
transaction → tools), test-driven.

## Build & run

```sh
cargo build --release            # produces target/release/md-server
MD_VAULT=/path/to/vault target/release/md-server   # speaks MCP over stdio
```

Register `md-server` as a stdio MCP server in your client, with `MD_VAULT` set to
the vault directory. The server logs to stderr (stdout is the JSON-RPC channel).

## Documentation

- **[CONTEXT.md](CONTEXT.md)** — glossary / ubiquitous language. Start here.
- **[docs/tool_spec.md](docs/tool_spec.md)** — the full tool specification (the
  behavioral contract).
- **[docs/adr/README.md](docs/adr/README.md)** — architecture decision records
  (why each choice was made).
