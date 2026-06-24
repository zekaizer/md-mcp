# 5. Per-section content hash

## Status

Accepted

## Context

`edit_sections` supports optimistic concurrency: a caller may pass the
`expected_hash` of the section it read, and the edit is rejected with
`HASH_MISMATCH` if the section changed underneath it
([tool_spec §4](../tool_spec.md)). The hash must be **per section, not
per file**, so that editing one section never conflicts with an untouched one,
and it must be produced identically by `read_sections` (when the agent reads) and
by `edit_sections` (when it reports the post-edit hash) so the value round-trips
without a re-read.

The spec fixes the hashed bytes precisely: a section's content for the chosen
`scope`, **LF-normalized**, with the **target heading line excluded**. It is
explicitly not an authentication hash.

## Decision

We will compute `content_hash` with **`blake3`**, encoded as 64-character
lowercase hex, over the LF-normalized bytes of a section's content span.

- **One canonical span function.** `Document::content_span(index, scope)` returns
  the exact bytes that are both *read* as a section's content and *hashed*, so the
  two can never diverge:
  - `Some(i)` + `Body` → the heading's lead body (`own_body_span`);
  - `Some(i)` + `Section` → the heading's subtree **minus its heading line**
    (`heading.span.end .. section_span.end`);
  - `None` (root) + `Body` → the preamble; `None` + `Section` → the whole body.
  The target heading line is never part of the span, so renaming a heading does
  not change its body's hash.
- **One hashing function.** `content_hash` LF-normalizes the span bytes and
  returns `blake3::hash(bytes).to_hex()`. Read-side and edit-side callers use this
  same function, so a value produced by `read_sections` matches what
  `edit_sections` recomputes.

`blake3` is chosen over SHA-256 (strata) and xxhash: it is fast, collision-strong,
stable across versions, permissively licensed, and yields a compact hex digest.
Cryptographic strength is not required here, but blake3 has it for free.

## Consequences

- Positive: section-granular hashing means non-overlapping sections (and `body`
  vs `section` of the same heading) never false-conflict; one shared canonical
  span + hash function guarantees read/edit agreement; heading renames don't
  perturb body hashes.
- Negative: the hash depends on exact LF normalization and the span definition —
  any divergence silently breaks `expected_hash`, so the canonicalization is
  centralized in one function and tested for read/edit symmetry.
- Neutral: `blake3` adds one small dependency (MSRV 1.85, well within the pinned
  1.95 toolchain).

### Considered and rejected

- **SHA-256** (strata uses it elsewhere) — fine, but slower and longer for no
  benefit here.
- **A whole-file hash** — would false-conflict on edits to unrelated sections,
  defeating the point.
