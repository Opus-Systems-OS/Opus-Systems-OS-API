# Opus Systems OS API

The front door to Opus Systems OS. One API, every client: the Iron-Fleet
desktop apps (Mac/Windows), the native Jarvis app (Mac/iOS/Windows), and the
Meta Quest 3 app (Unity/C#). Rust, axum, one binary: `opus-api`.

```
Mac app ─┐
Win app ─┤
Jarvis ──┼─> api.opustower.dev (this) ─┬─> control-plane (Iron-Fleet, internal)
Quest ───┘                              │     ├─> Managed Agents (Anthropic)
                                        │     └─> rig-gpu (Ollama over Tailscale)
                                        └─> (later) Anthropic-direct reads
```

The API holds **`CONTROL_PLANE_TOKEN` only** — never the Anthropic key. It
owns one thing: its clients' keys. Everything else it serves is read live
from upstream. It lives on the same droplet as the control plane and talks to
it over the compose network.

## Rules

- **`/v1` is the contract.** Additive changes only. Breaking → `/v2`. The
  OpenAPI document is generated from the handlers (`utoipa_axum`) and served
  at `/v1/openapi.json` and `/v1/docs`; a route that is not in the spec does
  not exist. The C# SDK is generated from the spec.
- **One error shape, everywhere:** `{"error":{"type","message","request_id"}}`.
  Types: `unauthorized`, `forbidden`, `not_found`, `invalid_request`,
  `method_not_allowed`, `rate_limited`, `upstream`, `rig_offline`, `internal`.
  `x-request-id` on every response and in every log line. axum's own
  rejections are folded into the envelope by `request_id::layer`.
- **Keys, not a token.** `osk_<id>_<secret>`, SHA-256 at rest, scopes,
  revocable. `keys:admin` is minted only by `opus-api keys create` on the
  host — never over HTTP.
- **Scopes:** `fleet:read`, `sessions:read`, `sessions:write`, `usage:read`,
  `inference`, `keys:admin`. A device key never gets `keys:admin`.
- **No budget or fleet mutation, ever.** No route creates agents or
  environments or raises a cap. Same rule as `mcp-fleet`.
- **Headsets get a WebSocket.** C# has `ClientWebSocket` and no SSE. SSE
  passthrough stays for the desktop apps.
- Conventions as in Iron-Fleet: `thiserror` enum → `IntoResponse`,
  `deny_unknown_fields` on request bodies, `tracing` with `EnvFilter`, tests
  through the real router with an in-memory SQLite, upstream stubbed with an
  axum server in tests. `cargo fmt`, `cargo clippy --all-targets -D warnings`,
  `cargo test` — CI runs all three before building the image.

## Layout

```
api/src/main.rs          CLI: serve (default) | keys create|list|revoke
api/src/lib.rs           app(): /v1 + request-id + trace layers
api/src/config.rs        PORT, DATABASE_PATH, CONTROL_PLANE_URL, CONTROL_PLANE_TOKEN
api/src/error.rs         Error → status + envelope
api/src/request_id.rs    x-request-id, span, final error body
api/src/db.rs            SQLite: api_keys; migrate()
api/src/auth/keys.rs     mint/parse/verify, Scope
api/src/auth/middleware.rs  authenticate → Principal; require(scope)
api/src/v1/              one file per area, each an OpenApiRouter
api/src/openapi.rs       title, security scheme, ErrorBody
api/tests/v1.rs          contract tests
api/Dockerfile           two-stage, same shape as Iron-Fleet's
deploy/env.example       variables; on the droplet they live in Iron-Fleet's deploy/droplet/.env
docs/api-design.md       the contract in prose: envelope, scopes, versioning, WS protocol
```

Deployment config (compose service, Caddy host) lives in
**Iron-Fleet/deploy/droplet** — one compose project per box.

## Build order

Do not start a stage before the one above it works live.

1. **Skeleton that deploys** — keys, envelope, request ids, OpenAPI, `/v1/me`.
   Exit: `GET https://api.opustower.dev/v1/me` with a minted key → 200.
2. **The fleet through the door** — `/v1/fleet/agents`, `/v1/sessions*`
   (incl. SSE stream), `/v1/usage*`, `/v1/inference/*`, `/v1/rig`.
   Exit: a jarvis session started, messaged, streamed and interrupted via
   the API with a `sessions:*` key.
3. **Realtime for headsets** — `GET /v1/sessions/{id}/ws`, per-key rate
   limits, CORS. Exit: a WebSocket client drives a jarvis turn.
4. **Clients migrate** — the Tauri app on the API with a per-device key;
   `fleet.opustower.dev` serves only Anthropic's webhook.
5. **SDK + native Jarvis** — C# SDK from the spec; the Swift app's direct
   Anthropic calls replaced with `/v1/sessions` so its spend lands in Usage.

## Secrets

`CONTROL_PLANE_TOKEN` and the database live on the droplet. Minted keys are
printed once and never stored in plaintext. Nothing secret is ever committed.
