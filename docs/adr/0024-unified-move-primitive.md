# 24. Unified move primitive (rename + relocate)

## Status

Accepted

Supersedes [ADR-0009](0009-delete-recovery-and-move-validation.md). ADR-0009's
delete-recovery decision is carried forward here unchanged — only its move-tool
surface is revised.

## Context

`rename_notes` (same parent, basename only) and `relocate_notes` (new directory,
basename kept) are disjoint. A move that also changes the basename — the core action
of a vault restructure — needs two round trips ([issue #1](https://github.com/zekaizer/md-mcp/issues/1)),
and neither tool alone is atomic across the pair. Each is a special case of a
full-destination move, so keeping both alongside a combined tool would leave three
overlapping move semantics.

[ADR-0009](0009-delete-recovery-and-move-validation.md) chose the two-tool split with
"the tool surface stays minimal" as a positive consequence, and deliberately rejected
order-dependent batches. As a result an acyclic parent-then-child move — relocating a
directory and, in the same batch, moving another note under its new location — is
expressible only as two sequential calls ([issue #7](https://github.com/zekaizer/md-mcp/issues/7)).

Revising the move tools changes the public MCP tool contract and commits callers to a
destination convention — both hard to reverse once vaults and agents depend on them.

## Decision

We will replace `rename_notes` and `relocate_notes` with a single `move_notes`
primitive. Each batch item is `{ source, dest }`.

We will resolve `dest` by a single convention:

- `dest` ending in `/` targets a **directory** and keeps the source basename (the
  former `relocate_notes`); multiple sources may target one directory.
- `dest` without a trailing `/` is the **full destination path**, including the new
  basename (the former `rename_notes`, and rename-while-move in one).
- A `dest` under the same parent with a new basename is a pure rename.

We will carry ADR-0009's move-validation invariants forward unchanged: the vault jail
(`validate_rel`, root is not a target), collision rejection (`CONFLICT`, overridable by
`overwrite`), in-batch collision (`BATCH_COLLISION`), overlap and directory-into-own-subtree
rejection (`OVERLAP`), and all-or-nothing atomic commit. A cyclic swap (A→B and B→A in
one batch) stays **rejected**, not auto-sequenced through a temp name.

Because every item names an absolute destination, an acyclic parent-then-child batch is
now well-defined and permitted: the batch declares a final tree, which is validated
collision-free and applied as one move-map. Ordering is a property of the declared
final state, not something the caller sequences — so this admits [issue #7](https://github.com/zekaizer/md-mcp/issues/7)
without reintroducing the rejected temp-name swap sequencing.

We will preserve the existing batch options and reporting: `dry_run`, `overwrite`,
`prune_empty`, `update_links` ([ADR-0022](0022-link-rewrite-on-move.md)), and the
`relinked` / `pruned` reports.

We will re-affirm ADR-0009's delete-recovery model (`delete_notes` moves targets to
`.md-mcp/trash/`) unchanged; it is independent of this decision. We will amend the
CONTEXT.md tool inventory to match.

## Consequences

- Positive: renaming while moving is one atomic call instead of two; three overlapping
  move semantics collapse to one primitive under one convention, and the tool surface
  drops by one (`rename_notes` + `relocate_notes` → `move_notes`); acyclic
  parent-then-child restructures become expressible atomically, subsuming issue #7.
- Negative: a breaking change to the public MCP contract — callers of `rename_notes` /
  `relocate_notes` must migrate to `move_notes`, requiring a version bump and a
  migration note. Callers now build the full destination path, losing `rename_notes`'s
  structural "no `/` in the new name" guard; the trailing-`/` convention and vault-jail
  validation mitigate this, but a mistyped parent path can still land a note in an
  unintended — yet valid — directory.
- Neutral: `relocate_notes`'s keep-the-basename ergonomics survive as the trailing-`/`
  convention rather than a separate tool; cyclic swaps remain rejected as before.

### Considered and rejected

- **Additive 13th tool** (keep `rename_notes` + `relocate_notes`, add `move_notes`) —
  pays the surface cost and keeps three overlapping semantics; it contradicts the
  minimal-surface value without removing the redundancy.
- **Status quo** (two-call renaming move) — keeps the contract stable but leaves the
  central restructure action non-atomic and doubles the round trips.
