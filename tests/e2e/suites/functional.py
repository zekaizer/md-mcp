"""Functional suite: per-tool behavioral coverage over stdio.

Exercises every one of the 12 tools against a fixture vault — read/search
shapes, section addressing (occurrence, ambiguity, scope), the content_hash
edit flow, partial-success vs all-or-nothing failure semantics, NFC path
handling, the suffix convention, and the read-response size budget.

`build_fixtures(vault)` populates the vault; `run(c, t, vault)` drives it.
The two are separate because the vault must exist before the server is spawned.
"""

from __future__ import annotations

import json
import os
import unicodedata

from harness import MCPClient, Runner, build_fixture

BASE = (
    "Preamble.\n\n# A\nA lead.\n\n## A1\nA1 body.\n\n## A2\nA2 body.\n\n"
    "# B\nB lead.\n\n## B1\nB1 body.\n"
)


def build_fixtures(vault: str) -> None:
    w = lambda p, s: build_fixture(vault, p, s)
    w("simple.md", "Just a body, no frontmatter, no headings.\n")
    w(
        "structured.md",
        "---\ntitle: Structured Note\nstatus: draft\ntags:\n  - project\n  - test\n"
        "count: 42\n---\nPreamble text before any heading.\n\n"
        "# Design\nDesign lead body.\n\n## Schema\nSchema content line 1.\n\n"
        "### Details\nDeep details.\n\n## API\nAPI section content.\n\n"
        "# Status\nStatus lead.\n\n## Q1\nQ1 content.\n\n## Q1\nDuplicate Q1 heading content.\n",
    )
    w(
        "codefence.md",
        "# Real Heading\nText.\n\n```python\n# not a heading\n```\n\n"
        "    # indented code\n\nSetext Attempt\n==============\n\n## Sub\nSub content.\n",
    )
    w("broken-fm.md", "---\ntitle: [unclosed\nstatus draft\n---\nBody under broken YAML.\n")
    w("no-close-fm.md", "---\ntitle: never closed\nBody no closing fence.\n")
    w("daily/2026-07-01.md", "---\nmood: good\nreviewed: true\n---\n# Log\nkeyword-alpha here.\n")
    w("daily/2026-07-02.md", "# Log\nkeyword-alpha and keyword-beta.\n")
    w("daily/2026-07-03.md", "# Log\nkeyword-beta only.\n")
    w("projects/alpha/readme.md", "---\nstatus: active\ntags: [project, alpha]\n---\n# Alpha\nsearchterm-xyz.\n")
    w("projects/beta.md", "---\nstatus: draft\n---\n# Beta\nsearchterm-xyz.\n")
    w(unicodedata.normalize("NFD", "한글노트") + ".md", "# 한글 제목\n유니코드 본문.\n")
    w("crlf.md", "# Top\r\nCRLF one.\r\n\r\n## Child\r\nchild body.\r\n")
    w("empty.md", "")
    w("fm-only.md", "---\nkey: value\n---\n")
    w(".hidden/secret.md", "# Secret\nkeyword-alpha.\n")
    w("projects/data.txt", "not a note")
    w("norm.md", "# Title ##\nbody\n\n## **Bold** `code` heading\nx\n")
    w("nonl.md", "no newline at end")


def run(c: MCPClient, t: Runner, vault: str) -> None:
    sc = lambda r: r["result"].get("structuredContent", {})
    rpc_err = lambda r: r.get("error")
    body = lambda p="edit.md": sc(
        c.call("read_notes", {"paths": [p], "include_frontmatter": False})
    )["notes"][0].get("content")

    def reset_edit():
        c.call("create_notes", {"notes": [{"path": "edit.md", "content": BASE}], "overwrite": True})

    def raw(p):
        return open(os.path.join(vault, p)).read()

    # -- tools/list surface ------------------------------------------------
    t.section("tools/list")
    names = sorted(tt["name"] for tt in c.tools())
    expected = sorted([
        "read_notes", "read_outlines", "read_sections", "list_notes", "search_notes",
        "create_notes", "append_notes", "edit_sections", "edit_properties",
        "rename_notes", "relocate_notes", "delete_notes",
    ])
    t.check("all 12 tools advertised", names == expected, str(names))
    t.check("serverInfo identifies md-mcp", c.server_info.get("serverInfo", {}).get("name") == "md-mcp",
            json.dumps(c.server_info.get("serverInfo")))

    # -- read_notes --------------------------------------------------------
    t.section("read_notes")
    r = sc(c.call("read_notes", {"paths": ["simple.md", "structured.md", "nope.md"]}))["notes"]
    t.check("normal body", r[0]["content"].startswith("Just a body"))
    t.check("frontmatter parsed", r[1]["frontmatter"]["count"] == 42 and r[1]["frontmatter"]["tags"] == ["project", "test"])
    t.check("body excludes frontmatter", r[1]["content"].startswith("Preamble text"))
    t.check("missing -> exists:false", r[2]["exists"] is False and "content" not in r[2])
    r = sc(c.call("read_notes", {"paths": ["simple.md"], "include_body": False, "include_frontmatter": False}))["notes"][0]
    t.check("flags off -> exists only", r["exists"] and "content" not in r and "frontmatter" not in r)
    r = sc(c.call("read_notes", {"paths": ["broken-fm.md"]}))["notes"][0]
    t.check("broken fm -> body + FRONTMATTER_PARSE", r.get("error", {}).get("code") == "FRONTMATTER_PARSE" and r["content"] == "Body under broken YAML.\n")
    r = sc(c.call("read_notes", {"paths": ["no-close-fm.md"]}))["notes"][0]
    t.check("unclosed fm is body", r["content"].startswith("---\ntitle: never closed"))
    t.check("'..' traversal", sc(c.call("read_notes", {"paths": ["../outside.md"]}))["notes"][0].get("error", {}).get("code") == "TRAVERSAL")
    t.check("absolute traversal", sc(c.call("read_notes", {"paths": ["/etc/passwd"]}))["notes"][0].get("error", {}).get("code") == "TRAVERSAL")
    t.check("empty paths -> empty", sc(c.call("read_notes", {"paths": []}))["notes"] == [])
    t.check("dir path -> error not crash", sc(c.call("read_notes", {"paths": ["daily/"]}))["notes"][0].get("error") is not None)
    r = sc(c.call("read_notes", {"paths": ["한글노트.md"], "include_frontmatter": False}))["notes"][0]
    t.check("NFC path finds NFD file", r["exists"] and "유니코드" in r.get("content", ""))
    r = sc(c.call("read_notes", {"paths": ["empty.md", "fm-only.md"]}))["notes"]
    t.check("empty + fm-only", r[0]["content"] == "" and r[1]["frontmatter"] == {"key": "value"} and r[1]["content"] == "")
    e = rpc_err(c.call("read_notes", {"paths": [f"x{i}.md" for i in range(101)]}))
    t.check("batch 101 -> invalid_params", e is not None and e["code"] == -32602)

    # -- read_outlines -----------------------------------------------------
    t.section("read_outlines")
    o = sc(c.call("read_outlines", {"paths": ["structured.md"]}))["outlines"][0]["headings"]
    q1 = [h for h in o if h["heading_path"] == ["Status", "Q1"]]
    t.check("dup marked ambiguous occ 1,2", len(q1) == 2 and all(h["ambiguous"] for h in q1) and [h["occurrence"] for h in q1] == [1, 2])
    t.check("nesting path", any(h["heading_path"] == ["Design", "Schema", "Details"] and h["level"] == 3 for h in o))
    o = sc(c.call("read_outlines", {"paths": ["codefence.md"]}))["outlines"][0]["headings"]
    t.check("codefence/indent/setext ignored", [h["heading_path"][-1] for h in o] == ["Real Heading", "Sub"])
    o = sc(c.call("read_outlines", {"paths": ["empty.md", "gone.md"]}))["outlines"]
    t.check("empty=[] / missing exists:false", o[0]["headings"] == [] and o[1]["exists"] is False)
    o = sc(c.call("read_outlines", {"paths": ["crlf.md"]}))["outlines"][0]["headings"]
    t.check("crlf parsed", [h["heading_path"][-1] for h in o] == ["Top", "Child"])
    o = sc(c.call("read_outlines", {"paths": ["norm.md"]}))["outlines"][0]["headings"]
    t.check("trailing ## stripped, inline md literal", o[0]["heading_path"] == ["Title"] and o[1]["heading_path"][-1] == "**Bold** `code` heading")

    # -- read_sections -----------------------------------------------------
    t.section("read_sections")
    s = sc(c.call("read_sections", {"targets": [
        {"path": "structured.md", "heading_path": ["Design"]},
        {"path": "structured.md", "heading_path": ["Design"], "scope": "body"},
    ]}))["sections"]
    t.check("section includes subs, body excludes", "## Schema" in s[0]["content"] and "## Schema" not in s[1]["content"] and s[1]["content"].startswith("Design lead"))
    t.check("hashes differ by scope", s[0]["content_hash"] != s[1]["content_hash"])
    s = sc(c.call("read_sections", {"targets": [
        {"path": "structured.md", "heading_path": []},
        {"path": "structured.md", "heading_path": [], "scope": "body"},
    ]}))["sections"]
    t.check("root section=whole body, body=preamble", "# Design" in s[0]["content"] and s[1]["content"] == "Preamble text before any heading.\n\n")
    s = sc(c.call("read_sections", {"targets": [{"path": "structured.md", "heading_path": ["Status", "Q1"]}]}))["sections"][0]
    t.check("ambiguous no occ -> AMBIGUOUS", s["found"] is False and s["error"]["code"] == "AMBIGUOUS")
    s = sc(c.call("read_sections", {"targets": [
        {"path": "structured.md", "heading_path": ["Status", "Q1"], "occurrence": 2},
        {"path": "structured.md", "heading_path": ["Status", "Q1"], "occurrence": 3},
        {"path": "structured.md", "heading_path": ["Design"], "occurrence": 0},
    ]}))["sections"]
    t.check("occ=2 resolves", s[0]["found"] and "Duplicate" in s[0]["content"])
    t.check("occ out of range NOT_FOUND", s[1]["error"]["code"] == "NOT_FOUND")
    t.check("occ=0 rejected", s[2]["found"] is False)
    s = sc(c.call("read_sections", {"targets": [
        {"path": "structured.md", "heading_path": ["Nope"]},
        {"path": "gone.md", "heading_path": ["X"]},
        {"path": "structured.md", "heading_path": ["design"]},
    ]}))["sections"]
    t.check("missing heading vs note distinct", s[0]["note_exists"] and not s[0]["found"] and not s[1]["note_exists"])
    t.check("case-sensitive", s[2]["found"] is False)
    s = sc(c.call("read_sections", {"targets": [{"path": "crlf.md", "heading_path": ["Top", "Child"]}]}))["sections"][0]
    t.check("crlf content LF-normalized", s["content"] == "child body.\n")

    # -- list_notes --------------------------------------------------------
    t.section("list_notes")
    r = sc(c.call("list_notes", {}))
    paths = [i["path"] for i in r["items"]]
    t.check("sorted", paths == sorted(paths))
    t.check("excludes .hidden/.txt/.md-mcp", not any(".hidden" in p or p.endswith(".txt") or ".md-mcp" in p for p in paths))
    t.check("nfd file listed", any(unicodedata.normalize("NFC", p) == "한글노트.md" for p in paths))
    r = sc(c.call("list_notes", {"include_dirs": True, "recursive": False}))
    t.check("dirs end with /", any(i["path"] == "daily/" and "size_bytes" not in i for i in r["items"]))
    r = sc(c.call("list_notes", {"glob": "daily/**/*.md"}))
    t.check("glob", [i["path"] for i in r["items"]] == ["daily/2026-07-01.md", "daily/2026-07-02.md", "daily/2026-07-03.md"])
    r = sc(c.call("list_notes", {"directory": "daily/", "glob": "2026-07-0[12].md"}))
    t.check("glob relative to directory", len(r["items"]) == 2)
    t.check("nonexistent dir -> empty", sc(c.call("list_notes", {"directory": "nope/"}))["items"] == [])
    t.check("dir without slash ok", len(sc(c.call("list_notes", {"directory": "daily"}))["items"]) == 3)
    all_paths, cursor = [], None
    while True:
        args = {"limit": 4}
        if cursor:
            args["cursor"] = cursor
        rr = sc(c.call("list_notes", args))
        all_paths += [i["path"] for i in rr["items"]]
        cursor = rr.get("next_cursor")
        if not cursor:
            break
    t.check("paging no dup/loss", len(all_paths) == len(set(all_paths)) and set(all_paths) == set(paths), f"{len(all_paths)} vs {len(paths)}")
    e = c.call("list_notes", {"limit": 0}).get("error")
    t.check("limit=0 rejected", e is not None and e["code"] == -32602)
    e = c.call("list_notes", {"limit": 2000}).get("error")
    t.check("limit>max rejected", e is not None)

    # -- search_notes ------------------------------------------------------
    t.section("search_notes")
    r = sc(c.call("search_notes", {"query": "keyword-alpha keyword-beta"}))
    t.check("AND", [i["path"] for i in r["items"]] == ["daily/2026-07-02.md"])
    r = sc(c.call("search_notes", {"query": "KEYWORD-ALPHA"}))
    t.check("case-insensitive + hidden excluded", len(r["items"]) == 2)
    r = sc(c.call("search_notes", {"query": "2026-07", "mode": "filename"}))
    t.check("filename mode", len(r["items"]) == 3)
    r = sc(c.call("search_notes", {"frontmatter": {"status": "active", "tags": "alpha"}}))
    t.check("fm scalar + list contains + echo", len(r["items"]) == 1 and r["items"][0]["frontmatter"]["tags"] == ["project", "alpha"])
    r = sc(c.call("search_notes", {"query": "searchterm-xyz", "frontmatter": {"status": "draft"}}))
    t.check("query AND fm", [i["path"] for i in r["items"]] == ["projects/beta.md"])
    r = sc(c.call("search_notes", {"frontmatter_exists": {"reviewed": False}, "limit": 5}))
    t.check("exists:false filter pages", len(r["items"]) == 5 and r.get("next_cursor"))
    t.check("no criteria rejected", rpc_err(c.call("search_notes", {})) is not None)
    t.check("typed 42 matches", len(sc(c.call("search_notes", {"frontmatter": {"count": 42}}))["items"]) == 1)
    t.check("string '42' no match", sc(c.call("search_notes", {"frontmatter": {"count": "42"}}))["items"] == [])
    r = sc(c.call("search_notes", {"query": "keyword-alpha", "context_lines": 0}))
    t.check("context_lines=0 snippet", r["items"][0]["snippet"] == "keyword-alpha here.")

    # -- create_notes ------------------------------------------------------
    t.section("create_notes")
    r = sc(c.call("create_notes", {"notes": [{"path": "new/deep/note1.md", "content": "# Hello\nBody.\n", "frontmatter": {"n": 1, "tags": ["a"]}}]}))["created"][0]
    t.check("deep parent + fm", r["created"] is True)
    rn = sc(c.call("read_notes", {"paths": ["new/deep/note1.md"]}))["notes"][0]
    t.check("fm round-trip", rn["frontmatter"] == {"n": 1, "tags": ["a"]} and rn["content"] == "# Hello\nBody.\n")
    r = sc(c.call("create_notes", {"notes": [{"path": "simple.md", "content": "x"}, {"path": "new/note2.md", "content": "second\n"}]}))["created"]
    t.check("partial success", r[0]["created"] is False and r[0]["error"]["code"] == "CONFLICT" and r[1]["created"] is True)
    r = sc(c.call("create_notes", {"notes": [{"path": "new/badfm.md", "content": "---\nt: x\n---\nbody"}]}))["created"][0]
    t.check("leading --- rejected", r["created"] is False)
    r = sc(c.call("create_notes", {"notes": [{"path": "new/note2.md", "content": "replaced\n"}], "overwrite": True}))["created"][0]
    t.check("overwrite", r["created"] is True)
    r = sc(c.call("create_notes", {"notes": [{"path": "new/dir/", "content": "x"}]}))["created"][0]
    t.check("dir/ -> SUFFIX", r["created"] is False and r["error"]["code"] == "SUFFIX")
    t.check("dir/ left nothing on disk", not os.path.exists(os.path.join(vault, "new/dir")))
    r = sc(c.call("create_notes", {"notes": [{"path": "../escape.md", "content": "x"}]}))["created"][0]
    t.check("traversal", r["error"]["code"] == "TRAVERSAL")
    r = sc(c.call("create_notes", {"notes": [{"path": "new/dup.md", "content": "first\n"}, {"path": "new/dup.md", "content": "second\n"}]}))["created"]
    t.check("same path twice -> 2nd conflicts", r[0]["created"] and not r[1]["created"])
    t.check("first content kept", body("new/dup.md") == "first\n")
    r = sc(c.call("create_notes", {"notes": [{"path": "new/empty.md", "content": "", "frontmatter": {"nul": None, "nested": {"a": [1, "1"]}}}]}))["created"][0]
    rn = sc(c.call("read_notes", {"paths": ["new/empty.md"]}))["notes"][0]
    t.check("null/nested fm types", rn["frontmatter"] == {"nul": None, "nested": {"a": [1, "1"]}})
    r = sc(c.call("create_notes", {"notes": [{"path": "한글노트.md", "content": "twin"}]}))["created"][0]
    t.check("NFC twin of NFD -> CONFLICT", r["created"] is False and r["error"]["code"] == "CONFLICT")

    # -- append_notes ------------------------------------------------------
    t.section("append_notes")
    c.call("append_notes", {"appends": [{"path": "new/note2.md", "content": "A"}, {"path": "new/note2.md", "content": "B\n"}]})
    t.check("raw twice in order", body("new/note2.md") == "replaced\nAB\n")
    r = sc(c.call("append_notes", {"appends": [
        {"path": "new/absent1.md", "content": "x\n"},
        {"path": "new/absent2.md", "content": "y\n", "create_if_missing": True},
    ]}))["appended"]
    t.check("missing NOT_FOUND / create_if_missing", r[0]["appended"] is False and r[0]["error"]["code"] == "NOT_FOUND" and r[1]["appended"] is True)
    c.call("append_notes", {"appends": [{"path": "new/crlf-a.md", "content": "l1\r\nl2\r\n", "create_if_missing": True}]})
    t.check("CRLF -> LF", body("new/crlf-a.md") == "l1\nl2\n")
    c.call("append_notes", {"appends": [{"path": "nonl.md", "content": "MORE\n"}]})
    t.check("no separator on no-NL file", body("nonl.md") == "no newline at endMORE\n")
    r = sc(c.call("append_notes", {"appends": [{"path": "y/", "content": "x", "create_if_missing": True}]}))["appended"][0]
    t.check("dir/ create -> SUFFIX", r["appended"] is False and r["error"]["code"] == "SUFFIX")

    # -- edit_sections -----------------------------------------------------
    t.section("edit_sections")
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": "NEW.\n\n"}]}))
    t.check("replace body keeps subs", r["ok"] and "## A1" in body() and "NEW." in body())
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], "operation": "replace", "content": "GONE.\n\n"}]}))
    t.check("replace section removes subs", r["ok"] and "## A1" not in body())
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A"], "operation": "append", "scope": "body", "content": "TOLEAD\n"},
        {"path": "edit.md", "heading_path": ["B"], "operation": "append", "content": "TOSECTION\n"},
    ]}))
    b = body()
    t.check("append body before ## A1", r["ok"] and b.index("TOLEAD") < b.index("## A1"))
    t.check("append section at end", b.index("TOSECTION") > b.index("## B1"))
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A", "A1"], "operation": "delete"},
        {"path": "edit.md", "heading_path": ["B"], "operation": "delete", "scope": "body"},
    ]}))
    b = body()
    t.check("delete section / body", r["ok"] and "## A1" not in b and "# B" in b and "B lead." not in b and "## B1" in b)
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "insert_after", "content": "## A1.5\nnew.\n\n"}]}))
    b = body()
    t.check("insert_after sibling", r["ok"] and b.index("## A1.5") < b.index("## A2"))
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": [], "operation": "insert_before", "content": "PREP.\n\n"},
        {"path": "edit.md", "heading_path": [], "operation": "insert_after", "content": "\nEOF-APP.\n"},
    ]}))
    b = body()
    t.check("root prepend/append", r["ok"] and b.startswith("PREP.") and b.rstrip().endswith("EOF-APP."))
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "rename", "new_heading": "A-One"}]}))
    t.check("rename + new_heading_path", r["ok"] and r["applied"][0]["new_heading_path"] == ["A", "A-One"] and "## A-One" in body())
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["B", "B1"], "operation": "move", "destination": {"heading_path": ["A", "A1"], "position": "after"}}]}))
    b = body()
    t.check("same-level move", r["ok"] and r["applied"][0]["new_heading_path"] == ["A", "B1"] and b.index("## B1") < b.index("## A2"))
    reset_edit()
    for hp, dest, name in [
        (["B"], {"heading_path": ["A", "A1"], "position": "before"}, "h1->h2 slot"),
        (["A", "A1"], {"heading_path": ["B"], "position": "after"}, "h2->h1 slot"),
        (["A", "A1"], {"heading_path": [], "position": "after"}, "h2->root"),
    ]:
        r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": hp, "operation": "move", "destination": dest}]}))
        t.check(f"move level mismatch rejected ({name})", not r["ok"] and r["errors"][0]["code"] == "HEADING_LEVEL")
    t.check("file untouched after rejected moves", body() == BASE)
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], "operation": "move", "destination": {"heading_path": ["A", "A1"], "position": "after"}}]}))
    t.check("self-move OVERLAP", not r["ok"] and r["errors"][0]["code"] == "OVERLAP")
    for bad, name in [("Multi\nLine", "newline"), ("", "empty"), ("  ", "whitespace")]:
        r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], "operation": "rename", "new_heading": bad}]}))
        t.check(f"rename {name} -> INVALID_HEADING", not r["ok"] and r["errors"][0]["code"] == "INVALID_HEADING")
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "rename", "new_heading": "## Injected"}]}))
    t.check("rename literal hashes ok", r["ok"] and "## ## Injected" in body())
    reset_edit()
    h = sc(c.call("read_sections", {"targets": [{"path": "edit.md", "heading_path": ["A"], "scope": "body"}]}))["sections"][0]["content_hash"]
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": "V2.\n\n", "expected_hash": h}]}))
    t.check("correct hash applies + returns new hash", r["ok"] and r["applied"][0].get("content_hash"))
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["B"], "operation": "replace", "scope": "body", "content": "BV2.\n\n"},
        {"path": "edit.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": "V3.\n\n", "expected_hash": h},
    ]}))
    t.check("stale hash rejects whole batch", not r["ok"] and r["errors"][0]["code"] == "HASH_MISMATCH" and "B lead." in body())
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A"], "operation": "replace", "content": "x\n"},
        {"path": "edit.md", "heading_path": ["A", "A1"], "operation": "replace", "content": "y\n"},
    ]}))
    t.check("nested overlap rejected", not r["ok"] and all(e["code"] == "OVERLAP" for e in r["errors"]))
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": "lead2\n\n"},
        {"path": "edit.md", "heading_path": ["A", "A1"], "operation": "replace", "content": "a1-2\n\n"},
    ]}))
    t.check("body + child disjoint ok", r["ok"])
    reset_edit()
    for args, name in [({"operation": "replace"}, "replace w/o content"), ({"operation": "rename"}, "rename w/o new_heading"), ({"operation": "move"}, "move w/o destination")]:
        r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A"], **args}]}))
        t.check(f"{name} -> MISSING_CONTENT", not r["ok"] and r["errors"][0]["code"] == "MISSING_CONTENT")
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A"], "operation": "replace", "scope": "body", "content": "NOPE\n"},
        {"path": "simple.md", "heading_path": ["NoSuch"], "operation": "replace", "content": "x\n"},
    ]}))
    t.check("multi-file all-or-nothing", not r["ok"] and "NOPE" not in body())
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [
        {"path": "edit.md", "heading_path": ["A", "A1"], "operation": "replace", "content": "grew\nlots\nmore\n\n"},
        {"path": "edit.md", "heading_path": ["B", "B1"], "operation": "replace", "content": "B1new.\n\n"},
    ]}))
    b = body()
    t.check("snapshot disjoint edits", r["ok"] and "grew" in b and "B1new." in b and "## A2" in b)
    r = sc(c.call("edit_sections", {"edits": [{"path": "ghost.md", "heading_path": ["A"], "operation": "replace", "content": "x"}]}))
    t.check("missing note NOT_FOUND", not r["ok"] and r["errors"][0]["code"] == "NOT_FOUND")
    t.check("empty batch ok", sc(c.call("edit_sections", {"edits": []}))["ok"])
    reset_edit()
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "replace", "content": "t\n\n# Rogue\nx\n"}]}))
    t.check("content h1 in h2 slot rejected", not r["ok"] and r["errors"][0]["code"] == "HEADING_LEVEL")
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "insert_after", "content": "#### Deep\nx\n"}]}))
    t.check("insert level skip rejected", not r["ok"] and r["errors"][0]["code"] == "HEADING_LEVEL")
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "insert_after", "content": "## New\nx\n\n### Child\ny\n"}]}))
    t.check("valid nested insert ok", r["ok"])
    r = sc(c.call("edit_sections", {"edits": [{"path": "edit.md", "heading_path": ["A", "A1"], "operation": "insert_after", "content": "floating text\n\n"}]}))
    t.check("plain text insert ok", r["ok"])
    r = sc(c.call("edit_sections", {"edits": [{"path": "broken-fm.md", "heading_path": [], "operation": "append", "content": "appended.\n"}]}))
    t.check("broken-fm note editable", r["ok"])
    r = sc(c.call("edit_sections", {"edits": [{"path": "structured.md", "heading_path": ["Status", "Q1"], "operation": "replace", "content": "x\n"}]}))
    t.check("ambiguous no occ rejected", not r["ok"] and r["errors"][0]["code"] == "AMBIGUOUS")

    # -- edit_properties ---------------------------------------------------
    t.section("edit_properties")
    c.call("create_notes", {"notes": [{"path": "props.md", "content": "# T\nbody\n", "frontmatter": {"status": "draft", "n": 1}}], "overwrite": True})
    r = sc(c.call("edit_properties", {"edits": [
        {"path": "props.md", "key": "status", "value": "final"},
        {"path": "props.md", "key": "strnum", "value": "123"},
        {"path": "props.md", "key": "intnum", "value": 123},
        {"path": "props.md", "key": "nul", "value": None},
        {"path": "props.md", "key": "n"},
    ]}))
    text = raw("props.md")
    t.check("set types + remove", r["ok"] and 'strnum: "123"' in text and "intnum: 123" in text and "nul: null" in text and "n: 1" not in text)
    r = sc(c.call("edit_properties", {"edits": [{"path": "props.md", "key": "status", "value": "no-apply"}, {"path": "props.md", "key": "ghostkey"}]}))
    t.check("remove absent rejects batch", not r["ok"] and "status: final" in raw("props.md"))
    r = sc(c.call("edit_properties", {"edits": [{"path": "broken-fm.md", "key": "x", "value": 1}]}))
    t.check("broken YAML rejected", not r["ok"] and r["errors"][0]["code"] == "FRONTMATTER_PARSE")
    r = sc(c.call("edit_properties", {"edits": [{"path": "simple.md", "key": "added", "value": True}]}))
    t.check("creates fm block", r["ok"] and raw("simple.md").startswith("---\nadded: true\n---\n"))
    c.call("edit_properties", {"edits": [{"path": "simple.md", "key": "added"}]})
    t.check("removing last key drops block", raw("simple.md") == "Just a body, no frontmatter, no headings.\n")
    r = sc(c.call("edit_properties", {"edits": [{"path": "ghost.md", "key": "a", "value": 1}]}))
    t.check("missing note NOT_FOUND", not r["ok"] and r["errors"][0]["code"] == "NOT_FOUND")
    build_fixture(vault, "props2.md", "---\nweird:   'single'   # comment\nkeep: [1,2,3]\n---\nbody\n")
    c.call("edit_properties", {"edits": [{"path": "props2.md", "key": "new", "value": "x"}]})
    text = raw("props2.md")
    t.check("byte-preserves untouched keys", "weird:   'single'   # comment" in text and "keep: [1,2,3]" in text and "new: x" in text)
    r = sc(c.call("edit_properties", {"edits": [{"path": "한글노트.md", "key": "tag", "value": "ok"}]}))
    t.check("NFC path on NFD file", r["ok"])

    # -- delete/rename/relocate --------------------------------------------
    t.section("organize")
    c.call("create_notes", {"notes": [
        {"path": "org/a.md", "content": "a\n"}, {"path": "org/b.md", "content": "b\n"},
        {"path": "org/sub/c.md", "content": "c\n"}, {"path": "org/sub2/d.md", "content": "d\n"},
    ]})
    r = sc(c.call("delete_notes", {"paths": ["org/a.md", "org/ghost.md"]}))
    t.check("delete missing rejects batch", not r["ok"] and os.path.exists(os.path.join(vault, "org/a.md")))
    r = sc(c.call("delete_notes", {"paths": ["org/sub/", "org/sub/c.md"]}))
    t.check("delete overlap rejected", not r["ok"] and all(e["code"] == "OVERLAP" for e in r["errors"]))
    t.check("delete vault root rejected", not sc(c.call("delete_notes", {"paths": [""]}))["ok"])
    r = sc(c.call("delete_notes", {"paths": ["org/sub/"]}))
    t.check("delete dir with slash", r["ok"] and r["deleted"][0]["trashed_to"].startswith(".md-mcp/trash/"))
    t.check("delete note to trash", sc(c.call("delete_notes", {"paths": ["org/a.md"]}))["ok"])
    c.call("create_notes", {"notes": [{"path": "org/a.md", "content": "v2\n"}]})
    r = sc(c.call("delete_notes", {"paths": ["org/a.md"]}))
    t.check("delete trash collision suffixed", r["ok"] and r["deleted"][0]["trashed_to"].endswith(".1"))
    t.check("delete .md-mcp protected", not sc(c.call("delete_notes", {"paths": [".md-mcp/"]}))["ok"])
    r = sc(c.call("create_notes", {"notes": [{"path": ".md-mcp/evil.md", "content": "x"}]}))["created"][0]
    t.check("create .md-mcp protected", r["created"] is False and r["error"]["code"] == "TRAVERSAL")
    t.check("list .md-mcp empty", sc(c.call("list_notes", {"directory": ".md-mcp/"}))["items"] == [])
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/b.md", "new_name": "b2.md"}]}))
    t.check("rename basic (renamed field)", r["ok"] and r["renamed"][0]["to"] == "org/b2.md")
    t.check("rename md->no-ext SUFFIX", sc(c.call("rename_notes", {"renames": [{"path": "org/b2.md", "new_name": "b2"}]}))["errors"][0]["code"] == "SUFFIX")
    t.check("rename slash SUFFIX", sc(c.call("rename_notes", {"renames": [{"path": "org/b2.md", "new_name": "x/y.md"}]}))["errors"][0]["code"] == "SUFFIX")
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/sub2/", "new_name": "sub3"}]}))
    t.check("rename dir (/ echo)", r["ok"] and os.path.isdir(os.path.join(vault, "org/sub3")) and r["renamed"][0]["to"] == "org/sub3/")
    c.call("create_notes", {"notes": [{"path": "org/s1.md", "content": "1\n"}, {"path": "org/s2.md", "content": "2\n"}]})
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/s1.md", "new_name": "s2.md"}, {"path": "org/s2.md", "new_name": "s1.md"}], "overwrite": True}))
    t.check("rename swap rejected", not r["ok"] and any(e["code"] == "BATCH_COLLISION" for e in r["errors"]))
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/s1.md", "new_name": "s1.md"}]}))
    t.check("no-op rename CONFLICT", not r["ok"] and r["errors"][0]["code"] == "CONFLICT")
    c.call("create_notes", {"notes": [{"path": "org/coll.md", "content": "x\n"}]})
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/coll.md", "new_name": "b2.md"}]}))
    t.check("rename collision CONFLICT", not r["ok"] and r["errors"][0]["code"] == "CONFLICT")
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/coll.md", "new_name": "b2.md"}], "overwrite": True}))
    t.check("rename overwrite", r["ok"])
    r = sc(c.call("rename_notes", {"renames": [{"path": "org/ghost.md", "new_name": "g.md"}]}))
    t.check("rename missing source NOT_FOUND idx", not r["ok"] and r["errors"][0]["code"] == "NOT_FOUND" and r["errors"][0].get("index") == 0)
    c.call("create_notes", {"notes": [{"path": "org/e.md", "content": "e\n"}, {"path": "org/f.md", "content": "f\n"}]})
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/e.md", "dest_dir": "org/newd/"}, {"source": "org/f.md", "dest_dir": "org/newd/"}]}))
    t.check("relocate N->1 autocreate", r["ok"] and [m["to"] for m in r["moved"]] == ["org/newd/e.md", "org/newd/f.md"])
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/newd/", "dest_dir": "org/newd/in/"}]}))
    t.check("relocate own subtree rejected", not r["ok"] and r["errors"][0]["code"] == "OVERLAP")
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/newd/e.md", "dest_dir": "org"}]}))
    t.check("relocate dest no slash DEST_NOT_DIR", not r["ok"] and r["errors"][0]["code"] == "DEST_NOT_DIR")
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/newd/e.md", "dest_dir": "org/b2.md/"}]}))
    t.check("relocate dest occupied by note", not r["ok"] and r["errors"][0]["code"] == "DEST_NOT_DIR")
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/newd/e.md", "dest_dir": "org/"}, {"source": "org/sub3/d.md", "dest_dir": "org/newd/"}]}))
    t.check("relocate independent batch ok", r["ok"])
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/newd/f.md", "dest_dir": "/"}]}))
    t.check("relocate to vault root via '/'", r["ok"] and r["moved"][0]["to"] == "f.md")
    r = sc(c.call("relocate_notes", {"moves": [{"source": "org/ghost.md", "dest_dir": "org/"}]}))
    t.check("relocate missing source NOT_FOUND idx", not r["ok"] and r["errors"][0]["code"] == "NOT_FOUND" and r["errors"][0].get("index") == 0)
    r = sc(c.call("rename_notes", {"renames": [{"path": "한글노트.md", "new_name": "한글노트2.md"}]}))
    t.check("rename NFC path on NFD file", r["ok"])
    t.check("delete NFC-renamed file", sc(c.call("delete_notes", {"paths": ["한글노트2.md"]}))["ok"])

    # -- read-response size budget -----------------------------------------
    t.section("size budget")
    big = "# Big\n" + ("x" * 300_000) + "\n"
    c.call("create_notes", {"notes": [{"path": "big.md", "content": big}], "overwrite": True})
    r = sc(c.call("read_notes", {"paths": ["big.md", "empty.md"]}))
    t.check("read_notes drops big whole (omitted)", r.get("omitted") == [0] and [n["path"] for n in r["notes"]] == ["empty.md"])
    r = sc(c.call("read_notes", {"paths": ["big.md"], "include_body": False}))
    t.check("metadata-only read still works", "omitted" not in r and r["notes"][0]["exists"])
    r = sc(c.call("read_sections", {"targets": [{"path": "empty.md", "heading_path": []}, {"path": "big.md", "heading_path": []}]}))
    t.check("read_sections omitted", r.get("omitted") == [1] and len(r["sections"]) == 1)
