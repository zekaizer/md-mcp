# 19. Sync health: surface push failures on write responses, retry failed auto-pushes

## Status

Accepted

Builds on [ADR-0016](0016-git-sync-integration.md) (git sync driver) and
[ADR-0018](0018-git-automation.md) (auto-commit / debounced auto-push / interval sync).
It does not supersede them: the sync sequence and the automation surface are unchanged;
this ADR adds failure visibility and a recovery path.

## Context

Testbed measurements against a real GitHub remote (2026-07-04) confirmed two gaps in
the ADR-0018 automation:

1. **Push failures are invisible to the MCP client.** With dead credentials (or any
   network failure) a write tool still answers success — correctly, since the vault is
   the durability layer — but the subsequent auto-push failure surfaces only as a
   server-side `tracing::warn!`. An agent (or the human behind it) has no signal that
   its writes are accumulating locally and never reaching the remote. Only an explicit
   `sync_vault` call reports the error.
2. **A failed push has no retry path.** The auto-push task fires only on commit
   signals. After a failure it stays idle until the *next* write, an interval sync, or
   a manual `sync_vault`; a restart does not help (nothing scans the backlog at
   startup). A transient outage can strand commits locally indefinitely on an
   otherwise-idle server.

Constraints: writes must stay non-blocking (no network in the write path — measured
write latency is 19–57 ms, all local git subprocess cost), the response change must be
backward compatible for existing clients, and a deterministic failure (rebase conflict)
must not be retried in a loop it can never win.

## Decision

We will track **sync health** in the server and surface it in two ways:

**1. `sync_warning` on write-tool responses.** The server keeps the outcome of the most
recent push/sync attempt (`Option<SyncHealth>`: failure reason, ahead count, failing
since). Every write tool (`create_notes`, `append_notes`, `edit_sections`,
`edit_properties`, `rename_notes`, `relocate_notes`, `delete_notes`) gains an optional
`sync_warning` string field, present **only** when the last attempt failed — e.g.
`"3 local commit(s) not on the remote (git push failed: …; failing for 84s)"`. Absent
(and omitted from JSON) when healthy, so the change is additive and invisible to
clients that predate it. `run_sync` records failure (execution error or conflict) and
clears on success, so the manual tool, the interval task, and the auto-push fallback
all feed the same state. Reads do not carry the field: the write path is where an agent
decides whether its data is safe.

**2. Auto-push retry with capped exponential backoff, and a startup nudge.** After a
failed push whose fallback sync also fails, the auto-push task arms a retry timer
(15 s doubling to a 15 min cap) instead of going idle; a fresh commit signal resets the
backoff and takes priority. A rebase **conflict is not retried** — it is deterministic
until the local or remote history changes, and the next write or interval sync
re-attempts anyway. On startup the task receives one synthetic commit signal, so a
backlog stranded by a previous run (or crash) pushes after the debounce without waiting
for new activity.

## Consequences

- Positive: an agent writing through a broken remote now sees the divergence on its
  next write response and can call `sync_vault` (or tell the human) instead of
  discovering the gap much later; transient outages self-heal without new writes; a
  restart drains the backlog.
- Positive: still no network in the write path — `sync_warning` is a lock-guarded read
  of in-memory state populated by the background tasks.
- Negative: seven response schemas grow an optional field, and the health state is one
  more piece of shared server state to keep consistent (single set/clear point in
  `run_sync` mitigates).
- Negative: the retry timer means a dead remote is probed every 15 min forever;
  acceptable traffic, and the log keeps one warn per attempt.
- Neutral: `sync_vault`'s response shape, the ADR-0018 debounce semantics, and conflict
  handling (abort, report, leave local) are unchanged.

### Considered and rejected

- **MCP logging notifications** for push failures — delivery and display are
  client-dependent (claude.ai ignores them); a response field reaches every client.
- **Failing the write** when the remote is unreachable — wrong layer: the vault write
  did succeed and durability is local-first by ADR-0016.
- **Retrying conflicts on the backoff timer** — burns a fetch+rebase round trip
  every interval on a failure that cannot resolve itself; the conflict is already
  surfaced via `sync_warning` and `sync_vault`.
- **A separate `sync_status` tool** — requires the agent to know to poll; the warning
  must reach agents that are unaware of sync entirely.
