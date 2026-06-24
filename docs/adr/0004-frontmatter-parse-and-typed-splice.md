# 4. Frontmatter: typed parse and byte-preserving splice

## Status

Accepted

## Context

md-mcp handles YAML frontmatter in two directions ([tool_spec §4](../tool_spec.md)):

- **Read / filter** needs a *parsed object* with full type fidelity — a string
  `"123"` must stay a string, distinct from the number `123`; `true`/`null`/lists/
  maps must keep their YAML types; `search_notes` compares typed values. Broken
  YAML must be reported as `FRONTMATTER_PARSE` and never written over.
- **Write** (`edit_properties`) must **preserve unchanged bytes** (other keys,
  comments, the body) and use a **fixed serializer** for the one key it changes,
  so day-to-day edits don't churn key order or quoting in the user's vault.

No serde-based YAML serializer can satisfy the write requirement: round-tripping a
whole document reorders keys and re-quotes scalars. strata solves this by never
deserializing YAML at all (it only line-scans for tags). md-mcp cannot — it needs
the parsed object for reads and filters.

## Decision

We will split the two directions and use a different tool for each.

- **Parse (read/validate): `serde-saphyr`.** `frontmatter::parse` deserializes the
  frontmatter block into a `serde_json::Value` via
  `serde_saphyr::from_str_with_options` with `strict_booleans: true` (so YAML 1.1
  forms like `yes`/`no` stay strings — the "Norway problem"). This gives full type
  fidelity for read output and `search_notes` filters. A note with no closing
  `---` has no frontmatter; an unparseable block is `FRONTMATTER_PARSE`.
- **Write: byte-preserving line splice with a single-field serializer.**
  `set_property`/`remove_property` locate the frontmatter block via the document
  parser, copy every byte outside the edited key's lines verbatim, and:
  - emit the **one** changed field by serializing the single-entry map
    `{key: value}` with `serde_saphyr::to_string` — a tested serializer handles
    quoting and type fidelity (a string `"123"` is emitted quoted), and because
    only one key is serialized nothing is reordered;
  - replace the key's existing lines **in place** (preserving key order), or insert
    a new key just before the closing `---`, or create a `---`…`---` block if the
    note has none;
  - distinguish `value: null` (emit `key: null`) from an **omitted value**
    (remove the key) — `edit_properties`'s set-vs-remove contract.
  The whole source is LF-normalized first, so the output is all-LF.
- **Write-time invariant.** After a set, the result is re-parsed and the key's
  value is checked to round-trip to the intended value; a mismatch is an internal
  error rather than a silently corrupt write. Editing over a block that does not
  parse is rejected up front (`FRONTMATTER_PARSE`).

`remove_property` of an absent key is a no-op in the core (the server's
all-or-nothing validation rejects it with `has_property`). Removing the last key
drops the now-empty block.

## Consequences

- Positive: full type fidelity on read; byte-for-byte preservation of unchanged
  keys, comments, and body on write; a fixed, correct serializer for the changed
  field (no hand-rolled YAML quoting to get wrong); the re-parse invariant catches
  any emit edge case before it reaches disk.
- Negative: a pre-1.0 dependency (`serde-saphyr`, pinned `=0.0.28`) on the
  read/serialize path — contained, and a write can only ever fail closed (the
  invariant). The line splice assumes top-level keys and does not understand a
  nested key that lexically shadows a top-level one; acceptable for note
  frontmatter and documented.
- Neutral: the edited key is re-emitted by the serializer, so its *own* prior
  quoting/style may change (e.g. `tags: [a, b]` → block list); this is the "fixed
  serializer on change" the spec asks for, and unchanged keys are untouched.

### Considered and rejected

- **Hand-rolled YAML scalar quoting/escaping** — the error-prone part; delegated
  to `serde-saphyr`'s serializer for the single edited field instead.
- **Whole-document serde round-trip** — reorders keys and re-quotes everything,
  violating byte preservation.
