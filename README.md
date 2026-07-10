# md-mcp

> Provisional working name.

An MCP server that gives AI agents structured access to a vault of plain
Markdown notes (`.md` + optional YAML frontmatter). Instead of treating your
notes as a flat pile of files, agents work with structure: read outlines,
edit individual sections, manage frontmatter properties, and reorganize notes
— all with atomic, all-or-nothing batches and an optional git-backed history.

Works with any MCP client: Claude Code, Claude Desktop, or anything speaking
Streamable HTTP or stdio. Pairs naturally with an Obsidian vault (the notes
stay pure Markdown; Obsidian-specific syntax like wikilinks is left untouched).

## What agents can do

| Tool | What it does |
|---|---|
| `list_notes` | Browse the vault (globs, pagination, directories) |
| `search_notes` | Search content, filenames, and frontmatter |
| `read_notes` | Read whole notes (body + frontmatter) |
| `read_outlines` | Scan a note's heading structure without reading the body |
| `read_sections` | Read only the sections you need from large notes |
| `create_notes` | Create notes (frontmatter as structured data) |
| `append_notes` | Append to notes |
| `edit_sections` | Replace/append/insert/delete/rename/move sections by heading path |
| `edit_properties` | Set or remove individual frontmatter keys |
| `move_notes` | Move and/or rename notes and directories in one step — optionally rewriting Markdown links vault-wide so nothing breaks |
| `delete_notes` | Delete to a recoverable trash, never permanently |
| `sync_vault` | Commit, pull (rebase), and push to a git remote (opt-in) |

Safety properties end users get for free:

- **All-or-nothing batches** — a destructive batch (edit / move / delete)
  either fully applies or leaves the vault untouched, even across many files,
  and survives a crash mid-write.
- **Path safety** — the server cannot read or write outside the vault
  directory; deletes go to a trash folder inside the vault.
- **Link integrity** — `move_notes` with `update_links: true` rewrites
  standard Markdown links (`[text](path.md)`) across the vault so moves don't
  leave broken links.
- **Concurrency-safe edits** — section edits can carry a content hash so a
  stale edit is rejected instead of silently overwriting newer changes.

## Quick start

Grab a binary from [GitHub releases](https://github.com/zekaizer/md-mcp/releases),
or build from source:

```sh
cargo build --release            # produces target/release/md-server
```

Run it against your vault:

```sh
# Streamable HTTP (default) — serves MCP at http://127.0.0.1:7654/mcp
MD_VAULT=/path/to/vault md-server

# stdio — for MCP clients that launch the server themselves
MD_VAULT=/path/to/vault md-server --stdio
```

Then connect your MCP client. For example, Claude Code:

```sh
# HTTP
claude mcp add --transport http my-vault http://127.0.0.1:7654/mcp

# stdio
claude mcp add my-vault -e MD_VAULT=/path/to/vault -- md-server --stdio
```

The vault directory is the only required setting (`--vault <dir>` or
`MD_VAULT`). Every setting is a `--flag` with an `MD_*` env-var fallback
(CLI > env > default); run `md-server --help` for the full list. The
transport is selected by the `--http` / `--stdio` flag, else the
`MD_TRANSPORT` env var (`http` | `stdio`), else HTTP. The server logs to
stderr (under stdio, stdout is the JSON-RPC channel).

`MD_INTRO_NOTE` (optional) names a note (e.g. `meta/start-here.md`) that
connecting agents are told to read first — useful for teaching agents your
vault's conventions.

## Configuration

### HTTP options

| Variable | Default | Meaning |
|---|---|---|
| `MD_HTTP_ADDR` | `127.0.0.1:7654` | bind address; `0.0.0.0:…` to expose beyond loopback |
| `MD_HTTP_TOKEN` | unset | when set, every request must send `Authorization: Bearer <token>`. For a CLI-managed secret use `--http-token-file <path>` (the token is read from the file, never passed on argv) |
| `MD_HTTP_ALLOWED_HOSTS` | unset | comma-separated `Host` allowlist; `*` disables the guard |
| `MD_HTTP_ALLOWED_ORIGINS` | unset | comma-separated `Origin` allowlist; `*` disables the guard |

The default bind is loopback with no auth. Two browser-facing guards are on by
default: the `Host` allowlist (loopback) and the `Origin` allowlist (the
loopback origins for the bound port — this is what stops a random web page from
driving the tools at `http://127.0.0.1:7654/mcp`; non-browser clients send no
`Origin` and are unaffected). Exposing the server (a non-loopback
`MD_HTTP_ADDR`) means setting `MD_HTTP_TOKEN` and listing the served host(s) in
`MD_HTTP_ALLOWED_HOSTS` (and, for browser clients, the origin in
`MD_HTTP_ALLOWED_ORIGINS`) — or `*` to disable a guard. There is no in-process
TLS — terminate TLS upstream (reverse proxy / tunnel) for remote use.

### Git sync & events (opt-in)

With git sync enabled, every change an agent makes can land as a git commit
and be pushed to a remote — your vault gets a full, revertable history and
syncs across machines.

| Variable | Default | Meaning |
|---|---|---|
| `MD_GIT_SYNC` | unset | `1` exposes the `sync_vault` tool (vault root must be the repo toplevel) |
| `MD_GIT_AUTO_COMMIT` | unset | `1` commits every write batch as one path-scoped git commit |
| `MD_GIT_AUTO_PUSH_SECS` | unset | push N seconds after the most recent auto-commit (debounced) |
| `MD_GIT_SYNC_INTERVAL_SECS` | unset | run a full sync every N seconds |
| `MD_EVENTS` | unset | `1` appends every mutation to `.md-mcp/events.jsonl` |
| `MD_ON_COMMIT_HOOK` | unset | command run per event record with the JSON on stdin (implies `MD_EVENTS`) |

The automation variables require `MD_GIT_SYNC=1`. When a push or sync fails,
write responses carry a `sync_warning` field until a sync succeeds, and the
push is retried automatically. External tooling can coordinate with the
server's lock via `flock <vault>/.md-mcp/lock git …`.

`MD_LOG_FORMAT` (optional, `text` | `json`) switches stderr logging to one
JSON object per line for log shipping, with a structured access log for
`/mcp` requests.

## Deployment

Production runs well as a systemd service reading one env file, optionally
behind a Cloudflare Tunnel for remote access — see
[deploy/README.md](deploy/README.md) for the unit file, an annotated sample
env, and a step-by-step guide.

## Development

```sh
make check    # fmt + clippy + cargo test + stdio e2e — the pre-push gate
make test     # unit + in-process protocol tests only
make e2e      # stdio black-box end-to-end suite (functional + hardening)
```

Contributor documentation:

- **[CONTEXT.md](CONTEXT.md)** — glossary / ubiquitous language. Start here.
- **[docs/tool_spec.md](docs/tool_spec.md)** — the full tool specification
  (the behavioral contract).
- **[docs/adr/README.md](docs/adr/README.md)** — architecture decision
  records (why each choice was made).
