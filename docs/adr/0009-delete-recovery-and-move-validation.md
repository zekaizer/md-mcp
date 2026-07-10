# 9. Delete recovery model and move validation

## Status

Superseded by [ADR-0024](0024-unified-move-primitive.md)

## Context

`delete_notes` must be recoverable, not a permanent erase
([tool_spec §3](../tool_spec.md)). The spec floats, as a *separate* decision,
whether to add trash-list / restore tools and whether to make overwrite/edit
recover a previous version. Separately, `rename_notes` and `relocate_notes` must
reject unsafe or order-dependent batches (suffix violations, collisions, swaps, a
directory moving into its own subtree, overlapping targets) — the §4 "no
overlapping targets" rule.

## Decision

### Delete recovery

`delete_notes` moves each target to a recoverable **trash** under `.md-mcp/trash/`
(via the transaction engine, [ADR-0007](0007-multi-file-transaction.md)), mirroring
the original path with a numeric suffix on collision so an earlier trashed copy is
never clobbered. The response reports each item's `trashed_to` location.

We will **not** add trash-list or restore tools in this iteration, and **not** add
previous-version recovery for overwrite/edit. The tool surface stays at the
specified twelve; a trashed note is recoverable by its reported path out of band.
A dedicated restore/history surface is a future decision if the workflow needs it.
(Transaction backups, distinct from the trash, exist only for the duration of a
batch and are deleted on commit — they are not a user-facing history.)

### Move validation (rename_notes / relocate_notes / delete_notes)

All three are destructive and all-or-nothing; the whole batch is validated before
any move:

- **Path safety** — every input and computed destination passes the vault jail
  (`validate_rel`); the vault root is not a valid target.
- **Suffix rules** — `rename_notes`: `new_name` has no `/`; a note keeps `.md`, a
  directory's new name has neither `.md` nor `/`. `relocate_notes`: `dest_dir`
  ends with `/`; the source basename is preserved.
- **Collisions** — a destination that already exists is rejected unless
  `overwrite` is set (`CONFLICT`).
- **In-batch collisions** (`BATCH_COLLISION`) — the same source twice, the same
  destination twice, or one item's destination equal to another's source (an
  order-dependent swap) reject the whole batch.
- **Overlap** (`OVERLAP`) — two targets in an ancestor/descendant relationship, or
  a directory relocated into its own subtree, are rejected.

Only an all-valid batch is committed; the transaction makes the moves atomic.

## Consequences

- Positive: deletes are recoverable with no data loss and no clobbering; the move
  tools cannot half-apply an order-dependent batch or escape the vault; the tool
  surface stays minimal (twelve tools).
- Negative: recovery from trash is manual (no restore tool yet); overwrite/edit
  have no built-in undo beyond the in-batch transaction.
- Neutral: swaps are rejected rather than auto-sequenced through a temp name — the
  agent can stage a swap explicitly if needed.

### Considered and rejected

- **Trash-list + restore tools now** — widens the surface beyond the spec's twelve
  for a workflow that has not asked for it; deferred.
- **Auto-sequencing swaps via a temp name** — implicit reordering of a destructive
  batch; rejecting is safer and clearer.
