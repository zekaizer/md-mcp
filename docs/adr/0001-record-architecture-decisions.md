# 1. Record architecture decisions

## Status

Accepted

## Context

md-mcp is a new MCP server that exposes a single vault of pure-Markdown notes
(`.md` + YAML frontmatter) to AI agents, per the tool specification in
[`docs/tool_spec.md`](../tool_spec.md). Before any feature code lands, several
foundational decisions must be made — implementation language and runtime, the
markdown/section parser, the frontmatter serializer, the multi-file transaction
mechanism, the delete-recovery model, the concurrency model, and the testing and
benchmarking strategy. Each is a load-bearing choice that later work will depend
on and that a reviewer (or a future maintainer, including the author months from
now) will want the rationale for.

We need a durable, reviewable record of *why* each choice was made — not just the
choice — so decisions are not silently reversed and their trade-offs stay
visible. The decisions are versioned alongside the code in the same repository so
they travel with it and are reviewed in the same pull requests.

## Decision

We will record every architecturally significant decision as an Architecture
Decision Record (ADR) using the Michael Nygard format, in `docs/adr/`, one file
per decision named `NNNN-kebab-title.md` with a monotonically increasing number.

- **Template (Nygard):** each ADR has `## Status`, `## Context`, `## Decision`
  (written in the active voice — "We will …"), and `## Consequences` (positive,
  negative, and neutral outcomes all stated).
- **Timing:** an ADR is written *before* the code that implements its decision,
  and is included in the same pull request as that code.
- **Scope — an ADR is required for:** choosing a library or framework, changing
  the public tool/API surface, changing the folder/module structure, changing the
  build system, and changing a non-functional requirement (performance, security,
  durability, deployment model).
- **Immutability:** an `Accepted` ADR is not edited after the fact. If a decision
  changes, the old ADR's status becomes `Deprecated` or `Superseded by ADR-N`
  and a new ADR is written. (Exception: an ADR on a not-yet-merged branch may be
  rewritten in place — soft-reset the branch and re-author it correctly rather
  than stacking a superseding ADR.)

## Consequences

- Positive: the reasoning behind the architecture is preserved and reviewable;
  decisions are deliberate and hard to reverse by accident; new contributors (and
  agents) can reconstruct intent from the repository alone.
- Negative: a small, ongoing documentation cost on every significant change.
- Neutral: ADRs accrete as a numbered, append-mostly log; the index in
  `docs/adr/README.md` keeps them navigable.
