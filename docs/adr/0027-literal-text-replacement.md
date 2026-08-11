# 27. Literal text replacement (`replace_text`)

## Status

Accepted

Revises the "no text-match editing" axis of `docs/tool_spec.md`
§설계 우선순위 2, which otherwise stands: section editing remains the path for
structural change.

## Context

Every content edit currently goes through `edit_sections`, whose unit of
rewrite is a whole section. Fixing one typo therefore costs three calls and
two full copies of the target text:

1. `read_outlines` — to learn the section's `heading_path`,
2. `read_sections` — to see the section's current bytes,
3. `edit_sections` `replace` — to send those bytes back, one character changed.

Measured over the production vault (259 notes): median note 12.5 KB, p90
31.8 KB; median leaf section 425 B, p90 1.4 KB, largest 46 KB. So the typical
one-character fix moves roughly a kilobyte of unchanged prose in each
direction, plus the note's outline — and the largest notes, where an agent is
least able to afford a full read, are penalized worst. The edit's actual
information content is a few bytes: *this string becomes that string*.

The spec excluded text-match editing to keep a single editing path. That
reasoning holds while every edit is structural. It does not hold for the
smallest and most frequent class of edit — a typo, a renamed term, a corrected
link — where the agent already knows the exact bytes to change and the
heading address is pure overhead: the `find` string *is* the address.

## Decision

We will add **`replace_text`**, a destructive, all-or-nothing batch tool that
substitutes literal strings inside note bodies.

- **Literal, byte-exact matching.** `find` is matched against the note's
  LF-normalized source verbatim — no regex, no case folding, no CJK gap
  folding, no Unicode normalization. What the caller sees is what gets
  replaced, and the replaced span is never in question.
- **Match count is the safety mechanism**, since a literal needle has no
  natural bound. Exactly one of three contracts applies per item:
  - `expected_count: n` — the batch is rejected unless the item matches
    exactly `n` times; all `n` are replaced.
  - `replace_all: true` — every match is replaced; zero matches is a rejection.
  - neither (default) — the item must match **exactly once**; two or more
    matches reject with `AMBIGUOUS`, zero with `NOT_FOUND`.
- **Bodies only.** The search span is `Document::content_span`, so frontmatter
  is never touched (it stays `edit_properties`' domain) and an optional
  `heading_path` + `scope` narrows the search to one section — the same
  addressing, and the same `expected_hash` optimistic-concurrency tag, as
  `edit_sections`.
- **Original-snapshot semantics.** Every item in a batch resolves its match
  spans against the unedited note, and intersecting spans reject the batch
  with `OVERLAP`. No item ever sees another item's output, so a batch's result
  does not depend on item order.
- **The response reports positions, not prose**: per item the replacement
  count plus the first few changed lines (line number and post-replacement
  text, both bounded). `dry_run: true` returns exactly that without writing.
- One new error code, `COUNT_MISMATCH`, for a violated `expected_count`.

## Consequences

- Positive: a typo fix becomes one call carrying only the two strings —
  roughly two orders of magnitude less payload than read-outline +
  read-section + write-section on a median note, and the gap widens with note
  size. A `search_notes` hit is directly actionable: path plus the misspelled
  text is a complete edit, with no addressing round-trip.
- Positive: unique-match-by-default makes the dangerous case (a short needle
  matching far more than intended) a rejection rather than a silent mass
  rewrite, and `dry_run` shows the blast radius before it happens.
- Negative: a second editing path for content. The boundary is behavioural,
  not merely stylistic — `replace_text` cannot change document structure
  (headings live in the body text it edits, but nothing validates heading
  levels), so structural change must keep going through `edit_sections`. The
  tool description carries that boundary.
- Negative: byte-exact matching is stricter than `search_notes`, which
  NFC-normalizes and folds CJK gaps. A hit found in an NFD-authored note, or
  spanning inline markup (`**전역** 지침`), will not match `find`. This is a
  deliberate asymmetry — search may guess, an edit may not — but it means
  search hits are not universally replaceable, and the tool description says
  so.
- Neutral: `edit_sections` is unchanged; existing callers keep working.
  Adding a `mode: literal|regex` field later would extend the contract without
  breaking it.

### Considered and rejected

- **Regex matching.** Adds a dependency and hands a model an unbounded blast
  radius inside an all-or-nothing multi-file batch. The safety property that
  matters here is counting matches, which literal matching already provides;
  expressiveness is not what makes a typo fix cheap.
- **A `replace` operation inside `edit_sections`.** Shares almost no fields
  with the existing operations, and keeps `heading_path` required — the
  addressing round-trip is precisely the cost being removed.
- **Vault-wide find/replace by glob.** One call with an unbounded target set,
  the mistake being hardest to undo exactly when it is largest.
  `search_notes` plus a 100-item batch covers the same ground with every
  target named.
- **Reusing the search-side CJK/NFC folding.** A folded match spans bytes the
  caller never wrote, so "what exactly was replaced" stops being answerable
  from the request alone — unacceptable for a destructive operation.
- **Returning the rewritten note.** Reinstates the payload the tool exists to
  avoid; line numbers plus counts are enough to verify the edit landed.
