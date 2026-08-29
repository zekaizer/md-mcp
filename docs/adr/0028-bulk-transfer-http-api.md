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
- `GET /api/notes?prefix=…` — a subtree as a tar stream, one round trip. A transfer
  past a few dozen notes is refused until the caller says `confirm=true`, and
  the refusal states how many there are. The question is asked about the size a
  request actually reaches, not about whether a prefix was typed: a named
  directory can hold a whole vault and an unnamed one can hold five notes. The
  index is exempt — it moves no content and is how a caller learns the size it
  is being asked to confirm.
- `GET /api/notes?format=index` — path, content hash and size per line, so a
  caller can learn what changed without transferring content.
- `PUT /api/notes/{path}` — one note, request body verbatim.
- `POST /api/notes` — a tar stream, for writing many notes in one round trip.

We will compute the HTTP `ETag` with [ADR-0005](0005-content-hash.md)'s hash
function over the exact bytes an endpoint serves, honouring `If-None-Match` on
reads (304). It is deliberately *not* that ADR's `content_hash`, which spans a
section's body and is LF-normalized: an entity tag has to change whenever the
served bytes change, frontmatter and line endings included, or a conditional
write would accept a replacement built from a stale copy. A write that creates a note
needs no condition; a write that would replace one requires an explicit
`If-Match` — the hash the caller read, or `*` to overwrite knowingly — answering
428 when no condition was given and 412 when the one given does not hold. Those
are different mistakes with different repairs — attach a condition, versus
re-read and retry — and a caller that cannot tell them apart spends a round trip
finding out. Concurrent editing from a conversation is the normal case here, so
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

We will attach **scopes** (`notes:read`, `notes:write`, and a directory the
credential is confined to) to issued tokens and check them per operation. A
stored token record without scopes predates this decision and means full
authority over the whole vault, so sessions that are live at deployment keep
working.

Confinement is the only real boundary here rather than a discouragement: a
credential that cannot name a path cannot reach it however the request is
phrased. It confines to a path, not to a directory, so the narrowest grant is a
single note — and a credential confined to one still has a collection, which is
that note. An unqualified request from a confined credential is narrowed to what
it can see rather than refused. It bounds accidents, not intent — the
human's consent at connector authorisation is already vault-wide, and MCP can
already read every note. Containment compares path segments, never string
prefixes.

We will surface the API to models through **one MCP tool**, which answers not
with a credential but with a one-time ticket and the commands that trade it for
one. A token named in a tool's answer sits in the conversation for as long as
the conversation is kept, long after it has stopped working; a spent ticket is
dead text. The commands collect the token into a file and read it back from
there, so it reaches no command line, shell history, or process listing either.
Tickets are single-use and lapse in a couple of minutes.

A transfer token lapses quickly when idle and renews by presenting itself while
it still works, so a long job does not die halfway and a forgotten token stops
mattering within minutes. Renewal carries no second credential to protect —
holding a working token is the proof — and cannot resurrect a lapsed one, so the
idle window bounds something real. Every chain also stops at a ceiling counted
from its first token: a sliding window without one is a permanent credential in
disguise. A token issued through the ordinary authorization flow is refused
here; it renews through its refresh token, and admitting it would launder it
into an endless chain.

The tool's authority is the caller's: a request that passed the bearer guard is
the consent, so no second human approval is required. One line
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
