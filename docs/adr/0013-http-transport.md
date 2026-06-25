# 13. HTTP transport (Streamable HTTP), with stdio as an option

## Status

Accepted

Revises the "MCP SDK and transport" decision of
[ADR-0002](0002-implementation-language-and-stack.md): that ADR chose **stdio
transport only** and explicitly rejected an HTTP transport (and the axum / tower
stack) on the grounds that md-mcp "has no network surface". This ADR reverses
that specific sub-decision. The rest of ADR-0002 (language, workspace layout,
core library set) stands unchanged.

It also revises the *operating premise* of
[ADR-0008](0008-concurrency-and-isolation.md) — "a single local process speaking
stdio to one client". The concurrency **mechanism** ADR-0008 chose (one
in-process `tokio::sync::RwLock`) is kept and now spans multiple concurrent HTTP
sessions; see "Concurrency across sessions" below.

## Context

ADR-0002 framed md-mcp as a local, single-vault server consumed by a desktop MCP
client over stdio, and treated "no network surface" as a fixed premise. That
premise has changed: we now want the server reachable over HTTP — so it can be
run once and shared by multiple or remote MCP clients, and so web-based clients
that speak only HTTP can use it. stdio must remain available for the
zero-configuration desktop-client case.

The MCP specification defines two network transports: the legacy HTTP+SSE
transport and **Streamable HTTP** (the current standard, single endpoint, with an
optional SSE response stream). rmcp 1.8 — already our SDK — ships a server-side
Streamable HTTP implementation behind the `transport-streamable-http-server`
feature: a `StreamableHttpService` that is a `tower_service::Service` over
`http::Request`, plus a `LocalSessionManager` for in-process session state. It
does **not** bind a socket; the application supplies the HTTP server harness.

`MdServer::serve(transport)` is already transport-agnostic (the stdio binary and
the in-process e2e test both call it with different transports), so the tool
surface and `md-core` are untouched by this change. The work is confined to the
binary: dependency features, configuration, transport selection, and an HTTP
serving path.

Making HTTP the default re-introduces a network surface that ADR-0002 was proud
to avoid. That surface — bind address, DNS-rebinding protection, authentication —
must be designed, not defaulted.

## Decision

### Transport and harness

We will add **Streamable HTTP** as a transport via rmcp's
`transport-streamable-http-server` feature, keeping `transport-io` for stdio:
`features = ["server", "macros", "schemars", "transport-io",
"transport-streamable-http-server"]`. We will serve the `StreamableHttpService`
with **axum 0.8** (the harness rmcp's own examples use), mounting it at the single
path `/mcp` with `Router::route_service("/mcp", service)` — `route_service`
(exact path) rather than `nest_service` sidesteps the `nest`/HTTP-2 `Host`-header
caveat rmcp documents, which matters because the Host header drives DNS-rebinding
validation. (axum is pulled `default-features = false` with only `http1` + `tokio`,
so the build is HTTP/1.1-only today and the HTTP-2 caveat is precautionary; the
minimal feature set also drops axum's unused extractors/json/query.) Sessions are
stateful, backed by rmcp's `LocalSessionManager`. The
service factory hands each session a cheap `MdServer::clone()`, so all sessions
share the one `Arc<Vault>` and the one readers-writer commit lock (ADR-0008) —
write serialization holds across sessions, not just within one.

### Concurrency across sessions

HTTP removes the "one client" premise: multiple clients, and multiple sessions,
are live at once. We will preserve ADR-0008's readers-writer discipline across
them by construction — the `StreamableHttpService` factory hands **every session a
clone of the one `MdServer`**, and that clone shares the same `Arc<Vault>` and the
same `Arc<RwLock<()>>`. So:

- reads (`read`-guard) from any session run concurrently;
- a commit (`write`-guard) is exclusive with every read and every other commit,
  **across sessions**, not just within one — the spec's "a multi-note read never
  observes a partially-applied batch" guarantee holds server-wide.

The factory must clone the shared server; constructing an independent `MdServer`
(hence an independent lock) per session would silently break write serialization.
An e2e test asserts the invariant: two concurrent sessions, concurrent writes that
both commit, and each session reading back the other's note.

The blocking-file-I/O-under-the-guard cost ADR-0008 already accepted is now
reached by more concurrent callers. It remains acceptable at personal-vault scale
(small notes, sub-millisecond commits, `rt-multi-thread` so reads still progress
on other workers); moving `md-core`'s synchronous I/O onto `spawn_blocking` is the
escape hatch if real contention appears, and stays deferred — md-mcp is not tuned
for high concurrency.

Cross-process isolation is unchanged from ADR-0008 and still deferred: two servers
cannot bind the same `MD_HTTP_ADDR`, but two servers on different addresses over
one vault are not prevented. Running md-mcp as a shared HTTP server makes the
single-instance file lock ADR-0008 sketched (`std::fs::File::try_lock`, zero
dependencies) more attractive; it is left to a follow-up.

### Default transport and selection

We will make **HTTP the default** transport; stdio is opt-in. Selection resolves
in this precedence:

1. CLI flag — `--http` or `--stdio` (exactly one; both, or an unknown flag, is an
   error);
2. else the `MD_TRANSPORT` environment variable (`http` | `stdio`);
3. else **HTTP**.

The flag is hand-parsed (two known tokens) — no argument-parser dependency. This
keeps the launch-time choice idiomatic as a CLI flag while still honoring the
all-env configuration style ADR-0002 established (`MD_VAULT`).

### HTTP configuration and security

HTTP behavior is read from the environment, fail-closed where it matters:

| Variable | Default | Meaning |
|---|---|---|
| `MD_HTTP_ADDR` | `127.0.0.1:7654` | bind socket address; loopback by default, `0.0.0.0:…` to expose |
| `MD_HTTP_TOKEN` | unset | when set, every request must carry `Authorization: Bearer <token>` |
| `MD_HTTP_ALLOWED_HOSTS` | unset | comma-separated `Host`-header allowlist override; `*` disables the guard |
| `MD_HTTP_ALLOWED_ORIGINS` | unset | comma-separated `Origin`-header allowlist override; `*` disables the guard |

Both allowlists share one grammar: unset = the secure default; `*` = disable;
otherwise a comma-separated list. A non-`*` value that contains no real entries
(`,`) is a hard startup error, never a silent disable — a malformed allowlist
must not fail open.

The security posture is layered:

- **Loopback by default.** The default bind is `127.0.0.1`; exposing the server is
  an explicit `MD_HTTP_ADDR` choice.
- **Two browser guards, both on by default.** rmcp validates the `Host` header
  (default allowlist `localhost`, `127.0.0.1`, `::1`) *and* the `Origin` header.
  The Host guard alone does **not** stop a malicious web page: the page can POST
  straight to `http://127.0.0.1:7654/mcp`, whose `Host` is already loopback, so the
  authoritative signal is the browser-stamped `Origin`. rmcp leaves Origin
  validation off by default, so we default `MD_HTTP_ALLOWED_ORIGINS` to the
  loopback origins for the bound port (`http://localhost:<port>`,
  `http://127.0.0.1:<port>`, `http://[::1]:<port>`). Non-browser MCP clients send
  no `Origin` and pass unaffected; a cross-site page's real `Origin` is rejected.
  A non-loopback bind sets `MD_HTTP_ALLOWED_HOSTS` (and, for browser clients,
  `MD_HTTP_ALLOWED_ORIGINS`) to the served name(s), or `*` to disable — a
  deliberate, logged choice.
- **Optional bearer auth.** Setting `MD_HTTP_TOKEN` installs an axum middleware
  that rejects any request without a matching bearer token with `401`. The header
  is parsed per RFC 7235/6750 (case-insensitive scheme, `1*SP`), the token is
  trimmed (a trailing newline from `$(cat secret)` would otherwise wedge auth
  shut), and the compare hashes both sides to fixed-width BLAKE3 digests and uses
  `blake3::Hash`'s constant-time `==` — so neither timing nor length leaks. No
  token means no auth — acceptable only because the default is loopback; a
  non-loopback bind with no token logs a warning at startup.
- **No TLS in-process.** We will not terminate TLS in md-server; an exposed
  deployment terminates TLS and may add stronger auth upstream (reverse proxy /
  Cloudflare tunnel). The bearer token travels in clear otherwise — a non-loopback
  bind logs this reminder at startup, and the docs state it plainly.

Logging stays on **stderr** for both transports — unconditional, so the stdio
stdout-is-the-protocol-channel invariant can never be violated by a config slip.

### Code shape

A new `md_server::http` module owns `router(server, &HttpConfig) -> axum::Router`
(service construction + the two guards + optional auth layer) and
`serve(server, &HttpConfig)` (bind + `axum::serve` with graceful shutdown on
Ctrl-C / SIGTERM). `Config` gains a
`transport: Transport` field (`Stdio` | `Http(HttpConfig)`); the precedence and
parsing logic are pure functions, unit-tested without touching the process
environment or a socket. `main.rs` matches on `config.transport`. The HTTP path is
exercised end-to-end by a test that drives the real axum server on an ephemeral
port with rmcp's Streamable HTTP **client**, including the auth reject/accept
paths.

## Consequences

- Positive: the server is reachable over the current-standard MCP network
  transport; one running instance can serve multiple or remote clients and
  HTTP-only (e.g. web) MCP clients; stdio still works unchanged for the desktop
  zero-config case; the `MdServer`/`md-core` boundary made this a binary-only
  change.
- Negative: we take on the axum / tower / hyper dependency tree (and reqwest,
  pulled in transitively as a dev-dependency via rmcp's
  `transport-streamable-http-client-reqwest` feature for the client test) that
  ADR-0002 deliberately avoided —
  a larger build and supply-chain surface; a real network surface now exists,
  making bind/auth/TLS our responsibility rather than the OS pipe's; rmcp's HTTP
  surface is part of its fast-moving 1.x (read changelogs before bumping).
- Neutral: HTTP sessions are stateful via `LocalSessionManager`, so server state
  is per-process (no horizontal scaling without an external session store — not a
  goal); the default port `7654` is deliberately off the crowded 8080/8000/3000
  band to cut collisions, and is overridable via `MD_HTTP_ADDR`.

### Considered and rejected

- **Keep stdio-only (ADR-0002 as-is)** — rejected because the requirement changed:
  we now need a shareable/remote and web-client-reachable endpoint.
- **Legacy HTTP+SSE transport** — deprecated in the MCP spec in favor of
  Streamable HTTP; no reason to adopt a sunset transport.
- **hyper-util-only harness (no axum)** — leaner dependency tree and rmcp already
  pulls hyper, but it needs `TowerToHyperService` + a hand-written
  accept/serve/shutdown loop; axum gives the same with `route_service` +
  `axum::serve(...).with_graceful_shutdown(...)` in a few lines and is what rmcp's
  examples exercise. Maintainability wins over a marginally smaller tree.
- **Mandatory authentication** — rejected as the default because the default bind
  is loopback, where a forced token is friction with little gain; auth is opt-in
  via `MD_HTTP_TOKEN` and is the expected companion to a non-loopback bind.
- **Environment-only transport selection (`MD_TRANSPORT` alone)** — kept as the
  fallback, but a CLI flag is the more idiomatic launch-time switch and overrides
  it; desktop clients that can only set env still work via the fallback.
- **An argument-parser crate (clap)** — overkill for two mutually exclusive
  flags; hand-parsing avoids the dependency.
