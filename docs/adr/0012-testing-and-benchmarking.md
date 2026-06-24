# 12. Testing and benchmarking

## Status

Accepted

## Context

md-mcp is built foundations-first and test-driven; correctness of the parser,
frontmatter splice, transaction engine, and tool envelopes must be provable, and
the hot paths (parse, hash, splice, patch) need a measurable baseline. Benchmark
results should be kept for comparison without polluting the repository.

## Decision

- **Unit tests per module** (`cargo test`), written red-green alongside each
  subsystem, covering the spec's edge cases (multibyte byte-accuracy, NFC,
  code-fence exclusion, type fidelity, all-or-nothing rejection, crash recovery,
  traversal corpus).
- **An end-to-end protocol test** (`crates/md-server/tests/e2e.rs`) drives the
  real server with an in-process rmcp client over a duplex transport: initialize,
  `tools/list` (asserting the full 12-tool surface), and a structured tool call.
  rmcp's `client` feature is a dev-dependency only, so the shipped binary stays
  client-free.
- **Benchmarks with `criterion`**, harness crate `md-core/benches/`, measuring
  parse, content-hash, frontmatter splice, and section patch on representative
  notes. Verification runs them with `CRITERION_HOME=$PWD/.local/bench`, so
  criterion's reports and history land in the git-ignored `.local/` directory and
  never enter the repository; a short summary is written to `.local/`.
- **Linting**: `cargo fmt --check` and `cargo clippy --all-targets` are clean
  (workspace lints: `unsafe_code = forbid`, `clippy::all = warn`).

`proptest`/`insta` are available in the workspace for round-trip property tests and
snapshotting and may be added where they earn their keep; they are not required by
this baseline.

## Consequences

- Positive: each subsystem is independently tested; the protocol surface is
  verified through the real transport; a reproducible performance baseline exists
  without committing volatile benchmark artifacts.
- Negative: criterion's on-disk report format is private/unstable — safe to
  archive per run, not to parse into a contract.
- Neutral: `.local/` is the home for all machine-specific verification output, per
  the repository convention.
