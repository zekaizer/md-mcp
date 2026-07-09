# Deploying md-mcp (ADR-0020)

One Linux host, one vault, exposed through a Cloudflare Tunnel. Day-to-day
management goes through **[`mdctl`](mdctl)** (steps 1–3 below are
`sudo deploy/mdctl install` + `mdctl edit`):

```
mdctl install     first-time setup: binary + unit + env skeleton
mdctl update      rebuild, install (keeping a .prev), restart, health-check
mdctl rollback    swap the .prev binary back, restart, health-check
mdctl start|stop|restart
mdctl status      unit state + health probe + sync backlog + recent warns
mdctl logs [...]  follow the journal
mdctl edit        edit the env file, then offer a restart
mdctl token       print a fresh bearer token
```

Components:

- `md-server` — systemd system service, loopback HTTP (`127.0.0.1:7654`)
- `cloudflared` — its own systemd service, terminates TLS and forwards
  `https://<hostname>` → `http://127.0.0.1:7654`

## 1. Binary

```sh
cargo build --release
sudo install -m 0755 target/release/md-server /usr/local/bin/md-server
```

## 2. Environment file

```sh
sudo mkdir -p /etc/md-mcp
sudo cp deploy/md-mcp.env.example /etc/md-mcp/env
sudo chown <user>: /etc/md-mcp/env && sudo chmod 0600 /etc/md-mcp/env
$EDITOR /etc/md-mcp/env   # fill MD_HTTP_TOKEN, GITHUB_PAT, repo URL, hostname
```

Quoting rules (the file feeds both shell `source` and systemd
`EnvironmentFile=`): single-quote any value containing spaces; never depend on
`${…}` expanding at load time — the credential helper's `${GITHUB_PAT}` is
expanded by git at runtime, by design.

The vault must be a clone of its GitHub repo with the synced branch checked
out and tracking its upstream. The clone's own `.git/config` supplies the
remote and branch binding, so the env only injects the PAT credential helper —
it must **not** re-declare `remote.origin.*` or `branch.<name>.*`, or git sees
a duplicate `branch.<name>.merge` and refuses to push ("multiple upstream
branches").

## 3. Service

```sh
sudo cp deploy/md-mcp.service /etc/systemd/system/
# adjust User=, Group=, and both ReadWritePaths= to MD_VAULT / MD_STATE_DIR
sudo systemctl daemon-reload
sudo systemctl enable --now md-mcp
journalctl -u md-mcp -f
```

## 4. Cloudflare Tunnel

```sh
cloudflared tunnel login
cloudflared tunnel create md-mcp
cloudflared tunnel route dns md-mcp <hostname>
```

`/etc/cloudflared/config.yml`:

```yaml
tunnel: <tunnel-id>
credentials-file: /root/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: <hostname>
    service: http://127.0.0.1:7654
  - service: http_status:404
```

```sh
sudo cloudflared service install
sudo systemctl enable --now cloudflared
```

Do **not** put a Cloudflare Access policy on this hostname — it intercepts the
OAuth handshake and breaks the claude.ai connector (ADR-0014).

## 5. Clients

- **claude.ai** (web/mobile): Settings → Connectors → Add custom connector →
  `https://<hostname>/mcp`. The consent page asks for the access token once —
  paste `MD_HTTP_TOKEN`. Tokens persist in `MD_STATE_DIR/oauth-state.json`
  across restarts.
- **Claude Code**:

  ```sh
  claude mcp add --transport http md-mcp https://<hostname>/mcp \
    --header "Authorization: Bearer <MD_HTTP_TOKEN>"
  ```

## 6. Verify

```sh
# discovery serves and reflects the public hostname
curl -s https://<hostname>/.well-known/oauth-authorization-server | jq .issuer
# tool surface is guarded
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://<hostname>/mcp   # 401
# sync is healthy: write something from a client, then
journalctl -u md-mcp --since -5min | grep -i warn   # expect nothing
git -C <vault> log --oneline -3                     # mcp(...) commits, pushed
```

## 7. Logs → VictoriaLogs (optional, ADR-0021)

With `MD_LOG_FORMAT=json` the server emits one JSON object per line to stderr;
systemd captures them in the journal. Ship the journal with systemd's own
uploader — VictoriaLogs accepts it natively (no collector needed):

```sh
sudo apt install systemd-journal-remote
# /etc/systemd/journal-upload.conf
#   [Upload]
#   URL=http://<vl-host>:9428/insert/journald
sudo systemctl enable --now systemd-journal-upload
```

Query in VictoriaLogs (the JSON lives in `_msg`; unit/host are stream fields
by default):

```
_time:1h _SYSTEMD_UNIT:"md-mcp.service" | unpack_json | duration_ms:>100
_time:1h _SYSTEMD_UNIT:"md-mcp.service" | unpack_json | level:"WARN"
```

Note: `systemd-journal-upload` ships the whole host journal; swap in vector
with a unit filter if that is unwanted. To also ship the mutation audit
stream (ADR-0017 events), point the commit hook at the jsonline endpoint —
the `Content-Type` header is required (VictoriaLogs silently ignores bodies
sent as form data):

```
MD_ON_COMMIT_HOOK='curl -s -X POST -H "Content-Type: application/stream+json" --data-binary @- "http://<vl-host>:9428/insert/jsonline?_stream_fields=app&app=md-mcp-events"'
```

Operational notes: push failures surface as `sync_warning` on write responses
and retry on a capped backoff; a backlog also drains at service start
(ADR-0019). Keep `MD_GIT_SYNC_INTERVAL_SECS` on — it is the recovery upper
bound. Rotating the PAT or the bearer token is an env-file edit +
`systemctl restart md-mcp` (the claude.ai connector re-authorizes against the
new gate token only when its refresh token expires or is revoked — issued
tokens stay valid until their own expiry).
