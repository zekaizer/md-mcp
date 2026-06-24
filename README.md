# md-mcp

> Provisional working name.

An MCP server that exposes a single vault of pure-Markdown notes (`.md` + optional
YAML frontmatter) to AI agents, with structure-aware tools — outline reads,
section-level edits, frontmatter properties, safe batch writes — instead of
treating the vault as a flat pile of files.

**Status:** early implementation. The tool surface is specified; the server is
being built foundations-first (path safety → parser → frontmatter → transaction →
tools), test-driven.

## Documentation

- **[CONTEXT.md](CONTEXT.md)** — glossary / ubiquitous language. Start here.
- **[docs/tool_spec.md](docs/tool_spec.md)** — the full tool specification (the
  behavioral contract).
- **[docs/adr/README.md](docs/adr/README.md)** — architecture decision records
  (why each choice was made).
