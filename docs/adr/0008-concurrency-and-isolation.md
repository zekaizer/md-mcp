# 8. Concurrency and isolation

## Status

Accepted

## Context

[tool_spec §4](../tool_spec.md) requires a readers-writer discipline: writes are
serialized, and the commit step is **exclusive with reads** so a multi-note read
never sees a torn snapshot (some notes from before a batch, some after). External
change detection is a *separate* layer, handled by `expected_hash`
([ADR-0005](0005-content-hash.md)).

strata serializes writes across processes with an OS `flock`, because it
coordinates the MCP server against a second process — a git-sync daemon. md-mcp
has no such daemon and no git: it is a single local process speaking stdio to one
client. So strata's cross-process lock has no counterpart here, and `md-core` is
deliberately synchronous and runtime-free ([ADR-0002](0002-implementation-language-and-stack.md)).

## Decision

We will enforce the readers-writer discipline with an **in-process
`tokio::sync::RwLock`** held in the server (`md-server`), keeping `md-core`
free of async and locking:

- Read tools acquire a **read guard** for the duration of their vault reads;
  multiple reads run concurrently.
- Destructive tools acquire a **write guard** around the transaction commit
  (`Vault::commit_batch`), so the commit is exclusive with all reads and with
  other writes. A multi-note read therefore never overlaps a commit and cannot
  observe a partially-applied batch.

We will **not** add a cross-process OS lock for write serialization — a single
process already serializes its own writes through this lock. A single-instance
startup guard (so a second `md-server` cannot open the same vault concurrently)
is an optional future addition (`std::fs::File::try_lock`, zero dependencies); it
is not required for correctness of the single-client stdio profile and is
deferred.

The lock is a coordination gate over the shared `Arc<Vault>`; the vault's own
file operations remain synchronous (small notes, sub-millisecond), called inline
under the guard.

## Consequences

- Positive: the spec's reads-exclusive-with-commit guarantee with one small
  in-process primitive and no extra dependency; `md-core` stays synchronous and
  unit-testable without a runtime; no cross-process machinery to maintain.
- Negative: blocking file I/O runs on the async worker under the guard; fine for a
  single-client server over small files, but not designed for high concurrency. A
  second server process on the same vault is not yet prevented (deferred
  single-instance lock).
- Neutral: `expected_hash` remains the independent layer that detects changes made
  by an external editor between a read and a write.

### Considered and rejected

- **An OS `flock` for write serialization (strata's model)** — exists in strata
  only to coordinate with its git daemon; pure overhead for a single local
  process.
- **A lock inside `md-core`** — would pull `tokio` into the runtime-free core,
  breaking the [ADR-0002](0002-implementation-language-and-stack.md) boundary.
