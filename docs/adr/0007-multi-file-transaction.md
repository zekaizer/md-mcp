# 7. Multi-file transaction: journal, backup, and crash recovery

## Status

Accepted

## Context

Destructive batches (`edit_sections`, `edit_properties`, `delete_notes`,
`rename_notes`, `relocate_notes`) are all-or-nothing across **multiple files**:
either every change lands or none does, even if the process crashes mid-batch
([tool_spec §4](../tool_spec.md)). strata delegates this to an out-of-process git
daemon (`git reset --hard`); md-mcp has no git and must guarantee it itself.

A bulk rename of N files is not atomic (N separate `rename(2)` calls), so a crash
can leave some files changed and some not. We need a journal that lets recovery
restore a consistent state, plus a definition of what "consistent" is. The spec
fixes it: an interrupted batch rolls back to **no effect**.

## Decision

We will implement a **write-ahead undo journal with rename-based backups**, in
`md-core::transaction`, operating through the vault's `cap-std` `Dir`. Internal
state lives in a hidden `.md-mcp/` directory under the vault root (journal,
backups, trash), excluded from listings.

A batch is a list of primitive ops — `Write{path, content}` (overwrite an existing
note), `Delete{path}` (to trash), `Move{from, to}`. Commit runs:

1. Write a journal record `{batch_id, committed: false, undo: []}` and fsync it.
2. For each op, **record the undo step in the journal (fsync) *before* mutating**,
   then perform the mutation — moving any displaced original *aside by rename*
   (cheap, works for directories) rather than copying:
   - create → undo `DeletePath`; overwrite → rename old to `backup/<id>/`, undo
     `RestoreFromBackup`; delete → rename to `trash/`, undo `RestoreFromTrash`;
     move → undo `ReverseMove` (and back up a displaced destination).
3. On success, set `committed: true` (the commit point), then delete the backups
   and journal.
4. On any op failure, apply the recorded undo steps in reverse and remove the
   journal — the batch has no effect.

**Recovery** runs at `Vault::open`: every journal that is not `committed` is rolled
back by replaying its undo steps in reverse; a `committed` journal (crash between
commit and cleanup) is just cleaned up. Undo steps are **idempotent and tolerant**
— a missing backup/trash/target is a no-op — so recovery is safe whether the
crash happened before or after a given mutation.

The invariant: a batch is either fully committed or rolled back to no effect,
never partial. Rolling back a batch that finished applying but had not yet been
marked committed is acceptable — the caller received no success response, so
no-effect is a valid all-or-nothing outcome.

Non-destructive tools (`create_notes`, `append_notes`) are partial-success and do
**not** use the transaction; each item is an independent single-file atomic write
([ADR-0006](0006-vault-path-jail-and-atomic-write.md)).

## Consequences

- Positive: real multi-file atomicity with crash recovery, no external daemon;
  rename-based backups are cheap and handle directories; tolerant undo makes
  recovery correct regardless of where a crash struck.
- Negative: a journal fsync per op (the cost of durability); the backup/trash
  living in-vault under `.md-mcp/` must be excluded from every listing and never
  be a traversal target.
- Neutral: `batch_id` uses `SystemTime` + a process counter; recovery is keyed on
  the `committed` flag, the single commit point.

### Considered and rejected

- **Roll-forward (redo) journaling** — would require staging all new content
  durably and completing renames on recovery; more complex than rolling back to
  the pre-batch state, which is exactly the spec's "no effect" outcome.
- **Content-copy backups** — expensive and awkward for directory moves; rename
  aside is atomic and O(1).
