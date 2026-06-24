# 6. Vault path jail and atomic write

## Status

Accepted

## Context

Every md-mcp tool addresses notes by **vault-relative path** supplied by an
agent, so those paths are untrusted input. Path traversal — escaping the vault
root via `..`, an absolute path, or a symlink that points outside — is the #1
security risk for this class of server ([tool_spec §4](../tool_spec.md)). Writes
must also be crash-atomic: a power loss mid-write must never leave a half-written
note.

strata defends the boundary with a two-gate lexical-plus-`canonicalize`
containment check in pure `std`. That is sound but leaves a TOCTOU window: a
symlink swapped between the containment check and the subsequent `open` can still
escape, and the not-yet-existing target of a create cannot be canonicalized so it
needs special handling. The maintainer prefers a root-cause fix over a guarded
window, and prefers a stable library where one fits.

## Decision

We will jail all vault file I/O behind a **`cap-std` capability `Dir`** opened on
the vault root at startup, and route every note read/write/delete/rename through
that `Dir`'s `*at`-relative methods.

- **Kernel-enforced containment.** On Linux `cap-std` uses
  `openat2(RESOLVE_BENEATH)`, so a path handed to the `Dir` *cannot* resolve
  outside it — including through a mid-path symlink, and including a target that
  does not exist yet (relevant to `create_notes`). This closes the TOCTOU window
  the lexical+`canonicalize` approach leaves open. `cap-std` 4.x (Bytecode
  Alliance, used by Wasmtime/WASI) is the stable library for exactly this.
- **Lexical pre-check (defense in depth + clean errors).** Before touching the
  `Dir`, `Vault::validate_rel` walks `Path::components()` and rejects any
  `Prefix`, `RootDir` (absolute), or `ParentDir` (`..`), allows `Normal` and
  `CurDir`, and rejects the empty/root path. This yields a precise
  `Code::Traversal` error with a helpful message instead of a generic I/O error,
  and enforces the rules syntactically; `cap-std` remains the kernel backstop for
  anything lexical analysis cannot see (live symlinks).
- **Vault-root rejection for destructive ops.** The empty path and `/` resolve to
  the vault root; destructive primitives reject them so a batch can never delete
  or rename the vault itself.
- **Atomic content write.** `Vault::write_atomic(rel, bytes)` creates the parent
  directory if absent, creates a `cap_tempfile::TempFile` in the **target's parent
  directory** (same filesystem → `rename(2)` is atomic), writes the bytes,
  `sync_all()`s the file contents, then `TempFile::replace(name)` renames it over
  the target. A reader therefore sees either the old file or the complete new
  file, never a torn one.
- **Create guard.** `create_note` refuses a pre-existing target detected via
  `symlink_metadata` (not `metadata`/`exists`), so even a dangling or
  out-of-tree symlink at the target counts as occupied and is refused rather than
  written through.

Full power-loss durability of the *directory entry* (fsync of the parent
directory after the rename) and multi-file batch atomicity are the concern of the
transaction engine ([ADR-0007](README.md)); this ADR covers single-file atomic
content writes and the traversal jail.

## Consequences

- Positive: traversal is impossible by construction (kernel-enforced), not merely
  checked — the strongest guarantee available, with no TOCTOU window; the
  not-yet-existing create target is handled natively; atomic writes prevent torn
  files. The jail is a small, central choke point in `md-core::vault`.
- Negative: all vault I/O must go through the `Dir` capability rather than ambient
  `std::fs` paths — a deliberate, one-time architectural constraint. `cap-std`
  containment is strongest on Linux (`openat2`); other platforms use a careful
  emulation (acceptable; md-mcp targets Linux).
- Neutral: the lexical pre-check duplicates part of what `cap-std` enforces, kept
  for clean error reporting and the syntactic rules (`..`, root rejection).

### Considered and rejected

- **Lexical + `std::fs::canonicalize` containment (strata's approach)** — sound
  but TOCTOU-windowed and needs special-casing for non-existent create targets.
  `cap-std` removes both problems at the kernel.
- **A purely lexical guard (`path-clean`/`normalize-path`)** — symlink-blind; a
  false sense of security as the sole defense.
