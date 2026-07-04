# 15. stdio end-to-end test suite

## Status

Accepted

## Context

[ADR-0012](0012-testing-and-benchmarking.md) established the testing baseline:
per-module unit tests and one in-process protocol test
(`crates/md-server/tests/e2e.rs`) that drives the server through an rmcp client
over an in-memory duplex transport. That covers the tool logic and the rmcp
request/response contract, but it never crosses a real process boundary: it does
not exercise the stdio transport the shipped binary actually speaks — stdout
purity (a stray write there corrupts the JSON-RPC channel), newline framing,
environment-driven configuration (`MD_VAULT`), fail-closed startup, or crash
recovery on a real on-disk vault.

While hardening the server we accumulated a comprehensive black-box test harness
in Python that spawns `md-server --stdio` and speaks the wire protocol directly.
It repeatedly caught behavior the in-process test could not — transport-level
edge cases, path/symlink escapes over the real filesystem, transaction crash
recovery from hand-crafted journals, and protocol misuse. This decision adopts
that harness as a permanent, first-class layer rather than leaving it as
throwaway scratch tooling.

## Decision

We will keep a **stdio black-box end-to-end suite** under `tests/e2e/`, additive
to — not a replacement for — the ADR-0012 in-process test.

- **Transport**: it launches the real release `md-server --stdio` and exchanges
  JSON-RPC over stdin/stdout, so stdout purity, framing, and process lifecycle
  are under test.
- **Two suites**: `functional` (per-tool behavior against a fixture vault:
  read/search shapes, section addressing, the `content_hash` edit flow,
  partial-success vs all-or-nothing semantics, NFC paths, the read-size budget)
  and `hardening` (traversal corpus, symlink escape and write-through,
  internal-state isolation, write/path-size limits, protocol misuse, transaction
  crash recovery, fail-closed startup).
- **Zero dependencies**: standard-library Python 3.9+ only, so it runs anywhere
  the toolchain already is, with no virtualenv to manage. Test code comments and
  identifiers are English, per the repository convention.
- **One gate**: a repo-root `Makefile` exposes `make check` = `fmt` + `lint` +
  `test` (cargo) + `e2e`. The runner exits non-zero on any failure, so `make
  check` is the single command to run before committing, merging, or pushing,
  and drops directly into a git hook or CI step.
- **Isolation**: each suite runs in its own `tempfile` vault; nothing touches the
  working tree or `.local/`.

## Consequences

- Positive: the real stdio contract and filesystem-level security boundary are
  verified end to end; the hardening scenarios are preserved and rerunnable; a
  single `make check` gate covers formatting, lints, and all test layers.
- Positive: dependency-free Python keeps the barrier to running it near zero.
- Negative: a second test language (Python) now lives alongside Rust; the suite
  is asserted by hand rather than through a typed client, so it must be kept in
  sync with tool contract changes by discipline, not by the compiler.
- Neutral: the suite requires a release build; `make e2e` builds it, and CI (when
  added) pays that cost once per run. This ADR extends ADR-0012's testing
  strategy; ADR-0012 remains accepted and unchanged.
