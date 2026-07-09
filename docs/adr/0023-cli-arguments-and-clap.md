# 23. CLI arguments via clap, with env fallback

## Status

Accepted

## Context

The server parses only `--http` / `--stdio` by hand; every other setting comes
from an `MD_*` environment variable ([ADR-0020](0020-deployment-and-configuration-posture.md)
deliberately keeps configuration in a single env file, with no config-file format).
That posture is right for the systemd deployment but poor for interactive and
ad-hoc use: there is no `--help`, no `--version`, no discoverable flags, and a
typo in an env var surfaces late.

Exposing configuration on the command line adds a dependency (an argument parser)
and makes the CLI a second configuration surface beside env — a change to the
ADR-0020 posture worth recording. One constraint shapes it: a process's argv is
world-readable (`/proc/<pid>/cmdline`, `ps`), so secret material must never become
a plain value flag.

## Decision

We will adopt **clap** (derive) as the argument parser, giving `--help`,
`--version`, and validated, self-documenting flags.

We will expose each non-secret setting as a flag whose value falls back to its
existing `MD_*` variable (`#[arg(env = ...)]`), with precedence CLI > env >
default. The single-env-file deployment keeps working unchanged; this **amends**
ADR-0020's env-only posture to "CLI flags or env", and does not add a config-file
format (still rejected).

We will keep secrets out of argv. The bearer token has no value flag; it stays
env-only (`MD_HTTP_TOKEN`), with a `--http-token-file <path>` option that reads and
trims the secret from a file so only the *path* ever appears on the command line.

We will keep the fail-closed parsing semantics: values still validate strictly
(unknown transport, malformed allowlist, zero seconds, …) and reject rather than
silently default.

## Consequences

- Positive: `--help`/`--version` and discoverable, validated flags; precedence is
  native to the parser; misconfiguration is caught at parse time with a clear
  message; env-based deployment is untouched.
- Negative: a new dependency and its compile-time cost; two configuration surfaces
  to keep documented and consistent; secrets need the deliberate argv carve-out.
- Neutral: amends but does not supersede ADR-0020 — the env file remains first-class
  and no config-file format is introduced.

### Considered and rejected

- **A config-file format** — still rejected (ADR-0020); env + flags cover the need
  without a third surface.
- **A plain `--http-token` value flag** — leaks the secret through argv
  (`/proc`, `ps`); a token *file* path is the safe CLI form.
- **Hand-rolling an expanded parser** — reimplements help/version/precedence that
  clap provides and tests.
