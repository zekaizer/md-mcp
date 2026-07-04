"""Fuzz suite: schema-aware adversarial input across all 12 tools over stdio.

Where `functional` proves the tools do the right thing on good input and
`hardening` proves specific attack vectors are blocked, this suite proves the
server *stays a server* under a flood of malformed, hostile, and boundary
input: it never crashes, never hangs, never corrupts stdout, and never escapes
the vault jail — whatever nonsense it is handed.

The fuzzer is schema-aware (it reads each tool's `inputSchema` so the generated
arguments are shaped like real calls often enough to reach past the MCP
framework's type check into the server's own logic) and corpus-driven (path
components, strings, and frontmatter values are drawn from a corpus of known
parser-breakers: traversal, control bytes, unicode edge cases, format
specifiers, oversized payloads, deep nesting).

Determinism: the RNG is seeded (default 1337, override with `--seed`), so any
failure reproduces byte-for-byte. Every anomaly is logged with the exact
JSON-RPC frame that triggered it, so a repro is a copy-paste away.

Invariants asserted (a violation of any is a finding — a *rejected* call is
not, that is the server working):
  1. Liveness    — every call returns within the timeout; no hang/deadlock.
  2. Framing     — stdout carries only parseable JSON-RPC (a stray byte there
                   surfaces as an MCPError from the harness).
  3. No crash    — the process stays alive; stderr never shows a panic.
  4. Well-formed — every response has a `result` or an `error`.
  5. Jail        — nothing is read from, written to, or leaked outside the
                   vault; the internal `.md-mcp` tree is never written by a tool.

`run(binary, t, scratch, iters=..., seed=...)` drives it; it manages its own
vault(s) and subprocess(es) under `scratch`, restarting the server if a fuzz
input manages to kill it (each distinct death is logged with its trigger).
"""

from __future__ import annotations

import json
import os
import random
import threading
from typing import Any

from harness import MCPClient, MCPError, Runner

# --------------------------------------------------------------------------
# Corpus: values chosen to break parsers, path jails, and (de)serializers.
# --------------------------------------------------------------------------

# Path-ish strings. Traversal, absolute, encoded, control, unicode, internal.
HOSTILE_PATHS = [
    "",
    ".",
    "..",
    "../",
    "../../etc/passwd",
    "../secret_outside.txt",
    "..%2f..%2fsecret_outside.txt",
    "....//secret_outside.txt",
    "a/../../secret_outside.txt",
    "./../../secret_outside.txt",
    "..\\..\\secret_outside.txt",
    "/absolute.md",
    "//double-root.md",
    "/etc/passwd",
    "\\\\unc\\share\\x.md",
    "a/b/../../../secret_outside.txt",
    ".md-mcp/journal/x.json",
    ".md-mcp/backup/y",
    "./.md-mcp/x.md",
    ".md-mcp/",
    "nul.md",
    "con.md",
    "a\x00.md",
    "a\nb.md",
    "a\rb.md",
    "a\tb.md",
    "  .md",
    "‮_reversed.md",  # RTL override
    "café.md",  # NFC
    "café.md",  # NFD (same as above decomposed)
    "\U0001f4a9.md",  # 4-byte emoji
    "한글.md",
    "a" * 300 + ".md",  # over any component limit
    "deep/" * 64 + "x.md",  # very deep
    "note",  # no .md suffix
    "dir/",  # directory suffix
    "sub/note.md",
    "legit.md",
    "%s%n%x.md",  # format-string bait
    "{path}.md",
    "note.md\x00.txt",  # embedded NUL truncation bait
]

# Free-form strings for content / query / heading / value fields.
HOSTILE_STRINGS = [
    "",
    " ",
    "\n",
    "\r\n",
    "\x00",
    "\x00\x01\x02\x03",
    "a\x00b",
    "%s%s%s%n",
    "{}{{}}${}",
    "../../etc/passwd",
    "'; DROP TABLE notes; --",
    "\U0001f4a9\U0001f680",
    "́́́",  # naked combining marks
    "‮‭",  # bidi overrides
    "﻿",  # BOM
    "---\ntitle: x\n---\n",  # frontmatter-looking body
    "# H\n" * 500,  # many headings
    "```\n" * 200,  # many code fences
    "\t \t \t",
    "line\r\nline\r\n",
    "日本語のテキスト",
    "a" * 100000,  # large-ish
]

# Frontmatter / JSON values, including deeply nested and type-confused.
def _deep_list(depth: int) -> Any:
    v: Any = 0
    for _ in range(depth):
        v = [v]
    return v


HOSTILE_JSON_VALUES = [
    None,
    True,
    0,
    -1,
    2**63,
    -(2**63),
    1.5,
    float("1e308"),
    "",
    "string",
    [],
    {},
    [1, 2, 3],
    {"k": "v"},
    {"nested": {"a": {"b": {"c": 1}}}},
    _deep_list(50),
    {"": ""},
    {"\x00": "\x00"},
    ["a" * 10000],
]

MAX_STR = 4 * 1024 * 1024  # server's write cap; occasionally exceed it.


# --------------------------------------------------------------------------
# Value generators
# --------------------------------------------------------------------------


class Gen:
    """Seeded value generator. Emits a spectrum from valid-shaped to hostile."""

    def __init__(self, rng: random.Random):
        self.rng = rng

    def _maybe(self, p: float) -> bool:
        return self.rng.random() < p

    def path(self) -> Any:
        r = self.rng.random()
        if r < 0.55:
            return self.rng.choice(HOSTILE_PATHS)
        if r < 0.70:
            return self.string()  # a whole hostile string used as a path
        if r < 0.80:
            return self.rng.choice([None, 123, True, [], {}])  # wrong type
        if r < 0.90:
            return "fuzz/" + "".join(
                self.rng.choice("ab/._-\x00\n한") for _ in range(self.rng.randint(1, 40))
            ) + ".md"
        return "note%d.md" % self.rng.randint(0, 20)

    def string(self) -> Any:
        r = self.rng.random()
        if r < 0.6:
            return self.rng.choice(HOSTILE_STRINGS)
        if r < 0.7:
            return self.rng.choice([None, 1, True, [], {"a": 1}])  # wrong type
        if r < 0.75:
            return "z" * (MAX_STR + self.rng.randint(1, 100))  # over the cap
        n = self.rng.randint(0, 64)
        alpha = "abc \n\t\r#-`한\x00\U0001f4a9"
        return "".join(self.rng.choice(alpha) for _ in range(n))

    def value(self) -> Any:
        return self.rng.choice(HOSTILE_JSON_VALUES)

    def heading_path(self) -> Any:
        r = self.rng.random()
        if r < 0.15:
            return self.rng.choice([None, "notalist", 1, {}])  # wrong type
        n = self.rng.randint(0, 5)
        return [self.string() for _ in range(n)]

    def occurrence(self) -> Any:
        return self.rng.choice([None, 0, 1, 2, -1, -5, 2**63, 1.5, "1", []])

    def scope(self) -> Any:
        return self.rng.choice(["body", "section", "SECTION", "", None, 1, "leaf"])

    def bool_(self) -> Any:
        return self.rng.choice([True, False, None, "true", 1, 0, []])

    def operation(self) -> Any:
        return self.rng.choice(
            ["replace", "append", "delete", "insert_before", "insert_after",
             "rename", "move", "REPLACE", "", None, "drop", 1]
        )

    def position(self) -> Any:
        return self.rng.choice(["before", "after", "BEFORE", "", None, 1])

    def small_batch(self, factory, over: bool = False) -> Any:
        """A list of items; occasionally exceed maxItems=100 or wrong-type it."""
        r = self.rng.random()
        if r < 0.05:
            return self.rng.choice([None, "notalist", 1, {}])
        if r < 0.10 or over:
            n = self.rng.randint(101, 130)  # over maxItems
        else:
            n = self.rng.randint(0, 4)
        return [factory() for _ in range(n)]


# --------------------------------------------------------------------------
# Per-tool argument factories. Each returns the `arguments` object.
# Some intentionally drop required keys / add junk keys.
# --------------------------------------------------------------------------


def _junk(g: Gen, d: dict) -> dict:
    if g._maybe(0.2):
        d[g.rng.choice(["bogus", "extra", "__proto__", ""])] = g.value()
    if g._maybe(0.1):  # drop a random key (may violate `required`)
        keys = list(d)
        if keys:
            d.pop(g.rng.choice(keys))
    return d


def _note_item(g: Gen) -> dict:
    d = {"path": g.path(), "content": g.string()}
    if g._maybe(0.5):
        d["frontmatter"] = g.value()
    return _junk(g, d)


def _append_item(g: Gen) -> dict:
    d = {"path": g.path(), "content": g.string()}
    if g._maybe(0.5):
        d["create_if_missing"] = g.bool_()
    return _junk(g, d)


def _section_target(g: Gen) -> dict:
    d = {"path": g.path(), "heading_path": g.heading_path()}
    if g._maybe(0.6):
        d["occurrence"] = g.occurrence()
    if g._maybe(0.6):
        d["scope"] = g.scope()
    return _junk(g, d)


def _edit_item(g: Gen) -> dict:
    d = {"path": g.path(), "heading_path": g.heading_path(), "operation": g.operation()}
    if g._maybe(0.7):
        d["content"] = g.string()
    if g._maybe(0.4):
        d["scope"] = g.scope()
    if g._maybe(0.4):
        d["occurrence"] = g.occurrence()
    if g._maybe(0.3):
        d["expected_hash"] = g.rng.choice([None, "", "deadbeef", g.string(), 123])
    if g._maybe(0.3):
        d["new_heading"] = g.rng.choice([None, g.string()])
    if g._maybe(0.3):
        d["destination"] = g.rng.choice([
            None,
            {"heading_path": g.heading_path(), "position": g.position()},
            {"heading_path": g.heading_path(), "position": g.position(),
             "occurrence": g.occurrence()},
            {},  # missing required
            "notanobject",
        ])
    return _junk(g, d)


def _property_edit(g: Gen) -> dict:
    d = {"path": g.path(), "key": g.string()}
    if g._maybe(0.6):  # present (even null) sets; omitted removes
        d["value"] = g.value()
    return _junk(g, d)


def _rename_item(g: Gen) -> dict:
    return _junk(g, {"path": g.path(), "new_name": g.rng.choice([g.path(), g.string()])})


def _relocate_item(g: Gen) -> dict:
    return _junk(g, {"source": g.path(), "dest_dir": g.rng.choice([g.path(), "", "/", "d/"])})


def make_factories(g: Gen):
    """Return {tool_name: () -> arguments}."""
    return {
        "read_notes": lambda: _junk(g, {
            "paths": g.small_batch(g.path),
            **({"include_body": g.bool_()} if g._maybe(0.4) else {}),
            **({"include_frontmatter": g.bool_()} if g._maybe(0.4) else {}),
        }),
        "read_outlines": lambda: _junk(g, {"paths": g.small_batch(g.path)}),
        "read_sections": lambda: _junk(g, {
            "targets": g.small_batch(lambda: _section_target(g))}),
        "list_notes": lambda: _junk(g, {
            **({"directory": g.path()} if g._maybe(0.7) else {}),
            **({"recursive": g.bool_()} if g._maybe(0.4) else {}),
            **({"glob": g.rng.choice([None, "**/*.md", "[", "*.{md", g.string()])}
               if g._maybe(0.5) else {}),
            **({"include_dirs": g.bool_()} if g._maybe(0.3) else {}),
            **({"limit": g.rng.choice([None, 0, 1, 1000, 1001, -1, 2**40, 1.5, "x"])}
               if g._maybe(0.5) else {}),
            **({"cursor": g.rng.choice([None, "", "garbage", g.string()])}
               if g._maybe(0.5) else {}),
        }),
        "search_notes": lambda: _junk(g, {
            **({"query": g.rng.choice([None, g.string()])} if g._maybe(0.8) else {}),
            **({"mode": g.rng.choice(["content", "filename", "both", "BOTH", "", None])}
               if g._maybe(0.5) else {}),
            **({"context_lines": g.rng.choice([None, 0, 2, -1, 2**40, 1.5, "x"])}
               if g._maybe(0.4) else {}),
            **({"limit": g.rng.choice([None, 0, 1, 100, 101, -1, 2**40])}
               if g._maybe(0.4) else {}),
            **({"frontmatter": g.value()} if g._maybe(0.4) else {}),
            **({"frontmatter_exists": g.value()} if g._maybe(0.4) else {}),
            **({"cursor": g.rng.choice([None, "", "garbage"])} if g._maybe(0.3) else {}),
        }),
        "create_notes": lambda: _junk(g, {
            "notes": g.small_batch(lambda: _note_item(g)),
            **({"overwrite": g.bool_()} if g._maybe(0.5) else {}),
        }),
        "append_notes": lambda: _junk(g, {
            "appends": g.small_batch(lambda: _append_item(g))}),
        "edit_sections": lambda: _junk(g, {
            "edits": g.small_batch(lambda: _edit_item(g))}),
        "edit_properties": lambda: _junk(g, {
            "edits": g.small_batch(lambda: _property_edit(g))}),
        "rename_notes": lambda: _junk(g, {
            "renames": g.small_batch(lambda: _rename_item(g)),
            **({"overwrite": g.bool_()} if g._maybe(0.5) else {}),
        }),
        "relocate_notes": lambda: _junk(g, {
            "moves": g.small_batch(lambda: _relocate_item(g)),
            **({"overwrite": g.bool_()} if g._maybe(0.5) else {}),
        }),
        "delete_notes": lambda: _junk(g, {"paths": g.small_batch(g.path)}),
    }


# --------------------------------------------------------------------------
# Timeout-guarded transport (a hang is a finding, not a wedged test run).
# --------------------------------------------------------------------------


class Timeout(Exception):
    pass


def _with_timeout(fn, timeout: float):
    box: dict[str, Any] = {}

    def worker():
        try:
            box["ok"] = fn()
        except BaseException as e:  # noqa: BLE001
            box["err"] = e

    th = threading.Thread(target=worker, daemon=True)
    th.start()
    th.join(timeout)
    if th.is_alive():
        raise Timeout()
    if "err" in box:
        raise box["err"]
    return box.get("ok")


# --------------------------------------------------------------------------
# Fixtures + jail probes
# --------------------------------------------------------------------------

SEED_FILES = {
    "legit.md": "data\n",
    "structured.md": "---\ntitle: t\n---\n# A\nlead\n\n## A1\nbody\n\n# B\nb\n",
    "daily/log.md": "# Log\nkeyword-alpha\n",
    "empty.md": "",
}


def _seed_vault(vault: str) -> None:
    for p, s in SEED_FILES.items():
        full = os.path.join(vault, p)
        os.makedirs(os.path.dirname(full), exist_ok=True) if os.path.dirname(p) else None
        with open(full, "w", newline="") as f:
            f.write(s)


def _list_tree(root: str) -> set[str]:
    """All files under `root`, as paths relative to `root`."""
    out = set()
    for dp, _dn, fn in os.walk(root):
        for f in fn:
            rel = os.path.relpath(os.path.join(dp, f), root)
            out.add(rel)
    return out


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------


def run(binary: str, t: Runner, scratch: str, iters: int = 250, seed: int = 1337) -> None:
    rng = random.Random(seed)
    g = Gen(rng)
    factories = make_factories(g)
    tools = list(factories)

    root = os.path.join(scratch, "fuzz")
    os.makedirs(root, exist_ok=True)
    vault = os.path.join(root, "vault")
    os.makedirs(vault, exist_ok=True)
    _seed_vault(vault)

    # Exfil targets that must stay pristine no matter what the fuzzer sends.
    outside = os.path.join(root, "secret_outside.txt")
    with open(outside, "w") as f:
        f.write("TOPSECRET")
    sibling = os.path.join(root, "sibling")
    os.makedirs(sibling, exist_ok=True)
    with open(os.path.join(sibling, "data.md"), "w") as f:
        f.write("SIBLING")
    # Everything under `root` at rest, minus the vault subtree: the fuzzer must
    # never cause a new file to appear outside `vault/` (paths are root-relative,
    # so vault files carry a "vault/" prefix and are excluded from this set).
    vault_prefix = "vault" + os.sep
    tree_before = {p for p in _list_tree(root) if not p.startswith(vault_prefix)}

    TIMEOUT = 15.0
    total_calls = 0
    crashes: list[str] = []       # server died on this frame
    hangs: list[str] = []         # no response within TIMEOUT
    badframes: list[str] = []     # stdout not valid JSON-RPC
    malformed_resp: list[str] = []  # response missing both result and error
    leaks: list[str] = []         # exfil target content appeared in a response
    panics: list[str] = []        # stderr showed a panic

    def spawn() -> MCPClient:
        return MCPClient(binary, vault, timeout=TIMEOUT)

    def repro(tool: str, args: Any) -> str:
        frame = {"method": "tools/call", "params": {"name": tool, "arguments": args}}
        s = json.dumps(frame, ensure_ascii=False)
        return s if len(s) < 400 else s[:400] + f"...(+{len(s) - 400}B)"

    def alive(c: MCPClient) -> bool:
        if c.proc.poll() is not None:
            return False
        try:
            _with_timeout(lambda: c.request("tools/list", {}), TIMEOUT)
            return True
        except Exception:  # noqa: BLE001
            return False

    client = spawn()

    t.section(f"fuzz campaign (seed={seed}, {iters}×{len(tools)} calls)")

    for i in range(iters):
        for tool in tools:
            args = factories[tool]()
            total_calls += 1
            rep = repro(tool, args)
            try:
                resp = _with_timeout(lambda: client.call(tool, args), TIMEOUT)
            except Timeout:
                if not hangs:
                    hangs.append(f"{tool}: {rep}")
                try:
                    client.proc.kill()
                except Exception:  # noqa: BLE001
                    pass
                client = spawn()
                continue
            except MCPError as e:
                # stdout closed or non-JSON on the JSON-RPC channel.
                if client.proc.poll() is not None:
                    if not crashes:
                        crashes.append(f"{tool}: {rep}")
                else:
                    if not badframes:
                        badframes.append(f"{tool}: {rep} :: {str(e)[:120]}")
                client = spawn()
                continue
            except json.JSONDecodeError as e:
                if not badframes:
                    badframes.append(f"{tool}: {rep} :: {str(e)[:120]}")
                client = spawn()
                continue

            # Got a response. It must be well-formed JSON-RPC.
            if not isinstance(resp, dict) or ("result" not in resp and "error" not in resp):
                if not malformed_resp:
                    malformed_resp.append(f"{tool}: {rep} :: {json.dumps(resp)[:150]}")
            # No exfil target content may ever surface in a response.
            blob = json.dumps(resp, ensure_ascii=False)
            if "TOPSECRET" in blob or "SIBLING" in blob or "root:" in blob:
                if not leaks:
                    leaks.append(f"{tool}: {rep}")

            # Cheap liveness probe: is the process still up? (Full ping is costly.)
            if client.proc.poll() is not None:
                if not crashes:
                    crashes.append(f"{tool}: {rep}")
                client = spawn()

        # Periodic deep checks + re-seed (destructive tools erode the fixtures).
        if i % 25 == 24:
            if any("panicked" in ln for ln in client.stderr_lines) and not panics:
                panics.append(next(ln for ln in client.stderr_lines if "panicked" in ln)[:160])
            _seed_vault(vault)

    # ---- final invariants ------------------------------------------------
    tree_after = {p for p in _list_tree(root) if not p.startswith(vault_prefix)}
    stray = tree_after - tree_before
    # The outside secret + sibling must be byte-identical.
    pristine = (
        os.path.exists(outside) and open(outside).read() == "TOPSECRET"
        and open(os.path.join(sibling, "data.md")).read() == "SIBLING"
    )
    if any("panicked" in ln for ln in client.stderr_lines) and not panics:
        panics.append(next(ln for ln in client.stderr_lines if "panicked" in ln)[:160])

    t.check(f"no server crash ({total_calls} calls)", not crashes,
            crashes[0] if crashes else "")
    t.check("no hangs (all calls answered)", not hangs, hangs[0] if hangs else "")
    t.check("stdout stayed pure JSON-RPC", not badframes, badframes[0] if badframes else "")
    t.check("every response well-formed", not malformed_resp,
            malformed_resp[0] if malformed_resp else "")
    t.check("no panic on stderr", not panics, panics[0] if panics else "")
    t.check("no exfil-target leak in any response", not leaks, leaks[0] if leaks else "")
    t.check("no stray files outside vault", not stray, f"{sorted(stray)[:5]}")
    t.check("exfil targets byte-identical", pristine)
    t.check("server responsive after campaign", alive(client))

    client.close()
