# 28. Bulk note transfer over a byte-level HTTP API

## Status

Accepted

## Context

Every byte that moves through MCP crosses the model's context twice: the client
reads a file into the conversation (input tokens), then writes it back out inside
a tool-call argument (output tokens). For one note that is the point — the model
is supposed to see it. For bulk work it is pure overhead, and past some volume it
is not merely expensive but impossible: five hundred notes cannot be pulled into
a context window to be counted, grepped, or rewritten by a script.

The clients that reach this server increasingly have a shell and a filesystem of
their own. Those clients can hold and process bytes the model never needs to read,
but only if the server offers a surface shaped for bytes rather than for
conversation.

The MCP envelope is not that surface. It is shaped for model interaction:
JSON-RPC over an established session, tool arguments as JSON strings, responses
framed as server-sent events ([ADR-0013](0013-http-transport.md)). A script moving
file content pays all of it — the content must be JSON-escaped, which inflates it;
a session must be initialised and torn down, which turns one round trip into
three. Each round trip to this deployment costs roughly 0.8 s.

Four constraints bound any design here:

- No request body limit is configured anywhere in the server, so axum's 2 MiB
  default is in force on every route. A 4 MiB body is already rejected with 413.
- Filenames arrive in a different Unicode normalization than a caller types.
  md-core already carries NFC comparison for vault paths for exactly this reason.
- The vault is edited concurrently from a conversation while a script holds a
  copy of it, so a blind write can silently lose the other side's edit.
- Issued OAuth tokens ([ADR-0014](0014-oauth-authentication.md)) carry no scope.
  Every token, once issued, holds the full authority of the static token, which
  is the wrong grant to hand to a sandbox.

## Decision

We will expose **`/api/notes`**, a byte-level HTTP API served beside `/mcp` and
behind the same bearer guard, whose request and response bodies are note bytes
rather than an envelope:

- `GET /api/notes/{path}` — one note, verbatim.
- `GET /api/notes?prefix=…` — a subtree as a tar stream, one round trip.
- `GET /api/notes?format=index` — path, content hash and size per line, so a
  caller can learn what changed without transferring content.
- `PUT /api/notes/{path}` — one note, request body verbatim.
- `POST /api/notes` — a tar stream, for writing many notes in one round trip.

We will carry [ADR-0005](0005-content-hash.md)'s `content_hash` as the HTTP
`ETag`, honouring `If-None-Match` on reads (304). A write that creates a note
needs no condition; a write that would replace one requires an explicit
`If-Match` — the hash the caller read, or `*` to overwrite knowingly — and answers
412 otherwise. Concurrent editing from a conversation is the normal case here, so
replacing a note is never the silent default, while the common push of new notes
stays a single unconditional request. A caller that just read a note already holds
its hash, so the guard costs it no extra round trip.

We will normalize incoming names to NFC before they become vault paths, so a
decomposed name from a client filesystem addresses the same note as a composed
one. Path containment stays the jail's job
([ADR-0006](0006-vault-path-jail-and-atomic-write.md)); tar entries are ordinary
paths to it.

We will raise the request body limit on the tar routes alone, leaving `/mcp` and
the OAuth endpoints at the default.

We will **not** expose DELETE. Deletion is the one operation whose blast radius
does not justify a second surface, and `delete_notes` already covers it with
all-or-nothing batching.

We will attach **scopes** (`notes:read`, `notes:write`) to issued tokens and check
them per operation. A stored token record without scopes predates this decision
and means full authority, so sessions that are live at deployment keep working.

We will surface the API to models through **one MCP tool** that mints a
short-lived, scope-reduced token and returns commands with the token and endpoint
already substituted. The tool's authority is the caller's: a request that passed
the bearer guard is the consent, so no second human approval is required. One line
in the server `instructions` names the situation that should trigger it, and a 401
body names the call that mints a fresh token.

## Consequences

- Positive: bulk moves cost no model context, and a script can process the whole
  vault. `curl` is a sufficient client, so no CLI, binary, or release channel has
  to be built or maintained. Mandatory `If-Match` closes a lost-update hole that
  MCP writes leave open today. Scopes let a token be weaker than its parent, which
  the existing all-or-nothing tokens cannot express.
- Negative: a second public contract to keep compatible for as long as it exists.
  Scopes touch the authentication path that live sessions depend on, so the
  no-scope-means-full-authority rule is load-bearing. A minted token appears in a
  conversation transcript, which is a wider exposure than the credential store —
  bounded by its lifetime and reduced scope, not eliminated.
- Neutral: MCP remains the only surface for reads and writes the model is meant to
  see, and the only way to delete. Clients without shell access gain nothing from
  this API and should not call the tool.
