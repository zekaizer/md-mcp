# 28. Byte-level note I/O over HTTP

## Status

Accepted

## Context

Every byte that moves through MCP crosses the model's context twice: the client
reads a note into the conversation (input tokens), then writes it back out
inside a tool-call argument (output tokens). For a note the model is supposed to
see, that is the point. For a large note it is pure overhead: a 200 KB note
edited by a script pays its full length twice for a change the model never
needed to read, and the section tools only thin the reading half — a write still
carries content. For bulk work it is worse than expensive; five hundred notes
cannot be pulled into a context window to be counted, grepped, or rewritten.

The clients that reach this server increasingly have a shell and a filesystem of
their own. Those clients can hold and process bytes the model never needs to
read, but only if the server offers a surface shaped for bytes rather than for
conversation. The MCP envelope is not that surface: JSON-RPC over an established
session, content JSON-escaped inside tool arguments, responses framed as
server-sent events ([ADR-0013](0013-http-transport.md)).

An earlier shape of this decision centred on bulk transfer: tar streams in both
directions, a renewable transfer token. Review kept finding holes exactly in
that machinery — archive-entry validation, partial-success status semantics, a
token renewal chain — and the primary caller turned out to be a model working on
*one large note*, not a migration. This records the hardened shape: the primary
case is one note, and bulk is that case in a loop.

Four constraints bound any design here:

- No request body limit is configured anywhere in the server, so axum's 2 MiB
  default is in force on every route.
- Filenames arrive in a different Unicode normalization than a caller types.
  md-core already carries NFC comparison for vault paths for exactly this reason.
- The vault is edited concurrently from a conversation while a script holds a
  copy of a note, so a blind write can silently lose the other side's edit.
- Issued OAuth tokens ([ADR-0014](0014-oauth-authentication.md)) carry no scope.
  Every token, once issued, holds the full authority of the static token, which
  is the wrong grant to hand to a sandbox.

## Decision

We will expose **`/api/notes`**, a byte-level HTTP API served beside `/mcp` and
behind the same bearer guard, whose request and response bodies are note bytes
rather than an envelope:

- `GET /api/notes/{path}` — one note, verbatim, with its entity tag.
- `PUT /api/notes/{path}` — one note, request body verbatim.
- `GET /api/notes` — an NDJSON index: path, entity tag and size per line, no
  content. It is how a caller learns what is there, what changed, and which
  tag to condition a write on. A confined credential is narrowed to its own
  subtree. The index takes no parameters, and an unknown one is refused rather
  than ignored: this API teaches through its errors, and a silently dropped
  `prefx=` would take that away exactly when it is needed.

That is the whole surface. **Bulk transfer is a loop over the single-note
endpoints, not an endpoint of its own.** Three things pay for the round trips
that costs:

- A loop already expresses it, in `curl` alone: the index supplies every path,
  and a shell iterates. No server-side convenience is needed for the job to be
  possible — only for it to be one request, which is not a goal.
- Every bulk convenience the earlier shape carried was its own defect surface,
  and the fixes this branch accumulated name them: tar entry validation
  (traversal, symlinks, absolute names, non-notes), whole-archive buffering
  behind a raised body limit, a three-way all/some/none status that `curl`
  cannot gate on. An archive parser at an authenticated boundary is a classic
  hole habitat, and the cheapest hardened parser is the one that is not there.
- The conditional write extends to bulk work for free. A tar push could never
  carry a per-entry precondition — the format has nowhere to put one — so bulk
  writes were exactly the writes the lost-update guard did not cover. A loop of
  `PUT`s with `If-Match` covers them.

We will compute the HTTP `ETag` with [ADR-0005](0005-content-hash.md)'s hash
function over the exact bytes an endpoint serves, honouring `If-None-Match` on
reads (304). It is deliberately *not* that ADR's `content_hash`, which spans a
section's body and is LF-normalized: an entity tag has to change whenever the
served bytes change, frontmatter and line endings included, or a conditional
write would accept a replacement built from a stale copy. A write that creates a
note needs no condition; a write that would replace one requires an explicit
`If-Match` — the hash the caller read, or `*` to overwrite knowingly — answering
428 when no condition was given and 412 when the one given does not hold. Those
are different mistakes with different repairs — attach a condition, versus
re-read and retry — and a caller that cannot tell them apart spends a round trip
finding out. Concurrent editing from a conversation is the normal case here, so
replacing a note is never the silent default. The reply to a write carries the
new tag, so a second edit needs no fresh `GET`.

We will make the vault, not this API, decide what a note path is: a name being
created is composed there, and a path that is not a `.md` note is refused there.
Path containment stays the jail's job
([ADR-0006](0006-vault-path-jail-and-atomic-write.md)). This API still composes
a path before comparing it against a grant's confinement, because that
comparison is a string test on the way in and has nothing to do with what is
stored.

We will **not** expose DELETE, and relocation is likewise not here. Deletion's
blast radius does not justify a second surface, and `delete_notes` already
covers it with all-or-nothing batching; `move_notes` rewrites links on the way
([ADR-0022](0022-link-rewrite-on-move.md)), which byte I/O cannot. The tool that
opens this API says both, since a caller told to work in scripts will otherwise
discover the gap by meeting a 405.

We will attach **scopes** (`notes:read`, `notes:write`, and a directory the
credential is confined to) to issued tokens and check them per operation. A
stored token record without scopes predates this decision and means full
authority over the whole vault, so sessions that are live at deployment keep
working.

Confinement is the only real boundary here rather than a discouragement: a
credential that cannot name a path cannot reach it however the request is
phrased. It confines to a path, not to a directory, so the narrowest grant is a
single note — and a credential confined to one still has an index, which lists
that note. An unqualified request from a confined credential is narrowed to what
it can see rather than refused. A delegated credential is refused on the tool
surface, which reads no scopes and would therefore hand it back everything its
confinement withheld. It bounds accidents, not intent — the human's consent at
connector authorisation is already vault-wide, and MCP can already read every
note. Containment compares path segments, never string prefixes.

This surface is an accelerator, not a replacement. The note tools remain how
work in the vault is normally done, and both the tool's description and the
server instructions say so: the cost of provisioning, redeeming and shelling out
is only worth paying when the job is a large note or a batch repetitive enough
that a shell plainly beats them. A model that reaches here for three small notes
has spent more than it saved.

We will surface the API to models through **one MCP tool**, which answers not
with a credential but with a one-time ticket and the commands that trade it for
one. A token named in a tool's answer sits in the conversation for as long as
the conversation is kept, long after it has stopped working; a spent ticket is
dead text. The commands collect the token into a file and read it back from
there, so it reaches no command line, shell history, or process listing either.
Tickets are single-use and lapse in a couple of minutes.

A transfer token lapses on a fixed TTL and is **not renewable**. The renewal
chain the earlier shape carried — a sliding idle window, a lifetime ceiling, a
predecessor grace period — was a small credential state machine defending a
convenience, and state machines on the authentication path are where this
surface grew holes. The caller it served, a human mid-job, is not the primary
caller: for a model a fresh grant is one tool call away. A token issued through
the ordinary authorization flow renews through its refresh token as before;
none of this touches it.

The tool's authority is the caller's: a request that passed the bearer guard is
the consent, so no second human approval is required. One line in the server
`instructions` names the situation that should trigger it, and a 401 body names
the call that mints a fresh token.

## Consequences

- Positive: a large note is read, rewritten and replaced without a byte of it
  crossing model context. Three handlers and one write semantics; every code a
  `curl -f` script can gate on directly. No archive parsing at the boundary.
  Every write, bulk included, can carry a precondition, closing the lost-update
  hole MCP writes leave open today. Scopes let a token be weaker than its
  parent, which the existing all-or-nothing tokens cannot express.
- Negative: N notes cost N round trips, so a whole-vault migration is minutes
  rather than seconds (`curl` reuses a connection across a loop, and
  `--parallel` exists). That is the right trade while migration stays
  occasional; if it becomes routine, this decision is wrong and should be
  superseded rather than quietly re-grown. A second public contract to keep
  compatible remains a second public contract. A minted token appears in a
  conversation transcript, which is a wider exposure than the credential store —
  bounded by its lifetime and reduced scope, not eliminated.
- Neutral: MCP remains the only surface for reads and writes the model is meant
  to see, and the only way to delete or move. Clients without shell access gain
  nothing from this API and should not call the tool.
