# 21. Structured logging: JSON lines to stderr, shipped via journald to VictoriaLogs

## Status

Accepted

Builds on [ADR-0020](0020-deployment-and-configuration-posture.md) (systemd
deployment — journald capture is what makes "stderr only" a complete story) and
[ADR-0017](0017-event-journal-and-hook.md) (the mutation event stream, which
rides the same pipeline for free).

## Context

The deployment host runs a VictoriaLogs instance (verified live: v1.50.0, the
`/insert/journald` and `/insert/jsonline` endpoints are enabled by default on
`:9428`). Server logs should land there structured — queryable by field, not by
regex over prose. Today the `tracing` output is human-formatted text, and
several key sites interpolate values into the message string
(`"auto-push failed: {e}"`), which would survive a format switch as one opaque
`message` field. There is also no per-request record of tool calls — the
latency measurements that motivated ADR-0019 had to be taken from the outside.

Constraint: the server must not own log *transport* (buffering, retry,
backpressure toward a possibly-down VictoriaLogs). ADR-0016 set the pattern —
delegate to ambient infrastructure.

## Decision

Three layers, with the app responsible only for the first:

1. **Format: `MD_LOG_FORMAT=text|json`** (default `text`; fail-closed on other
   values). `json` switches `tracing-subscriber` to one flattened JSON object
   per line on stderr (`flatten_event`, no span noise) — an existing
   dependency's feature flag, no new crate. Key log sites are normalized to
   structured fields (`error = %e`, `retry_in_secs`, `conflicts`, `pulled`/
   `pushed`) instead of message interpolation, and every completed sync that
   moved anything logs one `sync applied` line.
2. **Access log:** an axum middleware on `/mcp` (outermost, so 401s are
   captured) emits one `INFO md_server::access` line per request: HTTP method,
   JSON-RPC method, tool name (for `tools/call`), status, `duration_ms`.
   Bodies ≤ 256 KiB are buffered to sniff `method`/`params.name` — the server
   fully buffers request bodies downstream anyway; larger bodies stream
   through unsniffed and just lack the `rpc`/`tool` fields.
3. **Transport: journald → `systemd-journal-upload` → VictoriaLogs
   `/insert/journald`.** systemd's own uploader, no third-party collector; the
   default stream fields (`_MACHINE_ID`, `_HOSTNAME`, `_SYSTEMD_UNIT`) give
   per-unit filtering out of the box. The JSON in `_msg` is expanded at query
   time with `unpack_json`. The ADR-0017 mutation stream can ship directly via
   `MD_ON_COMMIT_HOOK` + `curl` to `/insert/jsonline` — with an explicit
   `Content-Type: application/stream+json`, verified necessary: VictoriaLogs
   answers 200 but silently ingests nothing when the body arrives as form data.

## Consequences

- Positive: field-level queries (`duration_ms:>100`, `level:"WARN"`,
  `tool:"create_notes"`) over every request and every sync outcome; the app
  gains zero network dependencies; a VictoriaLogs outage costs nothing — the
  journal keeps buffering on disk.
- Positive: dev output is unchanged (text default); the JSON switch is one env
  var already flowing through the ADR-0020 env file.
- Negative: `systemd-journal-upload` ships the whole host journal, not just
  this unit; acceptable on a single-owner host (and the other units' logs are
  arguably wanted too). A unit filter means swapping in vector — recorded as
  the upgrade path, not the default.
- Negative: request-body sniffing buffers up to 256 KiB per request in the
  middleware; bounded and small against the 4 MiB per-note write cap.
- Neutral: the text format keeps working everywhere; nothing changes for stdio
  transport users (logs already went to stderr).

### Considered and rejected

- **In-app shipping (custom tracing layer POSTing to VictoriaLogs)** — the app
  would own buffering/retry/backpressure, and a log-sink outage would become a
  server concern; journald does all of this for free.
- **`tracing-journald` (native journal fields end-to-end)** — keeps structure
  without JSON-in-message, but adds a dependency and does nothing outside
  systemd (dev, containers); JSON lines + query-time `unpack_json` is portable.
- **vector/fluent-bit as the default collector** — warranted only for unit
  filtering or ingest-time parsing; overkill for one host and one unit today.
