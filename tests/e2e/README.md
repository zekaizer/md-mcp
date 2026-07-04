# stdio end-to-end tests

Black-box tests that drive the shipped `md-server --stdio` binary over its real
JSON-RPC-on-stdio transport — the same wire a production MCP client uses. They
complement, and do not replace, the in-process rmcp protocol test
(`crates/md-server/tests/e2e.rs`, [ADR-0012]) and the per-module unit tests:
this layer exercises stdout purity, message framing, process lifecycle, and
end-user tool behavior that only the external transport reveals. See
[ADR-0015](../../docs/adr/0015-stdio-end-to-end-suite.md) for the rationale.

## Running

```sh
python3 tests/e2e/run.py            # build release, run everything
python3 tests/e2e/run.py --no-build # reuse target/release/md-server
python3 tests/e2e/run.py --only hardening
make e2e                            # from the repo root
make check                          # fmt + clippy + cargo test + e2e (pre-push gate)
```

The runner exits non-zero if any check fails, so it drops straight into a git
hook or CI step. No third-party packages — Python 3.9+ and a built server only.

## Layout

| File | Contents |
|---|---|
| `harness.py` | `MCPClient` (stdio JSON-RPC) and `Runner` (checks + summary) |
| `run.py` | Entry point: build, isolate temp vaults, run suites, report, exit code |
| `suites/functional.py` | Per-tool behavior: read/search shapes, section addressing, the content_hash edit flow, failure semantics, NFC paths, size budget |
| `suites/hardening.py` | Traversal corpus, symlink escape, internal-state isolation, write/path-size limits, protocol misuse, crash recovery, startup fail-closed |

## Adding a check

A suite is a function that takes a live client and a `Runner`:

```python
def run(c, t, vault):
    t.section("my group")
    r = c.structured("read_notes", {"paths": ["note.md"]})
    t.check("reads the note", r["notes"][0]["exists"])
```

`t.check(name, ok, detail="")` records a result; `t.section(label)` groups the
following checks. Keep each check's assertion self-contained so a failure names
exactly what broke. Fixtures live in `build_fixtures` (functional) or are built
inline (hardening, which manages its own vaults and subprocesses).

[ADR-0012]: ../../docs/adr/0012-testing-and-benchmarking.md
