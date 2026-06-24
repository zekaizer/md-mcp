# 3. Document parser and section model

## Status

Accepted

## Context

md-mcp addresses notes by **section** — a heading plus everything beneath it —
identified by a **heading path** with an **occurrence** disambiguator
([tool_spec §용어 정의](../tool_spec.md)). To read or edit a section, and to
compute its `content_hash`, the server needs each heading's exact **byte span** in
the source, derived from a heading model that:

- recognizes only **ATX** headings (`#`..`######`); Setext is out of scope;
- ignores `#` inside fenced or indented code blocks;
- compares heading text after Unicode **NFC** normalization, case-sensitively;
- works in **bytes** (not chars), so multi-byte (e.g. Korean) content does not
  corrupt a splice.

strata locates headings by running them through `comrak` and trusting only its
1-based line numbers, then recomputing byte offsets itself — precisely because
`comrak`'s column unit is a byte-vs-char hazard. But `comrak` also recognizes
Setext headings (which md-mcp must exclude) and is a heavy dependency used here
only to classify lines.

## Decision

We will **hand-roll** the document parser as a single code-fence-aware line scan
over LF-normalized source, in `md-core::document`. No markdown library.

- **One pass, byte-accurate.** The scanner walks lines (byte spans split on `\n`),
  tracking fenced-code state (` ``` `/`~~~`, open and close with ≤3-space indent),
  and recognizes a heading only outside a fence, at ≤3-space indent, as
  `#`×(1–6) followed by a space/tab or end of line (so `#tag` is not a heading).
  The ATX closing `#` sequence is stripped. Heading text is stored verbatim;
  byte spans come straight from the scan, so a splice never miscounts a multi-byte
  character. Indented (≥4-space) `#` lines are never headings, which subsumes the
  indented-code case without full paragraph analysis.
- **Frontmatter.** A leading line of exactly `---` with a later closing `---`
  delimits the frontmatter span; an unterminated leading `---` is body, not
  frontmatter. Heading/fence scanning starts after the frontmatter.
- **Section spans.** From the flat heading list we derive, as byte spans:
  `section_span` (heading start to the next same-or-higher heading — the subtree),
  `own_body_span` (heading line end to the next heading of any level — the lead
  body), `preamble_span` (frontmatter end to the first heading — the root body
  before any heading), and `whole_body_span` (frontmatter end to EOF — the root
  as one section). These are exactly the `body`/`section` scopes and the empty
  heading-path root the spec defines.
- **Addressing.** `resolve_heading(path, occurrence)` matches when the requested
  path is an NFC **suffix** of a heading's ancestor chain (so a different parent
  disambiguates automatically); `occurrence` (1-based, document order) is the
  fallback only for identical full chains. No match → `NOT_FOUND`; many + no
  occurrence → `AMBIGUOUS`. `outline()` returns every heading with its
  `heading_path`, `level`, `line`, `occurrence`, and `ambiguous` flag.

## Consequences

- Positive: no markdown dependency; exact control over the ATX-only, code-fence,
  NFC, and byte-offset rules the spec requires; Setext is excluded by
  construction; spans are byte-accurate for surgical edits.
- Negative: we own the CommonMark edge cases we choose to honor (fenced/indented
  code, ATX closing sequences) and must test them; full CommonMark block parsing
  is deliberately not implemented (out of scope for a notes vault).
- Neutral: the parser assumes LF-normalized input (the read path normalizes
  before parsing; writes are always LF), keeping spans and heading text free of
  stray `\r`.

### Considered and rejected

- **`comrak` (strata's approach)** — heavy, recognizes Setext (which we exclude),
  and used only to classify lines that a small scanner classifies directly.
- **`pulldown-cmark` `into_offset_iter`** — exposes byte ranges and is a viable
  backstop, but still pulls a full CommonMark parser for an ATX-only need; kept in
  reserve if a hand-rolled edge case proves troublesome.
