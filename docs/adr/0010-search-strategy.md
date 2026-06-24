# 10. Search and listing strategy

## Status

Accepted

## Context

`list_notes` enumerates notes by directory/glob; `search_notes` matches content,
filename, and frontmatter fields, returns snippets, and pages results
([tool_spec §2](../tool_spec.md)). BM25 ranking is explicitly *optional*, and a
personal vault is small (well under 10k notes). strata uses `tantivy`, a full
embedded search engine — overkill here, and md-mcp drops the semantic/embedding
features that motivated it.

## Decision

We will implement listing and search with a **linear scan**, no search-engine
dependency.

- **list_notes** is served by `Vault::list_entries` ([listing](../adr/README.md)):
  a sorted, dot-directory-excluded walk with a `globset` filter, plus opaque
  cursor paging in the server (the cursor is the last returned path; results are
  path-sorted so paging never duplicates or skips).
- **search_notes** scans note bodies with **`aho-corasick`**: query keywords are
  matched case-insensitively as a multi-pattern automaton, and a document matches
  when **every** keyword is found (whitespace-AND). `filename` mode is a
  case-insensitive substring over the path; `both` is the union. Frontmatter
  filters (`frontmatter`, `frontmatter_exists`) are applied to the parsed
  frontmatter object (scalar exact-match, list contains, key present/absent), all
  AND-combined. A snippet is built from the first match's line with
  `context_lines` of surrounding context. Results are path-sorted with cursor
  paging.
- **BM25 ranking is deferred.** Term frequencies are available for free from the
  automaton if ranking is later wanted; only a vault outgrowing linear scan would
  justify `tantivy`, and that pivot would get its own ADR.

## Consequences

- Positive: no heavy search-engine dependency; correct, predictable matching at
  personal-vault scale; stable cursor paging; the BurntSushi crates
  (`aho-corasick`, `globset`) are std-tier and stable.
- Negative: O(vault bytes) per search with no persistent index — negligible at the
  stated scale, but a growth ceiling. Case-insensitivity is ASCII-only unless
  NFC+lowercased first (documented limitation).
- Neutral: snippet/match correctness lives in hand-rolled glue (the AND check and
  byte-offset→line mapping), covered by tests.

### Considered and rejected

- **`tantivy`** — an inverted-index engine is disproportionate for a small vault
  and the dropped semantic features; reconsider only past the linear-scan ceiling.
