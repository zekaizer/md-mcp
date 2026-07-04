# 18. Git automation: per-batch auto-commit, debounced push, interval sync

## Status

Accepted

Decides the automation that [ADR-0016](0016-git-sync-integration.md)
explicitly deferred. ADR-0016's mechanisms (the git CLI driver, the
per-operation flock, the guard discipline, conflict-as-result) are unchanged
and reused.

## Context

With only the explicit `sync_vault` tool, replication happens when an agent
thinks to call it. For a vault that is the durable home of an agent's memory,
the operator may want hands-off replication: every change committed as it
lands, pushed shortly after, and remote changes pulled in periodically —
without any agent cooperation.

ADR-0016 deferred this because per-batch git spawning has a latency cost and
fine-grained history is noisier. Both are now accepted trade-offs to be chosen
by the operator, not hardcoded: each automation layer is a separate opt-in on
top of `MD_GIT_SYNC=1`.

## Decision

We will add three independent automation layers, each enabled by its own
environment variable and all requiring `MD_GIT_SYNC=1` (setting one without it
is a startup error, not a silent no-op):

### `MD_GIT_AUTO_COMMIT=1` — one write batch, one commit

After every successful mutating tool call, while **still holding the write
guard** and under the cross-process flock, run `git add -- <touched paths>`
followed by `git commit`. Key properties:

- **Path-scoped staging, never `add -A`**: only the paths the batch actually
  touched are staged, so a concurrent external edit is never swept into an
  mcp-attributed commit. With auto-commit on, `sync_vault`'s sweep commit
  therefore contains *only* external edits — attribution falls out for free.
- A destructive batch is one commit (mirroring its all-or-nothing boundary); a
  non-destructive call is one commit over its succeeded items. A call that
  changed nothing commits nothing.
- Synchronous under the guard: the commit snapshot is exactly the batch's
  result, and ordering is trivially correct. The cost is one local
  `git add`+`commit` (milliseconds on a vault-sized repo) added to write
  latency. An async commit queue was rejected: by the time a queued commit
  runs, the file may already hold a later batch's content, corrupting
  attribution — the one thing path-scoped commits exist to provide.
- Commit message: `mcp(<tool>): <n> notes` — mechanical, greppable.
- An auto-commit failure (e.g. `user.name` unset) fails the tool call's git
  step only: the vault write has already committed and is reported as such,
  with the git error logged. The vault, not git, is the durability layer
  ([ADR-0007](0007-multi-file-transaction.md)).

### `MD_GIT_AUTO_PUSH_SECS=<n>` — debounced push

A background task pushes `n` seconds after the most recent commit, resetting
the timer on each new one (debounce), so a burst of writes becomes one push.
A non-fast-forward rejection triggers one full sync (fetch + rebase under the
guard, as in ADR-0016) and a retry; a conflict is logged and left local — the
next explicit `sync_vault` reports it to an agent. Push runs outside all
guards (it touches no working-tree file).

### `MD_GIT_SYNC_INTERVAL_SECS=<n>` — periodic sync

A background task runs the full `sync_vault` sequence every `n` seconds,
pulling remote changes in without agent involvement. Conflicts are logged and
left local, exactly as auto-push. The interval loop and the tool share one
implementation; concurrent syncs are serialized by the write guard + flock.

## Consequences

- Positive: hands-off replication with clean attribution (mcp batches vs.
  external edits end up in separate commits); each layer is independently
  opt-in; automation failures never compromise vault durability or a tool
  call's outcome.
- Negative: auto-commit adds a local git spawn to every write's latency;
  history becomes fine-grained (a commit per batch); background conflicts are
  only *logged* until something runs `sync_vault` — silent divergence is
  possible on an unattended replica until then.
- Neutral: the env-var surface grows by three variables; all default off, so
  the plain profile is untouched.

### Considered and rejected

- **Time-based dirty sweeps** (commit whatever is dirty every N seconds) —
  sweeps external editors' half-saved work and destroys attribution; the
  batch boundary is the correct commit boundary.
- **Async auto-commit queue** — see above: attribution races.
- **Auto-push per commit (no debounce)** — a network round-trip per write
  batch; bursts are the common case for agent writes.
- **Automatic conflict resolution** (theirs/ours/union) in background syncs —
  silently discarding one side of a conflict is worse than divergence;
  resolution stays with agents/humans via `sync_vault`.
