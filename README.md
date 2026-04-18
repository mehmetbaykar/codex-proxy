# codex-proxy

OpenAI-compatible proxy in front of the ChatGPT-backed Codex upstream
(`chatgpt.com/backend-api/codex/responses`). Exposes `/v1/chat/completions`,
`/v1/responses`, `/v1/responses/ws`, `/v1/files*`, and `/v1/models` so Cursor
and other OpenAI-compat clients can talk to it directly.

## Deploy (Docker)

1. `mkdir -p ~/docker-apps/codex-proxy && cd ~/docker-apps/codex-proxy`
2. Copy `docker-compose.yml` and `.env.default` from this repo into that folder.
3. `cp .env.default .env` and set `CURSOR_PROXY_API_KEY` (required).
4. `docker compose up -d`
5. Sign in to ChatGPT from inside the container:
   ```
   docker exec -it codex-proxy codex-proxy login
   ```
   Open the printed URL on any browser, enter the short code, approve.
   Tokens are written to `./volume/auth/auth.json` and refreshed
   automatically from then on (transparent to clients).

Host port defaults to `3000`; change `HOST_PORT` in `.env` if nginx/Traefik
needs a different one. Container always listens on `3000` internally.

### Nginx (VPS reverse proxy)

SSE streams and websockets need explicit nginx tweaks — buffering off, HTTP/1.1
upgrade headers, long read timeout. Minimal `server` block:

```nginx
server {
    listen 443 ssl http2;
    server_name codex.example.com;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;

        # WebSocket upgrade for /v1/responses/ws
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        # SSE: no buffering or it stalls until the stream ends
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;

        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

### Logs

All state lives under `./volume/` on the host:

```
volume/
├── auth/auth.json         # if you mounted it
├── db/proxy.sqlite3       # file-lifecycle metadata
├── files/                 # uploaded /v1/files bytes
└── logs/
    ├── requests.jsonl     # every inbound request (matched + unmatched)
    ├── upstream.jsonl     # upstream open/complete + timings
    └── payloads/          # populated only when IS_DEBUG=1
        ├── <req-id>-raw-client-body.json
        └── <req-id>-sanitized-upstream-body.json
```

Tail live traffic: `tail -f volume/logs/requests.jsonl | jq`.
Enable full payload dumps: set `IS_DEBUG=1` in `.env`, `docker compose restart`.

## Dev (from source)

Requires Rust stable.

```
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
PORT=3000 CURSOR_PROXY_API_KEY=dev-key ./target/release/codex-proxy
```

State root defaults to `./.state/`. Override with `STATE_ROOT=/path`.

## Env vars

| Var | Default | Purpose |
| --- | --- | --- |
| `CURSOR_PROXY_API_KEY` | unset | Static bearer clients must present. If unset, auth is disabled. |
| `HOST_PORT` | `3000` | Host-side port in docker-compose. |
| `PORT` | `3000` | Container listen port. |
| `BIND_ADDR` | `0.0.0.0` | Container bind host. |
| `LISTEN_ADDR` | — | Overrides `BIND_ADDR`+`PORT` when set. |
| `STATE_ROOT` | `.state` (bin) / `/app/state` (container) | Root for auth/db/files/logs. |
| `CODEX_UPSTREAM_URL` | `https://chatgpt.com/backend-api/codex/responses` | Upstream endpoint. |
| `IS_DEBUG` | `0` | `1` turns on verbose tracing + raw payload dumps. |
| `PROXY_LOG_FULL_BODY` | `0` | Same as `IS_DEBUG=1` payload dumping, without the debug tracing filter. |
| `CODEX_MODEL_ALIASES` | `gpt-5.3-codex-spark-preview=gpt-5.3-codex-spark` | Extra client→canonical model rewrites. Format: `a=b,c=d`. |
| `CODEX_UPSTREAM_IDENTITY_ENCODING` | `0` | `1` to send `accept-encoding: identity` upstream. |
| `CODEX_UPSTREAM_TRANSPORT_TUNING` | `0` | `1` to enable larger connection pool + HTTP/2 keepalive on the reqwest client. |

## Publishing

- `.github/workflows/ci.yml` — runs `cargo clippy -D warnings` and `cargo test` on every push and PR.
- `.github/workflows/docker-publish.yml` — builds `linux/amd64` and pushes to
  `mehmetbaykar/codex-proxy` on GitHub release publish. Tags emitted: `latest`,
  the release ref (e.g. `v0.1.0`), and the short git SHA.
- Required repo secrets: `DOCKERHUB_USERNAME`, `DOCKERHUB_TOKEN`.
