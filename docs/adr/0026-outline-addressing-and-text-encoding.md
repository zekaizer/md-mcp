# 26. Outline addressing and text encoding

## Status

Accepted

Refines the `outline()` return contract in
[ADR-0003](0003-document-parser-and-section-model.md) (which otherwise stands).

## Context

`read_outlines` returned, for every heading, its full ancestor chain plus
always-present `occurrence`/`ambiguous` fields. Measured over a real vault
(156 notes, 3107 headings), 99.2% of headings have a leaf title that is unique
within their note — and `resolve_heading` already resolves unique suffixes —
so the ancestor chain carried information the resolver did not need, and
`occurrence: 1`/`ambiguous: false` never varied. Outlines cost ~450 KB
vault-wide, undermining their purpose: making outline-first reads cheap enough
for agents to use by default. The consumer is an LLM agent, not a program;
`ambiguous` also answered the wrong question ("is my full chain duplicated?")
when agents need "does the bare title suffice?".

## Decision

We will emit **minimal unique addresses** and encode outlines as **plain
text**.

- `Document::outline()` returns per heading the *shortest* suffix of its
  ancestor chain that `resolve_heading` maps back to that heading, found by
  probing `resolve_heading` itself (correct by construction, exact-vs-suffix
  shadowing included). `occurrence` is `Some` only when even the full chain is
  duplicated; the `ambiguous` flag is dropped — a multi-element address *is*
  the ambiguity signal, and it answers the question agents actually have.
- `read_outlines` returns one text block per note instead of a JSON heading
  array, following the `read_notes` content-string envelope pattern:
  - heading row: `<line right-aligned to the note's widest line number> <'#'×level> <title verbatim>`;
  - marker row (only under headings whose bare title does not resolve):
    `↳ heading_path: <JSON array>` plus ` occurrence: <n>` when needed — the
    exact values to pass to `read_sections`/`edit_sections`.
- The grammar is unambiguous by construction: titles cannot contain newlines
  (`normalize_newlines` strips `\r\n`, lone `\r`, and the parser is line
  based), so every row start is emitter-controlled; heading rows start with a
  digit, marker rows with `↳` — disjoint prefixes, no escaping, titles stay
  verbatim to end of line.

## Consequences

- Positive: vault-wide outline payload drops from ~450 KB to ~99 KB (22%),
  with the largest notes — where outlines matter most — near 37%; every
  emitted address round-trips through `resolve_heading` (property-checked over
  the full vault); addressing failure modes shrink to the marked rows.
- Negative: breaking change to the `read_outlines` output contract (minor
  version bump); level must be read from `#` count rather than a field; notes
  with repeated sibling templates (e.g. log notes) trigger many marker rows
  and compress less.
- Neutral: exact level numbers and full chains remain recoverable from `#`
  count plus document order (the same stack algorithm the parser uses);
  `resolve_heading` semantics are unchanged, so previously valid full-chain
  addresses keep working.

### Considered and rejected

- **Nested JSON tree** (34% of current): re-encodes the hierarchy `#` already
  encodes, needs a recursive schema, and hides document order/section sizes.
- **Omit-defaults JSON only** (77%): non-breaking but leaves the dominant
  redundancy — the ancestor chain — untouched.
- **Flat JSON with minimal suffixes** (45%): pays the breaking-change cost
  without the text encoding's size and legibility gains.
- **Indent/rail decoration of heading rows**: encodes level a second time;
  rejected after settling on `#` count as the single level encoding.
