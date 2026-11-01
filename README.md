# Opus Systems OS API

The front door to Opus Systems OS. One Rust binary, `opus-api`, at
`https://api.opustower.dev/v1`, in front of the agent fleet
([Iron-Fleet](https://github.com/Opus-Systems-OS/Iron-Fleet)). Every client
uses it: the Iron-Fleet desktop apps, the native Jarvis app, the web HUD
([J.A.R.V.I.S-Web](https://github.com/Opus-Systems-OS/J.A.R.V.I.S-Web)) and
the Meta Quest 3 app ([Opus-Systems-OS-Quest](https://github.com/Opus-Systems-OS/Opus-Systems-OS-Quest)).

```
clients ──osk_ key──> api.opustower.dev ──> control plane ──> Managed Agents (Anthropic)
                           │                      └─> rig (Ollama over Tailscale)
                           └─> Fish Audio, ops services, news and hazard feeds, briefing sources
```

It holds the control plane's token, never the Anthropic key, and owns one
thing: its clients' keys. Everything else is read live from upstream.

## What it serves

| Area | Routes (all under `/v1`) |
|---|---|
| Sessions | create, list, events, follow-ups, interrupt, SSE stream, WebSocket, client-executed tools |
| Files | upload, attach to messages, download an agent's outputs |
| Fleet | `fleet/agents`, `rig`, `usage` (+ CSV export), `inference/*` (Ollama on the rig) |
| Voice | `voice/speak`, `voice/transcribe`, `voice/credit` (Fish Audio) |
| Ops | `ops`, `ops/{service}`, `clients`: the stack's services at a glance, read-only |
| Intel | `intel/feed`, `intel/search`: news wires and hazard feeds, categorized and placed |
| Briefing | `sources`, `briefing`: weather, calendar, Gmail, YouTube, WHOOP, Buffer |
| Devices | `pair`: a device shows a code, the Mac approves it, the device collects its key once |
| Keys | `keys*` (`keys:admin`), `me`, `health` |

- Spec: `/v1/openapi.json` · docs: `/v1/docs`
- Design, routes, scopes, errors, rate limits: [`docs/api-design.md`](docs/api-design.md)
- Conventions and build order: [`CLAUDE.md`](CLAUDE.md) · where things stand: [`docs/status.md`](docs/status.md)

## Keys and scopes

`Authorization: Bearer osk_<id>_<secret>`. Keys are stored as SHA-256,
scoped, revocable, and may be limited to some agents. Scopes: `fleet:read`,
`sessions:read`, `sessions:write`, `usage:read`, `inference`, `voice`,
`ops:read`, `intel:read`, `sources:read`, `pair:approve`, `keys:admin`.
`keys:admin` is minted only on the host.

```sh
opus-api keys create --name mac --scopes fleet:read,sessions:read,sessions:write
opus-api keys create --name powers --scopes sessions:read,sessions:write --agents jarvis-powers
opus-api keys list
opus-api keys grant --id <id> --scopes ops:read      # never keys:admin
opus-api keys agents --id <id> --agents a,b          # or: all
opus-api keys revoke --id <id>
opus-api oauth start google|whoop                    # prints a consent URL
opus-api oauth status
```

On the droplet, run these with `docker compose exec -T api opus-api …`.

## Run locally

```sh
export CONTROL_PLANE_URL=http://localhost:8080 CONTROL_PLANE_TOKEN=… DATABASE_PATH=./opus-api.db
cargo run -- keys create --name dev --scopes fleet:read,sessions:read,sessions:write,usage:read,inference
cargo run -- serve                       # :8100
curl localhost:8100/v1/me -H "Authorization: Bearer osk_…"
```

`cargo test` runs the contract tests through the real router, with the
control plane stubbed. `OPUS_API_KEY=osk_… cargo run --example ws_drive -- wss://api.opustower.dev sesn_…`
drives one turn over the WebSocket, the reference client for headsets.

## Configuration

Environment only; every variable is in [`deploy/env.example`](deploy/env.example).
Required: `CONTROL_PLANE_URL`, `CONTROL_PLANE_TOKEN`. Common: `PORT` (8100),
`DATABASE_PATH`, `RATE_LIMIT_PER_MINUTE` (300), `ALLOWED_ORIGINS` (unset = no CORS).
Optional features switch on with their credentials and are absent from the
spec without them: Voice (`FISH_AUDIO_API_KEY`, `JARVIS_VOICE_ID`), Ops (one
read token per service, `DOCKER_SOCKET`), Briefing (`GOOGLE_*`, `WHOOP_*`,
`BUFFER_API_KEY`, `API_PUBLIC_URL`, `BRIEFING_*`).

## SDKs

- [`sdk/csharp`](sdk/csharp): hand-written `netstandard2.1` client (`OpusClient`, `SessionSocket`).
- [`sdk/upm/com.opussystems.api`](sdk/upm/com.opussystems.api): the same as a Unity package, by git URL.
- [`sdk/unity`](sdk/unity): a sample Unity project on it.

## Security

- Per-key rate limits, a smaller bucket on costly routes, and a per-address
  failure bucket checked before any key lookup; pairing is capped.
- WebSockets have message caps and an idle timeout, are re-checked for
  revocation every minute, and spend the key's limit per frame.
- Upstream failure text is logged with the request id, not returned.
- Downloads are always attachments with `nosniff`; outbound calls have
  timeouts and limited redirects; a request has 150 s to produce its head.
- CI runs `cargo audit` (also weekly), actions are pinned to SHAs, base images
  to digests, and Dependabot is on.

### OAuth tokens at rest

The briefing's Google and WHOOP tokens are encrypted in the database
(XChaCha20-Poly1305) when `TOKEN_KEY_FILE` names a file holding a 32-byte
key as 64 hex characters:

```sh
umask 077 && openssl rand -hex 32 > secrets/token_key   # mount it at /run/secrets/token_key
```

With it set, tokens stored before encryption are read as they are and
sealed in place on first use; nothing has to be re-consented. Without it,
tokens stay in plaintext and the API logs a warning at startup. Keep the
key apart from the database backups: either alone is useless, both together
are the Gmail token. Losing the key means running `opus-api oauth start`
again for each provider. The database file is `chmod 0600` on every start.

### Docker status without the socket

`DOCKER_SOCKET` may be a `tcp://host:port` proxy that allows only
`GET /containers`, so the engine socket need not be mounted into the API (a
read-only mount does not limit the Docker API).

## Deploy

CI builds `ghcr.io/opus-systems-os/opus-systems-os-api/api` on every push
to `main`. The droplet's compose project (in Iron-Fleet, `deploy/droplet/`)
runs it beside the control plane; Caddy terminates TLS for
`api.opustower.dev`. Then `deploy.sh` on the droplet pulls the images.
