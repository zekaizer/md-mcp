# 30. Read-only vault mode

## Status

Accepted

Builds on [ADR-0016](0016-git-sync-integration.md)/[ADR-0018](0018-git-automation.md)
(git sync and its automation, narrowed here to a fast-forward pull) and
[ADR-0028](0028-byte-level-note-api.md) (the transfer API, whose `PUT` is withheld).
Settles the "multi-vault serving" revisit trigger of
[ADR-0020](0020-deployment-and-configuration-posture.md) without a config file.

## Context

A second kind of vault is to be served beside the main one: a vault agents may
read but never change — typically a mirror of a repository someone else writes,
kept current by pulling.

Multi-vault serving stays one process per vault. Each instance keeps its flat env
file, its own hostname (the OAuth endpoints are root-mounted, so two vaults cannot
share one host by path), and its own connector name on the client. Nothing in a
single process would be shared: search is a linear scan with no index or cache
(ADR-0010).

What the server lacks is a way to make one instance refuse writes. Facts that shape
it:

- The tool surface is assembled per server from tool families (read, write,
  organize, search, plus `sync_vault` and `provision_transfer` when enabled).
  Clients decide what to auto-approve from each tool's `readOnlyHint`.
- The tool surface reads no OAuth scopes, and the rmcp session factory never sees
  the request, so read-only cannot be a property of a token. It has to be a
  property of the instance.
- A mirror kept current by pull needs the server to write the tree itself (git
  checkout, `.md-mcp/lock`). The filesystem stays writable for the server, so the
  kernel (systemd `ReadOnlyPaths=`) cannot be the enforcement; the code must.
- `sync_vault` commits local changes and pushes. On a mirror that would publish
  whatever drifted locally; yet a mirror still needs a way for an agent to ask
  for fresh content.
- `Vault::open` rolls back transactions a crash left behind, which mutates the
  tree.

## Decision

We will add an explicit **read-only mode** (`--read-only` / `MD_READ_ONLY=1`).
Writability is not probed: sandboxing and per-directory permissions make a probe
answer a different question than the operator's intent.

**Tool surface.** A read-only instance registers only the read and search families,
plus `sync_vault` when git sync is on — narrowed to a pull (see Git), so the
agent can still ask for fresh content. `provision_transfer` stays (when a bearer
is configured) for reading large notes, but refuses `write: true`. Every
advertised tool that touches notes is therefore `readOnlyHint: true`, except the
pull an agent triggers explicitly.

**Transfer API.** `PUT /api/notes/{path}` is not routed; the method answers
`405 Method Not Allowed`.

**Core enforcement.** The vault itself carries the mode. Every write entry point —
`write_atomic` (hence `create_note`), `commit_batch`, `remove_empty_dir` — refuses
with a new error code **`READ_ONLY`**. This is the backstop for a future surface
that forgets the mode; the surfaces above never reach it.

**Startup.** A read-only vault does not recover transactions. If transaction
journals are pending, opening it fails; the operator starts the instance once
read-write to recover. Serving half-applied batches silently is worse than not
starting.

**Git.** With git sync on, a read-only instance pulls by **fetch, then fast-forward
only**, under the same write guard and vault lock as a sync. It never commits and
never pushes. It pulls at three moments: once in the background at startup (a
mirror is otherwise stale until the first interval), every
`MD_GIT_SYNC_INTERVAL_SECS`, and whenever an agent calls `sync_vault`. A branch that cannot fast-forward (diverged history, or local edits
the merge would overwrite) is a sync failure: logged, recorded as sync health, and
retried by the next interval; nothing local is overwritten.
`MD_GIT_AUTO_COMMIT` or `MD_GIT_AUTO_PUSH_SECS` together with `MD_READ_ONLY` is a
startup error (fail-closed, as every other contradictory setting).

**Instructions.** The server instructions state that the vault is read-only and
drop the sentence about destructive batches.

Serving one directory from both a read-write and a read-only instance is
unsupported: the read-only instance's reads do not take the cross-process lock, and
two git drivers on one repository contend for its index.

## Consequences

- A read-only connector shows only tools a client may auto-approve, besides the
  explicit pull; a write can be attempted only through a surface that no longer
  exists, and the core refuses it anyway.
- `sync_vault` keeps one name and two behaviours, told apart by the instance's
  mode; its description states both.
- `READ_ONLY` joins the error-code contract.
- A mirror's local divergence is never resolved automatically. It shows up only in
  the logs (there are no write responses to carry the ADR-0019 warning), and an
  operator resolves it in git.
- Recovering a crashed transaction on a read-only vault takes one read-write start.
- Multi-vault deployment is a deployment concern only: one templated unit, env
  file, and hostname per vault. ADR-0020's flat env file stands.
