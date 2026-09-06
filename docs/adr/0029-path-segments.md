# 29. Vault paths as segment arrays

## Status

Accepted

Amends the path conventions of [ADR-0006](0006-vault-path-jail-and-atomic-write.md)
(the jail is unchanged; only the wire form of a path changes) and the `dest`
suffix convention of [ADR-0024](0024-unified-move-primitive.md).

## Context

A note was created through `create_notes` with the path
`03-resources/research/디바이스 메모리 I/O 용어 체계 ….md`. The server read the
`/` inside `I/O` as a directory separator, created the folder `… 디바이스 메모리 I`
and wrote the note `O 용어 체계 ….md` inside it. No error was raised, the vault
git history recorded the split as an ordinary create, and the caller only noticed
when a later read showed the title cut in half.

The server cannot reject this. With a string path, `a/b.md` and a title
containing `/` are the same bytes; the only defence available is a sentence in
the tool description, which was added after the incident. There is no escape
form either: `/` cannot be stored in a POSIX file name under any encoding, so an
escape would only relocate the substitution into the server.

The string form also bakes the separator into the contract. The vault is jailed to
a POSIX tree today, but the wire format should not have to say which character
joins names; the server already speaks in segments for headings
(`heading_path: string[]`) and containment in the transfer API compares segments,
never string prefixes ([ADR-0028](0028-byte-level-note-api.md)).

The one place the string form was load-bearing is `move_notes`: a `dest` ending in
`/` means "into this directory, keep the basename", and without it means "this
exact path" ([ADR-0024](0024-unified-move-primitive.md)). A segment array has no
trailing slash to carry that distinction.

Changing the shape of every path the tools accept and return is a public contract
change that every connected agent must follow at once.

## Decision

**The base rule: a path is an array of segments.** Wherever the tool contract
carries a vault path — a request field, a response field, an error detail — it
is `string[]`, root to leaf: `["03-resources", "research", "note.md"]`. This
holds for every tool without a per-field list; a field that carries a path and
is not an array is a defect. The string form is removed, not kept as an
alternative — a contract that accepts both leaves the check unreachable for
whoever keeps sending strings. The two exceptions below are the only ones, and
each exists because the surface has a path syntax of its own.

We will validate each segment on its own and reject the whole path on the first
bad segment with a new `SEGMENT` error code: a segment that is empty, is `.` or
`..`, or contains `/`, `\` or NUL is refused. The message names the offending
segment. `..` stays a `TRAVERSAL` case only when it is the whole path's escape;
inside an array it is simply an invalid name. The `.md` suffix rule for notes and
the vault jail (root is never a target, no symlink escape) are unchanged.

We will carry the vault root as the empty array `[]`, valid only where a directory
is accepted.

We will split `move_notes`'s destination into two mutually exclusive fields, since
the suffix can no longer say which one is meant:

- `dest: string[]` — the full destination path including the new basename.
- `into: string[]` — a directory to move into, keeping the source basename.

A `MoveItem` with both or neither is rejected (`MISSING_CONTENT`).

Because responses follow the base rule too, a value read from one tool passes
into another verbatim. `list_notes` directory items lose the trailing `/` and
instead carry an explicit marker (`kind: "dir"`); a note item is `kind: "note"`.

We will keep exactly two surfaces as strings, because they are strings by their
own standard, not by ours: Markdown links inside note bodies (`[..](a/b.md)`,
rewritten by `update_links` per ADR-0022) and the HTTP transfer API's URL paths
(ADR-0028, percent-encoded per segment). A URL cannot carry the segment check —
a `/` in a title is indistinguishable from a path separator there too — so the
transfer API closes the same hole the other way: `PUT /api/notes/{path}` writes
only into a directory that already exists and answers `409 Conflict` when the
parent is missing (the WebDAV rule for a PUT without a parent collection,
RFC 4918 §9.7). Creating a directory is an MCP-side act, done with a segment
array that names it deliberately; the byte surface never creates one as a side
effect. Internally the server keeps joining with the platform separator; on-disk
layout, the event journal and git commit subjects are unaffected.

We will ship this as a breaking minor release (v0.10.0) with the `tool_spec.md` and
CONTEXT.md path conventions rewritten to the array form.

## Consequences

- A title containing a separator is rejected with an error naming the segment,
  instead of silently becoming a folder. The failure moves from a later read back
  to the write that caused it.
- The contract no longer states a separator character. A vault on a non-POSIX
  filesystem, or a future non-filesystem store, needs no path-syntax change.
- Every client breaks at once. There is no transition window: an agent that
  sends a string after the upgrade gets a schema rejection from the MCP layer,
  which is the intended teaching signal, but it is abrupt. The server
  `instructions` will state the array form up front.
- Paths cost more tokens in JSON (`["a","b","c.md"]` versus `"a/b/c.md"`). Listing
  a large vault gets proportionally longer; the cursor paging of ADR-0010 bounds
  each response.
- `move_notes` gains a second destination field. The single-convention simplicity
  ADR-0024 valued is traded for a form that cannot be misread.
- A transfer recipe that lays out a new subtree must create the directory
  through MCP first (any `create_notes` into it) and then `PUT` the notes; the
  byte surface alone cannot populate a fresh folder. The 409 says so in its body.
- A mistyped directory segment still creates a new directory silently; this
  decision catches separators, not typos. A `create_dirs` guard remains a
  separate, compatible follow-up if that failure occurs.

## Alternatives considered

- **Keep strings, add `create_dirs:false` by default.** Catches every unintended
  directory including this one, with a single boolean. Rejected because it makes
  the separator mistake a side effect of another rule rather than an error in its
  own right, and leaves the separator in the contract.
- **Keep strings, accept an escape for `/`.** No filesystem can store the result,
  so the server would substitute a character the caller did not ask for.
- **Accept both string and array.** The check only fires for callers who opt into
  the array; the ones who need it are the ones still sending strings.
- **Tagged destination (`dest: {path|dir: [...]}`) instead of two fields.** One
  field, but nested objects are harder for callers to get right than two flat,
  mutually exclusive keys.
