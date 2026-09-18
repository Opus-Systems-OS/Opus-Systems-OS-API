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
| `keys:admin` | `/v1/keys*` |

Suggested grants: desktop apps — everything but `keys:admin`; a headset —
`sessions:read`, `sessions:write`, `inference`; the native Jarvis app —
`sessions:read`, `sessions:write`.

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
| `not_found` | 404 | No such route or resource |
| `method_not_allowed` | 405 | Route exists, method does not |
| `invalid_request` | 400 / 415 / 422 | Bad body, unknown field, bad parameter — or the control plane said so |
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

Stage 2 (next): `GET /v1/fleet/agents`; `POST/GET /v1/sessions`,
`GET /v1/sessions/{id}`, `GET/POST /v1/sessions/{id}/events`,
`GET /v1/sessions/{id}/stream` (SSE), `POST /v1/sessions/{id}/interrupt`;
`GET /v1/usage`, `GET /v1/usage/export.csv`; `GET /v1/inference/models`,
`POST /v1/inference/chat` (NDJSON when streaming), `POST /v1/inference/embeddings`;
`GET /v1/rig`.

Stage 3: `GET /v1/sessions/{id}/ws`.

## WebSocket protocol (stage 3, to be finalised then)

One socket per session. Server → client frames are the session's events as
JSON, the same objects the SSE stream carries. Client → server:

```json
{"type": "message", "task": "…"}
{"type": "interrupt"}
```

On connect the server sends history (oldest first), then live events;
frames carry the event `id` so a reconnecting client can dedupe.
