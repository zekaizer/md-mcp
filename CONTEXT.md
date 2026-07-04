# md-mcp

> **md-mcp** is a provisional working name.

An MCP server that exposes a single vault of pure-Markdown notes (`.md` files with
optional YAML frontmatter) to AI agents, with structure-aware tools — outline
reads, section-level edits, frontmatter properties — rather than treating the
vault as a flat pile of files.

This file is the **glossary**: the ubiquitous language every identifier, comment,
and document should use. It defines terms and links out for depth — it does not
restate behavior. For the full behavioral contract see the
[tool specification](docs/tool_spec.md); for the rationale behind each design
choice see the [architecture decisions](docs/adr/README.md).

Scope note: md-mcp is **pure Markdown + YAML frontmatter only**. Obsidian-flavored
syntax — wikilinks, block ids, backlinks, tags-as-graph — is out of scope.

## Vault & notes

**vault**:
The single root directory md-mcp serves — the whole managed note tree. Every path
an agent gives is vault-relative.
_Avoid_: repository, workspace, folder.

**note**:
A single Markdown file (`.md`) in the vault: optional YAML frontmatter followed by
a Markdown body. The unit addressed, read, and written.
_Avoid_: file, document, page.

**body**:
A note's Markdown content with the leading frontmatter block excluded. What read
and section tools operate on.

**frontmatter**:
The leading YAML block fenced by `---` … `---` at the very start of a note. Not a
section; edited only via `edit_properties`. A leading `---` with no closing `---`
is body, not frontmatter. See [tool_spec §용어 정의](docs/tool_spec.md).
_Avoid_: front-matter, metadata block, YAML header.

**property**:
One top-level key/value pair in the frontmatter. The unit `edit_properties`
sets or removes.
_Avoid_: field, attribute, key (loosely).

## Structure & addressing

**heading**:
An ATX heading line (`#` … `######`); `level` is the count of `#`. Setext
headings and `#` inside code fences are not headings. NOT a note's identity —
changing a heading is an edit, not a rename.
_Avoid_: header, title.

**section**:
A heading plus everything beneath it down to the next same-or-higher heading —
its lead body and all nested subsections. The unit a structured read or edit
addresses. (Markdown has no native "section"; md-mcp defines it.)
_Avoid_: block, chunk.

**lead body**:
A heading's own text from just after the heading line to the first deeper heading
— the body before any subsection.
_Avoid_: intro, preface.

**subsection**:
A section nested at a deeper level inside another section.

**preamble** / **root**:
The body before the first heading (after frontmatter). Addressed by the empty
heading path `[]`, which denotes the note's whole body as one root section.

**heading path**:
A section's address: the chain of ancestor heading texts from an outermost
ancestor down to the target (e.g. `["Design", "Schema"]`). Compared after
Unicode NFC normalization, case-sensitively.
_Avoid_: path, selector, breadcrumb.

**occurrence**:
A 1-based disambiguator used only when one heading path matches more than one
heading (identical ancestor chains). A heading is **ambiguous** when it needs one.
_Avoid_: index, nth.

**scope**:
Which span of a section a read or edit touches: `body` = lead body only;
`section` = lead body plus all subsections. Shared by `read_sections` and
`edit_sections` so a read and its follow-up edit address identical bytes. See
[ADR on section read/edit scope](docs/adr/0003-document-parser-and-section-model.md).

**outline**:
A note's table of contents — its headings only, in document order, without bodies.
What `read_outlines` returns.
_Avoid_: TOC, structure, tree.

**content hash**:
A hash of a section's content for the chosen `scope`, over LF-normalized bytes,
excluding the target heading line — section-granular, never whole-file. Produced
by `read_sections`/`edit_sections`, consumed by `edit_sections.expected_hash` for
optimistic concurrency (a mismatch rejects the batch).
_Avoid_: etag, checksum, version.

## Identity & moving

**rename**:
Changing a note's or directory's basename in place (same parent). The
`rename_notes` tool; 1:1, no path change.
_Avoid_: retitle, move.

**relocate**:
Moving a note or directory into a different directory, keeping its basename. The
`relocate_notes` tool; N notes → 1 directory.
_Avoid_: move (unqualified).

**path suffix convention**:
A note path ends in `.md`, a directory path ends in `/` — the suffix alone tells
note from directory, across every tool.

## Failure model & durability

**destructive tool**:
A tool that removes or overwrites existing content — `edit_sections`,
`edit_properties`, `rename_notes`, `relocate_notes`, `delete_notes`. A
destructive batch is **all-or-nothing**: one rejected item rejects the whole
batch and nothing is written. See [ADR on the transaction model](docs/adr/0007-multi-file-transaction.md).
_Avoid_: dangerous, unsafe.

**non-destructive tool**:
A tool that only adds or reads — every read plus `create_notes` and
`append_notes`. A non-destructive batch is **partial success**: a failing item
never sinks its siblings.
_Avoid_: safe.

**batch**:
The list of items one tool call carries (a single item is a one-element list).
Bounded at 100 items.

**error envelope**:
The structured rejection shape every tool shares —
`{ index, item?, operation?, code, message }` per violation — reporting all
detected violations at once, with a machine-readable `code`. See
[tool_spec §출력 envelope](docs/tool_spec.md).
_Avoid_: error string, message blob.

**atomic write**:
Writing a note by creating a temp file in the same directory and `rename(2)`-ing
it over the target, so a crash never leaves a partial file.

**transaction**:
The server-internal guarantee that a destructive multi-file batch lands entirely
or not at all: back up originals, write all temps, commit by bulk rename, roll
back on any failure, and recover an incomplete batch on restart. See
[ADR on the transaction model](docs/adr/0007-multi-file-transaction.md).
_Avoid_: commit, savepoint.

**trash**:
Where deleted notes go instead of being erased — a hidden location outside the
note namespace, mirroring the original path, never clobbering a prior entry — so a
delete is recoverable. `delete_notes` reports the trash destination.
_Avoid_: bin, recycle, graveyard.

**internal state**:
The server's own files — transaction journal, backups, trash — kept outside the
note namespace (a hidden `.md-mcp/` directory) so `list_notes`/`search_notes`
never surface them.

## Concurrency

**write serialization**:
Writes are serialized and the commit step is exclusive with reads, so a multi-note
read never sees a torn snapshot (some notes new, some old). External-change
detection is a separate layer, handled by `content hash`. See
[ADR on concurrency](docs/adr/0008-concurrency-and-isolation.md).

**vault lock**:
The cross-process OS lock (`.md-mcp/lock`) held around every transaction commit
and git operation, so a cooperating external tool never interleaves with a
mid-batch tree. See [ADR on git sync](docs/adr/0016-git-sync-integration.md).
_Avoid_: flock (as a noun), mutex.

## Sync & events

**sync**:
Replicating the vault through its git repository — commit, rebase onto the
upstream, push. Git is a replication layer above the transaction; the journal,
not git, is the durability layer. See [ADR on git sync](docs/adr/0016-git-sync-integration.md)
and [ADR on git automation](docs/adr/0018-git-automation.md).
_Avoid_: backup, mirror.

**sweep commit**:
`sync`'s commit of everything dirty at sync time (`mcp(sync): checkpoint`).
With `auto-commit` on it contains only external edits.

**auto-commit**:
The opt-in per-batch git commit: one write batch, one path-scoped commit, made
while the write guard is held. See [ADR on git automation](docs/adr/0018-git-automation.md).

**event journal**:
The opt-in append-only stream (`.md-mcp/events.jsonl`) of vault mutations: one
record per destructive batch and per succeeded non-destructive item, `seq`-ordered,
at-least-once, best-effort complete. See [ADR on the event journal](docs/adr/0017-event-journal-and-hook.md).
_Avoid_: log (unqualified), audit trail.

**commit hook**:
The configured command run once per event record with the record JSON on stdin;
push delivery only — the journal is the catch-up path.
_Avoid_: webhook, callback.
