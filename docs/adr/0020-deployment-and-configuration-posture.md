# 20. Deployment: systemd + a single env file (no config-file format)

## Status

Accepted

Builds on [ADR-0013](0013-http-transport.md) (HTTP transport, no in-process TLS),
[ADR-0014](0014-oauth-authentication.md) (co-hosted OAuth behind a Cloudflare Tunnel),
and [ADR-0018](0018-git-automation.md)/[ADR-0019](0019-sync-health-and-push-retry.md)
(git automation the deployment must keep running unattended).

## Context

The server graduates from a dev-loop testbed to an always-on personal deployment:
one Linux host, one vault, exposed through a Cloudflare Tunnel to the claude.ai
connector and Claude Code. Two questions need settling:

1. **Process management.** The testbed's `nohup run.sh` does not survive reboots,
   restarts on crash, or centralize logs.
2. **Configuration format.** All configuration is currently ~14 flat `MD_*` env
   vars (plus the `GIT_CONFIG_*`/`GITHUB_PAT` git binding). Is a `config.toml`
   warranted before exposing this to production?

Facts that constrain the choice:

- The full surface is **flat scalars** — paths, one token, on/off flags, seconds.
  No nesting, no lists beyond comma-joined allowlists, no per-tool policy.
- systemd consumes env files natively (`EnvironmentFile=`); the dev loop consumes
  the same file with `set -a; . env`. A TOML file would need a new crate, a
  loader, and env-vs-file precedence rules in `config.rs` — which is currently
  pure, fail-closed env parsing with tests.
- The **git remote binding must be environment variables anyway**: `GIT_CONFIG_*`
  and the `GITHUB_PAT` credential helper work because the git child process
  inherits the server's environment. A TOML file would need re-export logic to
  feed them back into the environment.
- Secrets (bearer token, PAT) live in the file either way; the protection is file
  mode `0600`, not the format.

## Decision

We will deploy as a **systemd system service** reading **one env file**
(`/etc/md-mcp/env`, mode `0600`), fronted by **cloudflared** as its own service.
We will **not** introduce `config.toml` (or any config-file format). Sample
deployment assets live in `deploy/` — a hardened unit file, an annotated
`md-mcp.env.example`, and a step-by-step guide — kept dual-compatible: the same
env file parses under both shell `source` (dev `run.sh`) and systemd
`EnvironmentFile=` (values needing spaces are single-quoted, which both parsers
accept; neither expands `${…}`, which the runtime-expanded credential helper
requires).

Posture defaults encoded in the sample env: `MD_GIT_SYNC_INTERVAL_SECS` **on**
(the periodic fetch doubles as the recovery upper bound for ADR-0019 retry and
the debounce-starvation cap), `MD_GIT_AUTO_PUSH_SECS=30`, tunnel hostname in
`MD_HTTP_ALLOWED_HOSTS`, loopback bind (TLS terminates at the tunnel, per
ADR-0013/0014).

Revisit triggers for a config file (recorded so the next reviewer does not
re-litigate from scratch): multi-vault serving, per-tool policy, or the flat
surface outgrowing ~25 keys / needing nesting.

## Consequences

- Positive: crash restart, boot persistence, and journald logging come from
  systemd; zero new code or dependencies in the server; one file to back up and
  one file to secure; the dev testbed and production read the identical format.
- Positive: the git env binding keeps working unchanged in both worlds.
- Negative: env files carry no structure or comments-as-data; if the surface
  grows past the revisit triggers, migration to a config file becomes a real
  (if mechanical) chore.
- Negative: systemd's env-file parser is *almost* shell-compatible; the quoting
  convention in the sample must be respected (documented in `deploy/README.md`).
- Neutral: containerization is not precluded — the same env vars feed
  `docker run --env-file` unchanged.

### Considered and rejected

- **`config.toml` + toml crate** — a parser, a precedence story, and re-export
  logic for the git env binding, bought for a flat 14-key surface: complexity
  without a consumer.
- **CLI flags for everything** — flags leak secrets into `ps` output and process
  listings; env files do not.
- **systemd user service (`--user` + linger)** — fewer privileges to manage, but
  system units get standard boot ordering (`network-online.target`) and one
  journald stream; on a single-owner host the distinction buys nothing.
