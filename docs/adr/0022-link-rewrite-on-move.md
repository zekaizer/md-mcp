# 22. Rewrite standard-Markdown links on move

## Status

Accepted

## Context

Moving or renaming a note leaves other notes untouched, so links pointing at it
silently break ([issue #2](https://github.com/zekaizer/md-mcp/issues/2)). The
[CONTEXT.md scope note](../../CONTEXT.md) bars *Obsidian-flavored syntax*, but that
means wikilinks (`[[Page]]`) — not CommonMark links `[text](dest)` / `![alt](dest)`
and reference definitions `[label]: dest`, which are pure Markdown. Fixing this
shifts the scope boundary, changes the tool contract, and commits to a
link-resolution convention — all hard to reverse once vaults and callers depend on
them. This ADR settles those; the discovery mechanism is implementation.

## Decision

We will maintain standard-Markdown links across moves, and amend the CONTEXT.md
scope note accordingly; wikilinks, block ids, and a backlink graph stay out of scope.

We will resolve a destination relative to the linking note's directory (root-absolute
from the vault root), matching the links real vaults contain — a vault-root-only
convention would match nothing useful. This maintains both inbound links from other
notes and a moved note's own outbound relative links.

We will expose this as an opt-in `update_links` flag (default `false`) on
`rename_notes` and `relocate_notes`, preserving current behavior and the minimal
surface ([ADR-0009](0009-delete-recovery-and-move-validation.md)); `delete_notes` is
excluded, having no valid new target. The response gains a `relinked` list, and
`dry_run` previews the rewrites without writing. Rewrites are atomic with the move:
one `commit_batch` ([ADR-0007](0007-multi-file-transaction.md)), one path-scoped
auto-commit ([ADR-0018](0018-git-automation.md)).

We will not maintain a link index. Inbound links are found on demand per batch; a
persistent backlink index is itself the out-of-scope backlink graph and adds
staleness and per-write maintenance.

## Consequences

- Positive: the gap closes for standard Markdown; no index means no staleness and no
  per-write cost; rewrites are atomic with the move; opt-in preserves defaults and
  the minimal surface.
- Negative: on-demand discovery is O(vault) per `update_links` batch; note-relative
  resolution widens a move's blast radius through outbound recompute; recognizing
  links correctly (code spans, fragments, reference definitions) risks corrupting
  notes if done naively.
- Neutral: the scope boundary shifts (standard links in, wikilinks out); with
  `update_links` off, nothing changes for existing callers.

### Considered and rejected

- **A persistent backlink index** — it is the out-of-scope backlink graph and adds
  staleness and write amplification.
- **Wikilink support** — non-CommonMark; reverses the pure-Markdown identity.
- **Root-absolute-only resolution** — simpler but inert against the relative links
  real vaults contain.
- **On by default** — silently reverses the minimal-surface posture.
- **Rewriting on `delete_notes`** — no valid new target; would hide breakage.
