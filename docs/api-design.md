# API design

The contract in prose. The machine-readable version is `/v1/openapi.json`;
when they disagree, the spec wins and this document is wrong.

## Base URL and versioning

`https://api.opustower.dev/v1`. The version is in the path so a client can
be pinned by its base URL alone. Within `/v1`, only additive changes: new
routes, new optional fields, new enum values on *responses* where the field
is documented as open. Removing or renaming anything, changing a type, or
tightening validation is a `/v2`.

## Authentication

`Authorization: Bearer osk_<id>_<secret>`.

| Part | Form | Role |
|---|---|---|
| `osk_` | literal | Recognisable in logs and secret scanners |
| `id` | 8 hex | Public. Looked up directly; appears in logs and listings |
| `secret` | 64 hex | 32 random bytes. Stored only as SHA-256; compared in constant time |

A key has a `name` (who holds it), a set of scopes, and can be revoked. All
failures — no header, malformed, unknown id, wrong secret, revoked — answer
`401 unauthorized` with the same message; the API never says which.

`keys:admin` keys are minted only by `opus-api keys create` on the host.
`POST /v1/keys` refuses to grant it.

## Scopes

| Scope | Grants |
|---|---|
| `fleet:read` | `GET /v1/fleet/*`, `GET /v1/rig` |
| `sessions:read` | `GET /v1/sessions*`, event history, SSE / WebSocket read side |
| `sessions:write` | `POST /v1/sessions`, send message, interrupt, WebSocket send side |
| `usage:read` | `GET /v1/usage`, `GET /v1/usage/export.csv` |
| `inference` | `/v1/inference/*` |
| `voice` | `/v1/voice/*` — Jarvis's voice (text to speech) |
| `ops:read` | `/v1/ops*` — the stack's services at a glance, read-only |
| `pair:approve` | `POST /v1/pair/{code}/approve` — approve a new device (held by the Mac, never a device) |
| `keys:admin` | `/v1/keys*` |

Suggested grants: desktop apps — everything but `keys:admin`; a headset —
`sessions:read`, `sessions:write`, `fleet:read`, `voice`, `ops:read`; the
native Jarvis app — `sessions:read`, `sessions:write`, `voice`.

There is no scope for creating agents or environments or changing budgets,
because there are no such routes.

## Errors

Every non-2xx body:

```json
{"error": {"type": "forbidden", "message": "missing scope `keys:admin`", "request_id": "req_088a2cbff136b2e0"}}
```

| `type` | Status | Meaning |
|---|---|---|
| `unauthorized` | 401 | Key missing, malformed, unknown, wrong, or revoked |
| `forbidden` | 403 | Key lacks the scope; message names it |
| `not_found` | 404 | No such route, session, agent or environment — ours or Anthropic's |
| `method_not_allowed` | 405 | Route exists, method does not |
| `conflict` | 409 | The environment exists but is not provisioned yet |
| `invalid_request` | 400 / 415 / 422 | Bad body, unknown field, bad parameter, malformed id — ours, the control plane's, Anthropic's or Ollama's (an unknown model is one of these) |
| `rate_limited` | 429 | `Retry-After` header in seconds |
| `rig_offline` | 503 | Inference asked for while the rig is off; `Retry-After` |
| `upstream` | 502 | The control plane could not be reached or answered unexpectedly |
| `internal` | 500 | Ours. Quote the request id |

`request_id` equals the `x-request-id` response header. A client may send
its own `x-request-id` (≤ 64 chars of `[A-Za-z0-9_-]`) and it will be
honoured; anything else is replaced with `req_<16 hex>`.

## Request ids and logs

Every log line for a request carries `request_id`, `method`, `path` and,
once authenticated, `key` (the public id). Nothing logs a secret.

## Routes

Stage 1 (live):

| Route | Scope | Notes |
|---|---|---|
| `GET /v1/health` | — | Liveness of the API process only |
| `GET /v1/me` | any key | The presented key: id, name, scopes |
| `POST /v1/keys` | `keys:admin` | Returns the plaintext once |
| `GET /v1/keys` | `keys:admin` | Never shows hashes |
| `DELETE /v1/keys/{id}` | `keys:admin` | 204; 404 if absent or already revoked |
| `GET /v1/openapi.json`, `GET /v1/docs` | — | The spec, and Redoc over it |

Stage 2 (live):

| Route | Scope | Notes |
|---|---|---|
| `GET /v1/fleet/agents` | `fleet:read` | `{data:[…]}`, the registry as synced; caps are cent strings |
| `GET /v1/rig` | `fleet:read` | `{configured, online, models, reason}` — always 200 |
| `POST /v1/sessions` | `sessions:write` | `{agent_slug, task, environment?, repositories?, tools?, system_suffix?}` → 201 |
| `GET /v1/sessions` | `sessions:read` | `?agent_slug&limit&page&order`; Anthropic's page envelope |
| `GET /v1/sessions/{id}` | `sessions:read` | The session object + `console_url` |
| `GET /v1/sessions/{id}/events` | `sessions:read` | `?page&limit&types&order`; `order=desc&limit=1` = the latest |
| `POST /v1/sessions/{id}/events` | `sessions:write` | `{task}` — a follow-up; resumes an idle session |
| `POST /v1/sessions/{id}/tool-results` | `sessions:write` | `{results:[{custom_tool_use_id, content, is_error?}]}` — answers `agent.custom_tool_use` |
| `POST /v1/sessions/{id}/interrupt` | `sessions:write` | Appends `user.interrupt` |
| `GET /v1/sessions/{id}/stream` | `sessions:read` | SSE, byte-for-byte from upstream; `?event_deltas=` |
| `GET /v1/usage` | `usage:read` | `?since&until`; `{window, by_agent, recent}` incl. `last_error` |
| `GET /v1/usage/export.csv` | `usage:read` | Audit CSV, oldest first |
| `GET /v1/inference/models` | `inference` | Ollama `/api/tags` |
| `POST /v1/inference/chat` | `inference` | Ollama `/api/chat` body; NDJSON unless `stream:false` |
| `POST /v1/inference/embeddings` | `inference` | Ollama `/api/embed` body |
| `POST /v1/voice/speak` | `voice` | `{text (1–2000), format?: mp3|wav|pcm|opus, latency?: low|normal|balanced}` → audio bytes, streamed |
| `POST /v1/voice/transcribe` | `voice` | one utterance as the raw body, `content-type` `audio/wav` (16 kHz mono recommended), `audio/mpeg` or `audio/mp4`, ≤ 1 MiB; `?language=en` → `{text, duration}`. Fish ASR; WebM is refused (Fish can't decode it). What was said is never logged. |
| `GET /v1/voice` | `voice` | `{configured, voice_id, model}` |
| `GET /v1/ops` | `ops:read` | `{services: [{id, name, state, headline, checked_at}]}` — the configured services, hub order |
| `POST /v1/pair` | none | Start pairing a device: `{code, token, expires_in}` (10 min; 10 starts/min per address) |
| `GET /v1/pair/{code}?token=` | none | `202` until approved; then `{api_key, key_id, name, wit_token?, speaker?}` once |
| `POST /v1/pair/{code}/approve` | `pair:approve` | `{name, wit_token?, speaker?}` → mints the device key (fixed profile), revokes older keys of that name |
| `GET /v1/ops/{service}` | `ops:read` | The same row with `detail` — `github`, `uptimerobot`, `droplet`, `docker`, `tailscale`, `cloudflare` |

### Voice

Jarvis's voice is one Fish Audio voice (`reference_id`), configured on the
server (`JARVIS_VOICE_ID`, `FISH_AUDIO_MODEL`) with the provider key
(`FISH_AUDIO_API_KEY`) held there. Clients send text and get audio; they
cannot choose a voice, so every client sounds the same — the Mac app, the
desktop app, a headset. The routes exist only when the key is configured
(otherwise 404, and they are absent from the spec). Provider failures are
`502 upstream` with a plain message (rejected key, exhausted credits) or
`503` with `Retry-After` when the provider is overloaded. Fish Audio bills
per character; the per-key rate limit bounds a runaway client.

### Pairing

A new device never has a key typed into it. It calls `POST /v1/pair` (no
key), shows the six-digit `code` on screen and keeps the `token`. A human
types the code into a client that holds `pair:approve` (the Mac's Jarvis
app), which calls `approve` with the device's name and whatever extras the
device needs — the Wit.ai token for dictation, the Mac's `host:port` for
music. Approval mints a key with the **fixed device profile**
(`fleet:read, sessions:read, sessions:write, voice, ops:read` — the body
cannot ask for scopes, and `keys:admin` is unreachable this way), revokes
any older key of the same name (so re-pairing rotates), and parks the
bundle under the code. The device polls `GET /v1/pair/{code}?token=…` and
collects it exactly once; a code without its token collects nothing.
Codes live ten minutes in memory.

### Ops

The stack's own services, read-only, for a launcher panel: GitHub (the
org's repos — open PRs, latest workflow run, unread notifications),
UptimeRobot (monitors, uptime ratios, response time), the droplet
(DigitalOcean — status, IPs, load and memory when the metrics agent is
on), Docker on the droplet (containers, over the engine socket mounted
read-only), Tailscale (devices, online by `lastSeen`), Cloudflare (zones and
DNS records). Each is a row `{id, name, state: ok|warn|down|unknown,
headline, checked_at}`; `/v1/ops/{service}` adds `detail`, a document whose
shape is per service (see `upstream/ops/*.rs`). One read token per service
lives on the server (`UPTIMEROBOT_API_KEY`, `TAILSCALE_API_KEY`,
`CLOUDFLARE_API_TOKEN`, `DIGITALOCEAN_TOKEN`, `GITHUB_TOKEN`,
`DOCKER_SOCKET`); a service without a token is simply not listed, and with
none the routes don't exist. A service is polled at most every 30 s however
many clients ask; one that rejects its token or cannot be reached is a
`down` row carrying the reason — the hub itself never fails because one
service did. Tokens are never returned, only what is derived from them.

### Client-executed tools

A client may declare tools that *it* runs — a headset's hand tracking, a
laptop's Music app — on the session it creates: `tools: [{type: "custom",
name, description, input_schema}]`. They are session-local: the agent is
untouched and other clients' sessions never see them. When the agent calls
one, the session emits `agent.custom_tool_use {id, name, input}` and idles
with `stop_reason.type = "requires_action"`; the client runs the tool and
answers with `POST /sessions/{id}/tool-results` (or the WebSocket
`tool_result` frame), and the turn continues. `system_suffix` is the same
idea for the prompt: a client's persona or device context, appended to the
agent's system prompt for that session only.

Session and event objects are Anthropic's, passed through unchanged; their
shape is Anthropic's contract. Everything the API composes itself
(`fleet/agents`, `rig`, `usage`, the create-session request and response)
is typed in the spec. Request bodies reject unknown fields.

Stage 3 (live):

| Route | Scope | Notes |
|---|---|---|
| `GET /v1/sessions/{id}/ws` | `sessions:read` (+ `sessions:write` to send) | WebSocket; protocol below. `?history=false` skips history; `?deltas=true` adds text fragments |

## WebSocket protocol

One socket per session. The bearer goes in the `Authorization` header of
the upgrade request (Unity's `ClientWebSocket.Options.SetRequestHeader`).
A refused upgrade is an ordinary HTTP error with the envelope: 401, 403,
404 (no such session), 400 (not an upgrade request).

Every frame, both directions, is a JSON object with a `type`.

Server → client, in order:

| Frame | When |
|---|---|
| `{"type":"hello","session_id","request_id"}` | First |
| `{"type":"event","event":{…}}` | Each session event — the same object `/events` and the SSE stream carry. History first (oldest first) unless `?history=false`, then live. Deduplicated on `event.id`: a client never sees the same event twice |
| `{"type":"delta","event_id","text"}` | With `?deltas=true`: a fragment of an `agent.message` as it is generated. The full `event` follows and is authoritative — speak the deltas, store the event |
| `{"type":"sent","data":[…]}` | Answer to `message` / `tool_result` / `interrupt`: the events appended |
| `{"type":"error","error":{"type","message"}}` | A client frame was rejected (bad JSON, unknown type, empty task, missing scope, upstream error). The socket stays open |
| `{"type":"pong"}` | Answer to `ping` |
| `{"type":"closed","reason"}` | Last frame before the server closes. `upstream_closed` = the session's stream ended (session terminated or expired) |

Client → server:

| Frame | Needs |
|---|---|
| `{"type":"message","task":"…"}` | `sessions:write` — a follow-up; resumes an idle session |
| `{"type":"tool_result","custom_tool_use_id","content","is_error"?}` | `sessions:write` — answers an `agent.custom_tool_use` event |
| `{"type":"interrupt"}` | `sessions:write` |
| `{"type":"ping"}` | — |

The server opens the live stream *before* listing history, so an event
emitted between the two is buffered and delivered once, after history.
A reconnecting client simply connects again; with `history=true` it gets
the full transcript, with `history=false` only what happens next.

## Rate limits

Per key, token bucket: `RATE_LIMIT_PER_MINUTE` (default 300) is both the
sustained rate and the burst. Over it → `429 rate_limited` with
`Retry-After` in seconds. Public routes (`/v1/health`, the spec, the docs)
are not limited. A WebSocket counts once, at the upgrade.

## CORS

`ALLOWED_ORIGINS` (comma-separated exact origins) enables CORS for those
origins only: methods GET/POST/DELETE, headers `authorization`,
`content-type`, `x-request-id`; exposes `x-request-id`, `retry-after`,
`content-disposition`. Unset → no CORS headers at all, which is right for
native clients.
