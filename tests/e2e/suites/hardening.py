"""Hardening suite: security and robustness over stdio.

Adversarial coverage the functional suite does not carry: the path-traversal
corpus, symlink escape/write-through, internal-state isolation, write-size and
path-length limits, protocol misuse, deep-nesting safety, and transaction crash
recovery. Several checks spawn their own servers (startup failures, hand-crafted
crash journals), so this suite manages its own vaults and processes under a
scratch root.

`run(binary, t, scratch)` drives it; `scratch` is a writable temp directory.
"""

from __future__ import annotations

import json
import os
import subprocess

from harness import MCPClient, Runner


def run(binary: str, t: Runner, scratch: str) -> None:
    sc = lambda r: r["result"].get("structuredContent", {})

    vault = os.path.join(scratch, "vault")
    os.makedirs(vault, exist_ok=True)
    # A secret file just outside the vault, and a sibling vault, as exfil targets.
    outside = os.path.join(scratch, "secret_outside.txt")
    with open(outside, "w") as f:
        f.write("TOPSECRET")
    sibling = os.path.join(scratch, "sibling")
    os.makedirs(sibling, exist_ok=True)
    with open(os.path.join(sibling, "data.md"), "w") as f:
        f.write("SIBLING")

    c = MCPClient(binary, vault)
    c.call("create_notes", {"notes": [{"path": "legit.md", "content": "data\n"}]})

    # -- path traversal corpus (read) --------------------------------------
    t.section("traversal (read)")
    vectors = [
        "../secret_outside.txt", "../../etc/passwd", "..%2f..%2fetc%2fpasswd",
        "....//secret_outside.txt", "a/../../secret_outside.txt", "./../../secret_outside.txt",
        "..\\..\\secret_outside.txt", "/absolute.md", "//double-root.md",
        "a/b/../../../secret_outside.txt", "../sibling/data.md",
    ]
    leaked = None
    for v in vectors:
        content = sc(c.call("read_notes", {"paths": [v]}))["notes"][0].get("content", "")
        if "TOPSECRET" in content or "SIBLING" in content or "root:" in content:
            leaked = v
            break
    t.check(f"no read leak ({len(vectors)} vectors)", leaked is None, f"leaked via {leaked!r}")

    # -- traversal on write/organize ---------------------------------------
    t.section("traversal (write)")
    r = sc(c.call("create_notes", {"notes": [{"path": "../evil.txt", "content": "x"}]}))["created"][0]
    t.check("create traversal blocked", r.get("error", {}).get("code") == "TRAVERSAL" and not os.path.exists(os.path.join(scratch, "evil.txt")))
    r = sc(c.call("append_notes", {"appends": [{"path": "../evil.md", "content": "x", "create_if_missing": True}]}))["appended"][0]
    t.check("append traversal blocked", r.get("error", {}).get("code") == "TRAVERSAL")
    r = sc(c.call("move_notes", {"moves": [{"source": "legit.md", "dest": "../escaped.md"}]}))
    t.check("move '../' full-path blocked", not r["ok"] and r["errors"][0]["code"] == "TRAVERSAL" and not os.path.exists(os.path.join(scratch, "escaped.md")))
    r = sc(c.call("move_notes", {"moves": [{"source": "legit.md", "dest": "../"}]}))
    t.check("move '../' dir-target blocked", not r["ok"] and r["errors"][0]["code"] in ("TRAVERSAL", "DEST_NOT_DIR"))
    r = sc(c.call("delete_notes", {"paths": ["../secret_outside.txt"]}))
    t.check("delete traversal blocked", not r["ok"] and os.path.exists(outside))

    # -- symlink escape ----------------------------------------------------
    t.section("symlink escape")
    os.symlink(os.path.abspath(outside), os.path.join(vault, "link_out.md"))
    r = sc(c.call("read_notes", {"paths": ["link_out.md"]}))["notes"][0]
    t.check("symlink-out read blocked", "TOPSECRET" not in r.get("content", "") and r.get("error") is not None)
    c.call("create_notes", {"notes": [{"path": "link_out.md", "content": "OVERWRITTEN"}], "overwrite": True})
    t.check("write-through-symlink blocked", open(outside).read() == "TOPSECRET")
    os.symlink(os.path.abspath(sibling), os.path.join(vault, "dirlink"))
    c.call("create_notes", {"notes": [{"path": "dirlink/pwned.md", "content": "x"}]})
    t.check("write via symlinked dir blocked", not os.path.exists(os.path.join(sibling, "pwned.md")))
    c.call("edit_sections", {"edits": [{"path": "link_out.md", "heading_path": [], "operation": "replace", "content": "x"}]})
    t.check("edit-through-symlink blocked", open(outside).read() == "TOPSECRET")

    # -- internal state isolation ------------------------------------------
    t.section("internal-state isolation")
    blocked = all(
        sc(c.call("create_notes", {"notes": [{"path": p, "content": "x"}]}))["created"][0].get("error", {}).get("code") == "TRAVERSAL"
        for p in [".md-mcp/journal/x.md", ".md-mcp/x.md", "./.md-mcp/backup/y.md"]
    )
    t.check("write to .md-mcp blocked (all spellings)", blocked)
    t.check("delete .md-mcp blocked", not sc(c.call("delete_notes", {"paths": [".md-mcp/"]}))["ok"])
    t.check("list .md-mcp empty", sc(c.call("list_notes", {"directory": ".md-mcp/"}))["items"] == [])

    # -- write size / path length ------------------------------------------
    t.section("input size limits")
    cap = 4 * 1024 * 1024
    big = "z" * (cap + 1)
    r = sc(c.call("create_notes", {"notes": [{"path": "toobig.md", "content": big}, {"path": "fine.md", "content": "ok\n"}]}))["created"]
    t.check("create over-cap TOO_LARGE, sibling ok", not r[0]["created"] and r[0]["error"]["code"] == "TOO_LARGE" and r[1]["created"])
    t.check("oversized not on disk", not os.path.exists(os.path.join(vault, "toobig.md")))
    c.call("create_notes", {"notes": [{"path": "grow.md", "content": "z" * (cap - 100)}], "overwrite": True})
    r = sc(c.call("append_notes", {"appends": [{"path": "grow.md", "content": "z" * 200}]}))["appended"][0]
    t.check("append growth bounded", not r["appended"] and r["error"]["code"] == "TOO_LARGE")
    c.call("create_notes", {"notes": [{"path": "edg.md", "content": "# A\nbody\n"}], "overwrite": True})
    r = sc(c.call("edit_sections", {"edits": [{"path": "edg.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": big}]}))
    t.check("edit over-cap all-or-nothing", not r["ok"] and r["errors"][0]["code"] == "TOO_LARGE")
    r = sc(c.call("create_notes", {"notes": [{"path": "a" * 300 + ".md", "content": "x"}]}))["created"][0]
    t.check("over-long name -> SUFFIX (not raw IO)", r["error"]["code"] == "SUFFIX")
    r = sc(c.call("create_notes", {"notes": [{"path": "a" * 250 + ".md", "content": "x"}]}))["created"][0]
    t.check("250-char name accepted", r["created"])

    # -- protocol misuse ---------------------------------------------------
    t.section("protocol misuse")
    def is_err(resp):
        res = resp.get("result", {})
        return res.get("isError") is True or "error" in resp or "failed to deserialize" in json.dumps(res)
    t.check("unknown tool -> error", is_err(c.request("tools/call", {"name": "no_such_tool", "arguments": {}})))
    t.check("wrong param type -> error", is_err(c.request("tools/call", {"name": "read_notes", "arguments": {"paths": "x"}})))
    t.check("missing required -> error", is_err(c.request("tools/call", {"name": "read_notes", "arguments": {}})))
    t.check("invalid enum -> error", is_err(c.request("tools/call", {"name": "edit_sections", "arguments": {"edits": [{"path": "x.md", "heading_path": [], "operation": "xxx"}]}})))
    t.check("negative occurrence -> error", is_err(c.request("tools/call", {"name": "read_sections", "arguments": {"targets": [{"path": "legit.md", "heading_path": ["X"], "occurrence": -5}]}})))
    t.check("extra field ignored (lenient)", sc(c.call("read_notes", {"paths": ["legit.md"], "bogus": 1})).get("notes") is not None)
    r = c.call("read_notes", {"paths": ["ghost.md"]})
    t.check("business 'missing' is not isError", r["result"].get("isError") in (False, None) and sc(r)["notes"][0]["exists"] is False)
    # Deep-nested JSON is rejected as a parse error and the server stays alive.
    nested = json.loads("[" * 400 + "]" * 400)
    resp = c.request("tools/call", {"name": "create_notes", "arguments": {"notes": [{"path": "d.md", "content": "x\n", "frontmatter": {"d": nested}}]}})
    alive = sc(c.request("tools/list", {})) is not None or c.tools() is not None
    t.check("deep nesting rejected, server alive", ("error" in resp or is_err(resp)) and alive)
    c.close()

    # -- crash recovery ----------------------------------------------------
    t.section("crash recovery")
    # (a) uncommitted journal -> rolled back on reopen.
    cv = os.path.join(scratch, "crash_uncommitted")
    os.makedirs(os.path.join(cv, ".md-mcp/journal"), exist_ok=True)
    os.makedirs(os.path.join(cv, ".md-mcp/backup"), exist_ok=True)
    with open(os.path.join(cv, ".md-mcp/backup/bk0"), "w") as f:
        f.write("original\n")  # source was moved aside mid-crash
    j = {"batch_id": "deadbeef", "committed": False,
         "undo": [{"RestoreFromBackup": {"backup": ".md-mcp/backup/bk0", "path": "real.md"}}]}
    with open(os.path.join(cv, ".md-mcp/journal/deadbeef.json"), "w") as f:
        json.dump(j, f)
    cc = MCPClient(binary, cv)
    r = sc(cc.call("read_notes", {"paths": ["real.md"], "include_frontmatter": False}))["notes"][0]
    t.check("uncommitted journal rolled back", r.get("exists") and r.get("content") == "original\n")
    t.check("journal cleaned up", not os.path.exists(os.path.join(cv, ".md-mcp/journal/deadbeef.json")))
    cc.close()

    # (b) committed-but-uncleaned journal -> kept, just cleaned.
    cv = os.path.join(scratch, "crash_committed")
    os.makedirs(os.path.join(cv, ".md-mcp/journal"), exist_ok=True)
    with open(os.path.join(cv, "x.md"), "w") as f:
        f.write("committed-state\n")
    j = {"batch_id": "cccc", "committed": True, "undo": [{"DeletePath": {"path": "x.md"}}]}
    with open(os.path.join(cv, ".md-mcp/journal/cccc.json"), "w") as f:
        json.dump(j, f)
    cc = MCPClient(binary, cv)
    r = sc(cc.call("read_notes", {"paths": ["x.md"], "include_frontmatter": False}))["notes"][0]
    t.check("committed journal not rolled back", r.get("content") == "committed-state\n")
    t.check("committed journal cleaned", not os.path.exists(os.path.join(cv, ".md-mcp/journal/cccc.json")))
    cc.close()

    # (c) corrupt journal tolerated at startup.
    cv = os.path.join(scratch, "crash_corrupt")
    os.makedirs(os.path.join(cv, ".md-mcp/journal"), exist_ok=True)
    with open(os.path.join(cv, "note.md"), "w") as f:
        f.write("hi\n")
    with open(os.path.join(cv, ".md-mcp/journal/garbage.json"), "w") as f:
        f.write("{not valid json!!!")
    try:
        cc = MCPClient(binary, cv)
        ok = sc(cc.call("read_notes", {"paths": ["note.md"], "include_frontmatter": False}))["notes"][0].get("content") == "hi\n"
        cc.close()
        t.check("corrupt journal tolerated at startup", ok)
    except Exception as ex:  # noqa: BLE001
        t.check("corrupt journal tolerated at startup", False, str(ex)[:80])

    # -- startup failures (fail-closed) ------------------------------------
    t.section("startup fail-closed")
    def start_exit(vault_arg, drop_vault=False):
        env = dict(os.environ)
        if drop_vault:
            env.pop("MD_VAULT", None)
        else:
            env["MD_VAULT"] = vault_arg
        p = subprocess.run([binary, "--stdio"], env=env, capture_output=True, text=True, timeout=10)
        return p.returncode
    t.check("missing vault dir -> nonzero exit", start_exit(os.path.join(scratch, "nope")) != 0)
    t.check("vault is a file -> nonzero exit", start_exit(outside) != 0)
    t.check("no MD_VAULT -> nonzero exit", start_exit("", drop_vault=True) != 0)
