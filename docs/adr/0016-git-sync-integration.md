# 16. Git sync integration: coexistence hardening and the `sync_vault` tool

## Status

Accepted

Partially revises [ADR-0008](0008-concurrency-and-isolation.md): that ADR
decided against any cross-process OS lock on the grounds that md-mcp is a single
local process with no companion daemon. This ADR introduces a **per-operation**
`flock` so external git tooling can coordinate with the server. ADR-0008's
in-process readers-writer mechanism, and its deferral of a single-*instance*
startup guard, are unchanged.

## Context

A vault is often a git repository, synced across machines and agents by
`git push`/`pull`. strata delegated this to an out-of-process git daemon; md-mcp
is deliberately git-free ([ADR-0007](0007-multi-file-transaction.md)) — its own
journal, not git, is the durability layer. That stays. What is missing is a
**replication layer above it**: a way for an agent to synchronize the vault with
a remote, and guarantees that git operations (ours or an external tool's) never
interleave with a transaction or a multi-note read.

The existing architecture already absorbs most of the problem:

- `expected_hash` ([ADR-0005](0005-content-hash.md)) detects external changes
  between a read and a write — a pull is just an external edit.
- Listings hide dot-directories, so `.git/` never surfaces
  ([ADR-0010](0010-search-strategy.md)).
- NFC path normalization (tool_spec) already targets multi-device sync.
- The in-process `RwLock` ([ADR-0008](0008-concurrency-and-isolation.md),
  [ADR-0013](0013-http-transport.md)) gives an exclusion point where the working
  tree is quiescent: no in-flight transaction, no concurrent read.

Two hazards remain. Syncing `.md-mcp/` would replicate one machine's journal,
backups, and trash to another, whose recovery would then replay foreign undo
steps. And a git checkout running concurrently with a transaction commit (from a
cron job, an editor plugin, or a second process) can tear both.

## Decision

We will add git sync as an **opt-in feature of `md-server`**, keeping `md-core`
git-free. It is enabled only by `MD_GIT_SYNC=1`; without it the server gains no
git behavior and exposes no new tool.

### Coexistence hardening (applies whenever the vault is a git repository, even without `MD_GIT_SYNC`)

- At server startup, if `<vault>/.git` exists, append `.md-mcp/` to
  `.git/info/exclude` if not already present. `info/exclude` rather than
  `.gitignore`: the latter is a user-owned, replicated file we must not edit; the
  exclusion is per-machine anyway. A missing `.md-mcp/` rule in either file is
  never an error.
- `.git/` joins `.md-mcp/` as a write-protected internal path: no tool may
  create, edit, or move anything under it (listings already hide it).

### The `sync_vault` tool

One new MCP tool, registered only when sync is enabled. Preconditions checked at
startup: the `git` binary is on `PATH` and `git rev-parse --show-toplevel`
equals the vault root — a vault that is a *subdirectory* of a repository is not
synced (a scoped commit could be built, but a pull would still change files
outside the vault; we refuse the asymmetry). On precondition failure sync is
disabled with a startup warning.

The tool drives the system `git` binary (`GIT_TERMINAL_PROMPT=0`; credentials
are the ambient ssh-agent / credential-helper configuration), against the
current branch and its configured upstream. The sequence is arranged so that
**network I/O never runs under the write guard**:

1. `git fetch` — touches no working-tree file; no guard, but under the
   cross-process flock (below).
2. Under the **write guard + flock**: sweep-commit (`git add -A` + `commit`) if
   dirty, then `git rebase` onto the fetched upstream tip. Both are local-only
   and fast. On conflict: `git rebase --abort` and stop — the working tree is
   restored, and conflict markers never reach a note.
3. Guard released. `git push` — touches no working-tree file. A
   non-fast-forward rejection triggers **one** retry from step 1.

The **sweep commit** is a consequence, not a choice: v1 has no per-batch
auto-commit, so the dirty state at sync time mixes md-mcp writes with external
edits and cannot be attributed. Everything is committed as one
`mcp(sync): checkpoint` commit; excluding files from it is the user's
`.gitignore` responsibility.

**Rebase, not merge**: both sides of a sync are checkpoint commits; linear
history without merge bubbles is worth more than preserving the (meaningless)
branch topology, and `rebase --abort` restores state exactly as
`merge --abort` would.

**Conflict is a result, not an error.** The tool returns
`{status: "clean" | "conflict", pulled, pushed, conflicts: [paths]}`. A conflict
is one of sync's normal outcomes — not retryable, resolved by an agent or human
— so it is reported in the result object. The error envelope
([ADR-0011](0011-error-envelope-and-structured-output.md)) is reserved for
execution failures (git missing mid-flight, network auth failure, unexpected
exit codes).

**Pulled changes are published to the event journal**
([ADR-0017](0017-event-journal-and-hook.md)): the upstream tip is recorded
before and after the fetch, and `git diff --name-status <old>..<new>` becomes
one synthetic `{tool: "sync_vault"}` event record, so the event stream remains a
complete account of vault changes.

### Cross-process flock (revising ADR-0008's "no OS lock")

A `flock` on `.md-mcp/lock` is held **per operation**: around every transaction
commit and around `sync_vault`'s steps 1–2. This is the documented cooperation
protocol for external tools — a cron job or editor plugin that wraps its git
operations in `flock <vault>/.md-mcp/lock ...` can never interleave with a
transaction. A lifetime single-instance lock was rejected because a permanently
held lock makes the cooperation protocol impossible; the single-instance startup
guard remains deferred as in ADR-0008.

## Consequences

- Positive: agent-driven sync with a hard guarantee that git never sees a
  mid-transaction tree; reads are never blocked on network I/O; conflicts can
  never corrupt a note with markers; any external git tooling gets a documented
  way to coordinate; `.md-mcp/` can never leak into the repository.
- Negative: a runtime dependency on the `git` binary (opt-in only); sweep
  commits cannot attribute external edits vs. md-mcp writes; the write guard is
  held across a local rebase (fast, but nonzero); a flock acquisition per
  transaction commit.
- Neutral: automation — per-batch auto-commit, debounced push, interval sync —
  is **deferred**, to be decided in a future ADR once `sync_vault` usage is
  observed. Remote/branch selection is fixed to the current branch's upstream in
  v1.

### Considered and rejected

- **Embedding git via `gix` or `git2`** — `gix`'s merge/rebase support is
  immature (conflict handling is the crux here); `git2` adds a C dependency with
  subtly different merge semantics. The CLI gives exact git semantics and
  ambient credential handling for zero dependencies.
- **`pull --no-rebase` (merge)** — merge-bubble noise on every two-device sync,
  for no benefit over rebase on auto-generated checkpoint commits.
- **Reporting conflicts through the error envelope** — the envelope is
  batch-item shaped and signals "retryable rejection"; a conflict is neither.
- **Supporting a vault that is a repo subdirectory** — commit scoping is
  possible but pull is inherently repo-wide; refused rather than half-supported.
- **A lifetime single-instance flock** — mutually exclusive with the external
  cooperation protocol, which is this ADR's point.
