# 17. Event journal and commit hook

## Status

Accepted

## Context

External tools — indexers, backup jobs, the git automation deferred by
[ADR-0016](0016-git-sync-integration.md) — need to observe vault changes.
Because every write is an atomic rename ([ADR-0006](0006-vault-path-jail-and-atomic-write.md)),
an inotify/FSEvents watcher already observes changes without ever seeing a
partial file; that channel needs no design. What a watcher cannot see is
**semantics**: which tool changed what, which changes were one all-or-nothing
batch, and what happened while the consumer was not running. That calls for a
durable, replayable event stream with a push channel on top.

Two placement constraints are fixed by prior ADRs: `md-core` is synchronous and
policy-free ([ADR-0002](0002-implementation-language-and-stack.md)), so event
emission cannot live inside it; and the transaction journal
([ADR-0007](0007-multi-file-transaction.md)) is a crash-recovery mechanism, not
a pub/sub surface — coupling event durability to it would entangle the layers.

MCP's own change-notification surface (`notifications/resources/*`) does not
apply: md-mcp is tools-only and exposes no resources; and it reaches MCP
clients, not external processes.

## Decision

We will add an **append-only event journal** at `.md-mcp/events.jsonl`, written
by `md-server` immediately after a `Vault` mutation returns success — `md-core`
stays event-free. One JSON record per line:

```
{seq, ts, batch_id?, tool, ops: [{op: "create"|"write"|"delete"|"move", path, to?}]}
```

- A **destructive batch** emits one record carrying all its ops and its
  `batch_id` — the record boundary mirrors the all-or-nothing boundary. A
  rolled-back batch emits nothing.
- Each **succeeded non-destructive item** (`create_notes`, `append_notes`)
  emits one record; failed siblings emit nothing.
- **`sync_vault` pulled changes** emit one synthetic record derived from the
  upstream diff ([ADR-0016](0016-git-sync-integration.md)), so the stream
  remains a complete account of vault changes, not merely of md-mcp's own
  writes.
- Reads emit nothing.

`seq` is monotonically increasing, recovered from the journal's last line at
startup. Each append is fsynced. **Delivery semantics: at-least-once,
best-effort complete** — a consumer may see a record for the same logical
change twice (e.g. a pulled change it also observed as a batch on another
machine) and must be idempotent; and a crash in the window between a mutation's
commit and its append can lose that one record. Closing that window would mean
writing events inside the transaction journal, which the layering above
forbids; the gap is documented instead.

Rotation is size-based (default 10 MB): the file is renamed to
`events.jsonl.1` (replacing any previous one) and a fresh journal starts.
`seq` continues across rotation, so a consumer that falls more than one file
behind detects the gap by the sequence jump.

**Enablement is opt-in**: `MD_EVENTS=1` turns the journal on, and setting a
hook (below) implies it — the hook's catch-up story depends on the journal
existing. Without either, nothing is written.

### Commit hook

`MD_ON_COMMIT_HOOK=<command>` registers an external command as a push
consumer. For each record, the command is spawned with the record's JSON on
stdin. Execution is a serialized async queue **outside all guards**: a slow or
hanging hook never blocks a write or a read. Each invocation has a timeout
(default 30 s, then killed); a non-zero exit or timeout is logged and the
record is **not retried** — the journal is the catch-up mechanism, so a
consumer that missed pushes re-reads from its last processed `seq`.

An SSE endpoint (`/events` on the HTTP transport, resuming via
`Last-Event-ID` = `seq`) is a natural third channel but is **deferred** until
there is a remote consumer to justify it.

## Consequences

- Positive: external tools get a durable, replayable, semantically labeled
  stream with exact batch boundaries; the hook gives push semantics without any
  server-side delivery state; unconsuming vaults pay nothing (opt-in); the
  layering of ADR-0002/0007 is untouched.
- Negative: one fsync per mutation when enabled; the crash window means the
  stream is not a perfect ledger; consumers must be idempotent; a hook is a
  configured arbitrary-command execution — the operator owns what it runs.
- Neutral: the record schema is an external contract from day one; extending it
  (new `op` kinds, new fields) must be additive.

### Considered and rejected

- **Always-on journal** — imposes an ever-growing file and per-write fsync on
  every vault for consumers that mostly do not exist; violates the config
  surface's fail-closed spirit.
- **Writing events inside the transaction journal** (to close the crash
  window) — couples a pub/sub contract to the crash-recovery mechanism and
  drags policy into `md-core`; also covers only destructive batches, so
  non-destructive writes would need a second path anyway.
- **Webhook (HTTP POST) as the push channel** — requires retry/backoff/URL
  state in the server; an exec hook composes with anything (including `curl`)
  and keeps delivery state out of the server.
- **MCP resource-change notifications** — would require first exposing notes
  as MCP resources, a separate product decision; and reaches MCP clients, not
  external processes.
