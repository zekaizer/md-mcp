# 11. Error envelope and structured output

## Status

Accepted

## Context

[tool_spec](../tool_spec.md) requires every tool to return **structured output**
(MCP `outputSchema`) with explicit branch fields (`exists`, `found`,
`content_hash`, `error`), and a uniform rejection envelope
`{ ok: false, errors: [{ index, item?, operation?, code, message }] }` that
reports **all** detected violations at once — so an agent fixes everything in one
round trip instead of failing repeatedly. The `code` is machine-readable
([ADR-0005 et al.](README.md)); the failure model differs by tool kind
(partial-success for non-destructive, all-or-nothing for destructive).

## Decision

We will return structured JSON from every tool via rmcp's `Json<T>` wrapper
(emitting `outputSchema` + `structuredContent`), and reserve rmcp protocol errors
(`ErrorData`) for unexpected/internal failures only — never for business
rejections, which travel in the structured body.

- **Shared error item.** One `ApiError { code, message, index? }` (code from
  `md_core::Code::as_str`) is embedded wherever a tool reports a failure.
- **Non-destructive tools** (`read_*`, `create_notes`, `append_notes`) return
  per-item results, each carrying its own optional `error`/`exists`/`found`; one
  bad item never sinks the others.
- **Destructive tools** (`edit_sections`, `edit_properties`, `rename_notes`,
  `relocate_notes`, `delete_notes`) return `{ ok, ... , errors }`: on success
  `ok: true` with the applied results and an empty `errors`; on rejection
  `ok: false` with **every** violation in `errors` and nothing applied. The whole
  batch is validated (resolve targets, dry-run patches, check overlaps/collisions
  and `expected_hash`) before any write; only an all-valid batch reaches
  `Vault::commit_batch` ([ADR-0007](0007-multi-file-transaction.md)).
- **Indices** in `errors` are the offending item's position in the request array,
  so the agent can correlate and fix each one.

Schema constraints expressible in JSON Schema (e.g. `maxItems: 100`) are declared
via `schemars` attributes and rejected by the framework before the envelope logic
runs; server-logic violations use the envelope.

## Consequences

- Positive: one consistent, machine-parseable contract across all tools;
  all-violations-at-once avoids fix-retry round trips; clean separation between
  protocol errors and business rejections.
- Negative: each tool defines a response struct (more types), the cost of
  schema-precise structured output.
- Neutral: response structs derive `serde::Serialize` + `schemars::JsonSchema`
  (via rmcp's re-export, `#[schemars(crate = "rmcp::schemars")]`) so the
  `outputSchema` matches the wire shape exactly.
