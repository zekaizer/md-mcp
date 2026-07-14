# 25. Insertion placement follows authoring intent

## Status

Accepted

## Context

`append` inserts at the end of the addressed content span, and
`insert_before`/`insert_after` insert exactly at section boundaries. Those
boundaries include the blank lines that separate a section's last text from
the next heading, so appended text lands *after* the separator and against the
following heading (`lead\n\nmore\n## B`) — the opposite of what an author
writing "append to this section" produces — and inserted sibling sections glue
to both neighbours. `move` already refuses to glue: it keeps a blank line at
both insertion seams. The placement of inserted bytes is client-visible tool
contract; agents build edits against it, so it must be settled, not tuned
case by case.

## Decision

We will treat `append` as extending the section's text flow: when the
addressed span is followed by a heading, the insertion point backs up over the
span's trailing whitespace-only lines to sit directly after the last non-blank
line, leaving the boundary blank lines as the separator. A span ending at EOF
keeps the literal end — there is no boundary to preserve, and `append_notes`
already means "literal end" there. Append neither adds nor removes separator
blank lines: a document that glued its headings stays glued.

We will treat `insert_before`/`insert_after` as placing a sibling block: the
position stays exactly at the section boundary, and the seam rule `move`
already applies — keep a blank line at both seams, only ever adding — is
shared with them.

The asymmetry is the point: `append` joins text *inside* a section;
`insert_*` and `move` place *blocks* between sections.

## Consequences

- Positive: appended text reads as a continuation of the section; inserted
  and moved sections are uniformly blank-line-separated; the seam logic is
  one shared implementation instead of a `move`-only special case.
- Negative: byte-level output changes for callers that pinned the old
  placement; whitespace-only-line detection is one more lexical rule in the
  patch layer.
- Neutral: spans, `content_hash`, overlap footprints, and the wire schema are
  unchanged — only the insertion offset and seam padding move.

### Considered and rejected

- **Blank-line seams for `append` too** — appending prose would then always
  open a new paragraph; "continue this section" is the common agent intent.
- **Trimming the trailing blank run for `insert_*` as well** — with seam
  padding both placements converge on the same output for the normal
  one-blank-line case, and keeping the boundary anchor preserves the
  zero-width footprint that lets an insert coexist with a body edit of the
  same heading in one batch.
- **Leaving `append` as span-end insertion** — simplest, but every appended
  paragraph hugs the next heading, which no author writes and linters flag.
