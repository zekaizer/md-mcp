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

# Streamable HTTP (default) — serves MCP at http://127.0.0.1:7654/mcp
MD_VAULT=/path/to/vault target/release/md-server

# stdio — for desktop MCP clients that launch the server themselves
MD_VAULT=/path/to/vault target/release/md-server --stdio
```

`MD_VAULT` (the vault directory) is required. The transport is selected by the
`--http` / `--stdio` flag, else the `MD_TRANSPORT` env var (`http` | `stdio`),
else HTTP. The server logs to stderr (under stdio, stdout is the JSON-RPC
channel). See [ADR-0013](docs/adr/0013-http-transport.md).

### HTTP options

| Variable | Default | Meaning |
|---|---|---|
| `MD_HTTP_ADDR` | `127.0.0.1:7654` | bind address; `0.0.0.0:…` to expose beyond loopback |
| `MD_HTTP_TOKEN` | unset | when set, every request must send `Authorization: Bearer <token>` |
| `MD_HTTP_ALLOWED_HOSTS` | unset | comma-separated `Host` allowlist; `*` disables the guard |
| `MD_HTTP_ALLOWED_ORIGINS` | unset | comma-separated `Origin` allowlist; `*` disables the guard |

The default bind is loopback with no auth. Two browser-facing guards are on by
default: the `Host` allowlist (loopback) and the `Origin` allowlist (the loopback
origins for the bound port — this is what stops a random web page from driving the
tools at `http://127.0.0.1:7654/mcp`; non-browser clients send no `Origin` and are
unaffected). Exposing the server (a non-loopback `MD_HTTP_ADDR`) means setting
`MD_HTTP_TOKEN`, and listing the served host(s) in `MD_HTTP_ALLOWED_HOSTS` (and,
for browser clients, the origin in `MD_HTTP_ALLOWED_ORIGINS`) — or `*` to disable a
guard. There is no in-process TLS — terminate TLS upstream (reverse proxy /
tunnel) for remote use.

## Testing

```sh
make check    # fmt + clippy + cargo test + stdio e2e — the pre-push gate
make test     # unit + in-process protocol tests only
make e2e      # stdio black-box end-to-end suite (functional + hardening)
```

Unit tests and one in-process rmcp protocol test live with the crates
([ADR-0012](docs/adr/0012-testing-and-benchmarking.md)); a stdio black-box suite
drives the real binary over its wire protocol
([tests/e2e](tests/e2e/README.md), [ADR-0015](docs/adr/0015-stdio-end-to-end-suite.md)).

## Documentation

- **[CONTEXT.md](CONTEXT.md)** — glossary / ubiquitous language. Start here.
- **[docs/tool_spec.md](docs/tool_spec.md)** — the full tool specification (the
  behavioral contract).
- **[docs/adr/README.md](docs/adr/README.md)** — architecture decision records
  (why each choice was made).
