# Deploying md-mcp (ADR-0020)

One Linux host, one vault, exposed through a Cloudflare Tunnel. Components:

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
out. The `branch.<name>.remote/merge` keys in the env file must match that
branch (`main` in the sample).

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

Operational notes: push failures surface as `sync_warning` on write responses
and retry on a capped backoff; a backlog also drains at service start
(ADR-0019). Keep `MD_GIT_SYNC_INTERVAL_SECS` on — it is the recovery upper
bound. Rotating the PAT or the bearer token is an env-file edit +
`systemctl restart md-mcp` (the claude.ai connector re-authorizes against the
new gate token only when its refresh token expires or is revoked — issued
tokens stay valid until their own expiry).
