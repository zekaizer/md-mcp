# md-mcp

> Provisional working name.

An MCP server that exposes a single vault of pure-Markdown notes (`.md` + optional
YAML frontmatter) to AI agents, with structure-aware tools — outline reads,
section-level edits, frontmatter properties, safe batch writes — instead of
treating the vault as a flat pile of files.

**Status:** the full 11-tool surface is implemented and tested (read, search,
section/property edits, safe batch moves and deletes) over a crash-safe
transaction engine, plus opt-in git sync (a 12th tool, `sync_vault`, with
auto-commit/push/pull automation) and an event journal for external consumers.
Built foundations-first (path safety → parser → frontmatter → transaction →
tools), test-driven.

## Build & run

```sh
cargo build --release            # produces target/release/md-server

# Streamable HTTP (default) — serves MCP at http://127.0.0.1:7654/mcp
MD_VAULT=/path/to/vault target/release/md-server

# stdio — for desktop MCP clients that launch the server themselves
MD_VAULT=/path/to/vault target/release/md-server --stdio
```

The vault directory is required (`--vault <dir>` or `MD_VAULT`). Every setting is
a `--flag` that falls back to its `MD_*` env var, with precedence CLI > env >
default ([ADR-0023](docs/adr/0023-cli-arguments-and-clap.md)); run `md-server
--help` for the full list. The transport is selected by the `--http` / `--stdio`
flag, else the `MD_TRANSPORT` env var (`http` | `stdio`), else HTTP. The server
logs to stderr (under stdio, stdout is the JSON-RPC channel). See
[ADR-0013](docs/adr/0013-http-transport.md).

`MD_INTRO_NOTE` (optional) names a vault-relative note (e.g. `meta/start-here.md`)
advertised in the MCP server instructions, so connecting agents read the vault's
own introduction before working in it.

`MD_LOG_FORMAT` (optional, `text` | `json`) switches stderr logging to one JSON
object per line for log shipping, and `/mcp` requests get a structured access
log (tool, status, duration) — see
[ADR-0021](docs/adr/0021-structured-logging.md).

### HTTP options

| Variable | Default | Meaning |
|---|---|---|
| `MD_HTTP_ADDR` | `127.0.0.1:7654` | bind address; `0.0.0.0:…` to expose beyond loopback |
| `MD_HTTP_TOKEN` | unset | when set, every request must send `Authorization: Bearer <token>`. For a CLI-managed secret use `--http-token-file <path>` (the token is read from the file, never passed on argv) |
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

### Git sync & events (opt-in)

| Variable | Default | Meaning |
|---|---|---|
| `MD_GIT_SYNC` | unset | `1` exposes the `sync_vault` tool (vault root must be the repo toplevel) |
| `MD_GIT_AUTO_COMMIT` | unset | `1` commits every write batch as one path-scoped git commit |
| `MD_GIT_AUTO_PUSH_SECS` | unset | push N seconds after the most recent auto-commit (debounced) |
| `MD_GIT_SYNC_INTERVAL_SECS` | unset | run a full sync every N seconds |
| `MD_EVENTS` | unset | `1` appends every mutation to `.md-mcp/events.jsonl` |
| `MD_ON_COMMIT_HOOK` | unset | command run per event record with the JSON on stdin (implies `MD_EVENTS`) |

The automation variables require `MD_GIT_SYNC=1`. Whenever the vault is a git
repository (opted in or not), the server excludes its internal `.md-mcp/` state
via `.git/info/exclude` and takes an OS lock (`.md-mcp/lock`) around every
transaction commit and git operation — external tooling can coordinate with
`flock <vault>/.md-mcp/lock git …`. When a push or sync fails, write responses
carry a `sync_warning` field until a sync succeeds, and the auto-push task
retries on a capped backoff (a stranded backlog also drains at startup). See
[ADR-0016](docs/adr/0016-git-sync-integration.md),
[ADR-0017](docs/adr/0017-event-journal-and-hook.md),
[ADR-0018](docs/adr/0018-git-automation.md),
[ADR-0019](docs/adr/0019-sync-health-and-push-retry.md).

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

## Deployment

Production runs as a systemd service reading one env file, behind a Cloudflare
Tunnel — see [deploy/README.md](deploy/README.md) for the unit file, an
annotated sample env, and the step-by-step guide
([ADR-0020](docs/adr/0020-deployment-and-configuration-posture.md)).

## Documentation

- **[CONTEXT.md](CONTEXT.md)** — glossary / ubiquitous language. Start here.
- **[docs/tool_spec.md](docs/tool_spec.md)** — the full tool specification (the
  behavioral contract).
- **[docs/adr/README.md](docs/adr/README.md)** — architecture decision records
  (why each choice was made).
