# 14. Authentication: a co-hosted minimal OAuth 2.1 server for the claude.ai connector

## Status

Accepted

Builds on [ADR-0013](0013-http-transport.md) (the Streamable HTTP transport and its
`MD_HTTP_TOKEN` bearer). It does not supersede it: the static bearer remains, now also
serving as the OAuth authorization gate and the Claude Code (CLI) path.

## Context

ADR-0013 made md-mcp reachable over HTTP and added an optional static bearer
(`MD_HTTP_TOKEN`). That is enough for Claude Code (CLI), which accepts a header bearer,
but **not** for the target client here: the **claude.ai web/mobile connector speaks
OAuth 2.1 or nothing** — it exposes no static-token field and sends no custom headers,
so a pre-shared token cannot reach the server. The MCP authorization spec (2025-06-18)
makes the MCP server an OAuth 2.1 **resource server** with an associated **authorization
server (AS)** that may be **co-hosted**.

We investigated delegating the AS to an external IdP (the maintainer's initial
preference — the deployment already fronts md-mcp with a Cloudflare Tunnel). The
evidence rules that out for this client:

1. **Cloudflare Access "Managed OAuth" / MCP portals break the claude.ai web/mobile
   connector** (anthropics/claude-ai-mcp #410, open 2026-06): Connect fails instantly,
   no login screen, because Cloudflare runs the OAuth handshake before proxying so the
   failure is invisible to the origin. The same URL works in Claude Code. Free vs paid
   is irrelevant — it is a flow incompatibility, not a gated feature.
2. **claude.ai web ignores a separate external AS's `authorization_endpoint` /
   `token_endpoint`** (anthropics/claude-ai-mcp #82): external-AS delegation is fragile;
   the connector works most reliably when the AS lives at the **MCP server's own
   origin**.
3. The sibling project **stratum** reached this same conclusion empirically (its
   ADR-0005) and ships a minimal **co-hosted** OAuth 2.1 AS with opaque tokens, proven
   against the live claude.ai connector — a reference to port, not reinvent.
4. rmcp's `auth` module is **client-side only** (it helps an MCP *client* authenticate);
   it offers no resource-server / AS helpers. Either path is hand-rolled on the server.

The connector supports Dynamic Client Registration (RFC 7591), authorization-code +
PKCE S256, RFC 9728 protected-resource metadata, and RFC 8414 AS metadata, and uses
port-agnostic loopback redirect matching. The deployment is a frequently-restarted
personal/staging server: the human must authorize **once** and have it survive restarts.

## Decision

We will **co-host a minimal OAuth 2.1 authorization server inside `md-server`** (no
external IdP) — the server is both resource server and AS — and **persist issued tokens
and registered clients to disk** so authorization survives restarts. We port stratum's
proven pattern.

**OAuth is enabled exactly when the HTTP transport runs with `MD_HTTP_TOKEN` set** — the
static token is the `/authorize` ownership gate. Without a token (the loopback-dev
default of ADR-0013) there is no OAuth and behavior is unchanged.

**Endpoints (unauthenticated — they *are* the auth flow), in a new `md_server::oauth`
module, mounted alongside the guarded `/mcp`:**

- `GET /.well-known/oauth-protected-resource` (and `…/mcp`) (RFC 9728):
  `{ resource: "https://<host>/mcp", authorization_servers: ["https://<host>"], bearer_methods_supported: ["header"] }`.
- `GET /.well-known/oauth-authorization-server` (RFC 8414): issuer + the three endpoints,
  `response_types_supported: ["code"]`,
  `grant_types_supported: ["authorization_code","refresh_token"]`,
  `code_challenge_methods_supported: ["S256"]`,
  `token_endpoint_auth_methods_supported: ["none"]`.
- `POST /register` (RFC 7591 DCR, `application/json`): store `redirect_uris`, return a
  generated public `client_id` (no secret).
- `GET/POST /authorize`: validate client/redirect/PKCE; the human pastes `MD_HTTP_TOKEN`
  once on a minimal HTML page (constant-time check); on success 302 to the client
  `redirect_uri` with a one-time `code` bound to (client_id, redirect_uri,
  code_challenge). Loopback `redirect_uri`s are matched port-independently via a real URL
  parser (never string prefixes), so a registered loopback callback cannot be coerced
  into leaking the code to another origin.
- `POST /token` (`application/x-www-form-urlencoded`): `authorization_code` (verify PKCE
  S256, one-time code) and `refresh_token` (rotated) grants; issues opaque `access_token`
  (~1 h) + `refresh_token` (~90 d).

This AS serves exactly one resource (`/mcp`), so we do **not** implement RFC 8707
resource indicators or token audiences — there is no second resource to disambiguate.
`/register` is unauthenticated (the connector self-registers via DCR), so stored clients
and per-registration redirect URIs are capped (oldest-evicted) to bound the state file.

**`/mcp` (the only guarded route):** the bearer layer accepts **either** a live issued
access token **or** the static `MD_HTTP_TOKEN` (the latter keeps Claude Code working).
On failure: `401` +
`WWW-Authenticate: Bearer resource_metadata="https://<host>/.well-known/oauth-protected-resource"`.
The ADR-0013 Host/Origin DNS-rebinding guards on `/mcp` are unchanged; the discovery
routes derive their public base URL from the validated `Host` header (HTTPS by posture —
TLS terminates at the tunnel).

**Tokens are opaque** (32 bytes of OS entropy, base64url), not JWTs, validated by lookup
against in-memory/persisted state. **Persistence:** registered clients and issued
access/refresh tokens are written atomically to `${MD_STATE_DIR}/oauth-state.json`
(default `$XDG_STATE_HOME/md-mcp`, else `~/.local/state/md-mcp`; mode `0600`), pruned of
expired entries on load; a load failure logs and starts empty. Short-lived auth codes
stay in memory only.

**New dependencies:** `getrandom` (entropy), `sha2` (PKCE S256), `base64` (base64url);
and axum's `form` / `query` / `json` extractor features (ADR-0013 had pinned axum to a
minimal set — the OAuth endpoints genuinely need these). No JWT/OAuth crate.

## Consequences

- Positive: the claude.ai web/mobile connector works; Claude Code keeps working via the
  static header; no external IdP, no SaaS account, no per-request network calls; the
  one-time human authorization survives restarts, then refreshes silently.
- Positive: honours the MCP spec; tokens travel only in the `Authorization` header.
- Negative: we now own security-sensitive AS code (PKCE, exact/loopback redirect
  validation, one-time codes, short-lived + rotated tokens) — mitigated by keeping it
  minimal, single-user, and audited against the spec and stratum.
- Negative: the state file holds live bearer tokens at rest (`0600`, outside the
  vault/repo); acceptable for a single-user host. Stateless (JWT) tokens would remove
  this and are deferred.
- Neutral: the ADR-0013 transport, guards, and loopback-default posture are unchanged;
  OAuth is inert until `MD_HTTP_TOKEN` is set.

### Considered and rejected

- **Cloudflare Access Managed OAuth / MCP portals** — breaks the claude.ai web/mobile
  connector (#410); CLI-only, unacceptable for the target client.
- **Cloudflare Workers OAuth Provider** — a real AS, but it requires running the AS as a
  Cloudflare Worker in front of the self-hosted binary: you still own AS logic (in JS),
  plus a Worker deploy and Worker↔origin token validation — more moving parts than
  co-hosting in `md-server`.
- **External managed IdP (WorkOS / Auth0 / Stytch / Descope)** — works for some setups,
  but adds a SaaS dependency and hits the external-AS fragility of #82; rejected for a
  single-user, no-dependency personal server.
- **Static bearer only (ADR-0013 as-is)** — fine for Claude Code, but the claude.ai
  web/mobile connector cannot send it.
- **Signed/stateless JWT access tokens** — would remove tokens-at-rest, but needs signing
  keys and rotation; deferred, opaque + persisted is simpler and single-user.
