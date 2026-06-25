# 2. Implementation language, runtime, MCP transport, and core libraries

## Status

Accepted — the "MCP SDK and transport" decision (stdio-only, no HTTP) is
partially superseded by [ADR-0013](0013-http-transport.md), which adds a
Streamable HTTP transport. Everything else in this ADR stands.

## Context

md-mcp implements the tool surface in [`docs/tool_spec.md`](../tool_spec.md): a
local MCP server over a single vault of pure-Markdown notes. Before feature code
lands we must fix the language, the MCP SDK and its transport, the workspace
layout, and the set of supporting libraries — these gate every later module.

A sibling project, [strata](https://github.com/zekaizer/stratum), already solves
a superset of this problem in Rust (it adds Obsidian semantics, full-text search,
and embeddings on top of the same vault model). md-mcp deliberately reuses
strata's *patterns* as reference — not its code — while staying pure-Markdown.

Two facts shape the choices and separate md-mcp from strata:

1. **md-mcp is a local, single-vault server consumed by a desktop MCP client**,
   not a remote HTTP service. strata runs behind a Cloudflare tunnel with bearer
   auth over Streamable HTTP; md-mcp has no network surface.
2. **The maintainer wants stable, well-maintained libraries used actively** where
   one genuinely fits, and hand-rolling only where libraries are unmaintained or a
   poor fit (e.g. byte-preserving YAML, which no serde YAML crate can deliver).

The library landscape was surveyed against current (2026-06) crates.io / maintenance
status before selection.

## Decision

### Language, edition, toolchain

We will implement md-mcp in **Rust**, edition **2024**, resolver **3**, with the
toolchain pinned via `rust-toolchain.toml` to **1.95.0** (`rustfmt`, `clippy`).
Rust is justified on real grounds: a single self-contained static binary with
trivial deployment, a no-GC long-running process, and maintainer fit (systems
background). The Obsidian-flavored-parser risk that strata had to weigh does
**not** apply here — md-mcp is pure Markdown, so the parser is small and
hand-rollable with low risk.

### MCP SDK and transport

We will use **rmcp 1.8** (the official Rust MCP SDK, foundation-backed, ~13.5M
downloads) with **stdio transport only**:
`features = ["server", "macros", "schemars", "transport-io"]`,
`default-features = false`. Tools are registered with the `#[tool_router]` /
`#[tool]` / `#[tool_handler]` macros; request and output structs derive
`serde` + `rmcp::schemars::JsonSchema` (consumed via rmcp's re-export to avoid
version skew); structured output is emitted with the `Json<T>` wrapper so each
tool advertises an `outputSchema` and returns `structuredContent`
([tool_spec §4 structured output](../tool_spec.md)). We will **not** depend on
axum / tower / an HTTP transport / a bearer-auth layer — those exist in strata
only because it is an HTTP service.

Because stdout is the JSON-RPC channel under stdio, **all logging goes to stderr**
(`tracing_subscriber` writer = stderr); any stray stdout write corrupts the
protocol stream.

### Workspace structure

We will lay out a single Cargo workspace, members under `crates/`:

- **`md-core`** — a library crate holding all pure logic (vault path jail, atomic
  write, document/section parser, frontmatter, content hashing, transaction
  engine). It has **zero async / MCP / tokio dependencies**, so it is unit-testable
  without a runtime. This boundary is load-bearing and is preserved as the crate
  grows.
- **`md-server`** — the binary crate: the rmcp stdio server and the tool wiring.
  Tool bodies stay thin and call into `md-core`. The in-process concurrency lock
  (readers-writer around the commit step) lives here, keeping `md-core` sync.

Shared metadata lives in `[workspace.package]`; dependency versions are pinned
once in `[workspace.dependencies]` and referenced with `dep.workspace = true`;
lints (`rust` `unsafe_code = "forbid"`, `clippy::all = "warn"`) are centralized in
`[workspace.lints]`. Internal crates set `publish = false`.

### Core library set

| Concern | Library | Notes |
|---|---|---|
| MCP transport / tools | `rmcp` 1.8 (stdio) | official SDK; `Json<T>` structured output |
| Serialization | `serde`, `serde_json` | request/output structs; frontmatter as `serde_json::Value` |
| YAML frontmatter (parse) | `serde-saphyr` `=0.0.28` | modern YAML-1.2 deserializer; **parse/validate only** |
| YAML frontmatter (write) | hand-rolled splice | byte preservation; see [ADR-0004](README.md) |
| Content hashing | `blake3` | per-section `content_hash`; see [ADR-0005](README.md) |
| Path jail + atomic write | `cap-std`, `cap-tempfile` | `openat2(RESOLVE_BENEATH)` — TOCTOU-free; see [ADR-0006](README.md) |
| Glob filter | `globset` | `list_notes` (`literal_separator(true)`) |
| Search | `aho-corasick`, `memchr` | linear scan; see [ADR-0010](README.md) |
| Unicode NFC | `unicode-normalization` | path / heading comparison |
| Errors | `thiserror` (core), `anyhow` (server) | |
| Async + logging | `tokio`, `tracing`, `tracing-subscriber` | server only |
| Test / bench | `criterion`, `tempfile`, `insta`, `proptest` | see [ADR-0012](README.md) |

`serde-saphyr` is pinned exactly because it is pre-1.0; it is confined to the
read/validate path, so a regression there cannot corrupt a note (writes never go
through it). Upgrades are gated behind round-trip tests.

## Consequences

- Positive: one static stdio binary, trivial to register with a desktop MCP
  client; a clean `core`/`server` split that keeps the logic runtime-free and
  testable; one pinned source of truth for versions and lints; stable,
  maintained libraries everywhere a library fits.
- Negative: rmcp is a fast-moving 1.x (pin the minor, read changelogs before
  bumping); the stdio stdout-pollution foot-gun must be guarded; `serde-saphyr`
  is pre-1.0 (mitigated by exact pin + containment to the read path);
  byte-preserving frontmatter is hand-rolled (a correctness surface, unavoidable
  given the spec).
- Neutral: choosing rmcp's stdio transport pulls in `tokio` for the server;
  `md-core` stays free of it. The full library set is pinned now but added to each
  crate's manifest as each subsystem is implemented.

### Considered and rejected

- **TypeScript** (`@modelcontextprotocol/sdk` + remark/mdast) — richer markdown
  ecosystem, but md-mcp's parser is small (pure Markdown) so that edge is moot,
  and the single-binary / no-GC / maintainer-fit / strata-reuse benefits win.
- **HTTP transport + bearer auth (axum/tower)** — strata needs it as a remote
  service; md-mcp is local stdio, so it is pure cost.
- **`serde_yaml` / `serde_yaml_ng` / `serde_yml`** — deprecated / quiet /
  RUSTSEC-flagged respectively, and serde YAML round-tripping reorders keys and
  requotes, destroying the byte-preservation the spec demands.
- **`comrak` for the section parser** — strata uses it (and hand-rolls section
  spans anyway); for md-mcp's ATX-only scope a hand-rolled code-fence-aware
  scanner is simpler, avoids the dependency, and sidesteps comrak's Setext
  recognition that md-mcp must exclude. See [ADR-0003](README.md).
- **`tantivy`** for search — an embedded search engine is overkill at personal-vault
  scale; a linear scan with `aho-corasick` meets the spec (BM25 is optional).
- **`divan`** for benchmarks — no file output, so it cannot satisfy the
  `.local/` benchmark-archive requirement.
