# Status — resume here

Written for whoever picks this up next, on any machine. Verified fact, not
plan. The build order is in `CLAUDE.md`; the contract in `api-design.md`.

## Stage 1 — done 2026-09-18 ~03:54 UTC

`api.opustower.dev` is live: Caddy-terminated TLS on the Iron-Fleet droplet,
`api` compose service from `ghcr.io/opus-systems-os/opus-systems-os-api/api`
(public package), keys DB on the `droplet_api_data` volume and in the nightly
backup bundle (Iron-Fleet PR #27).

Exit test, live: `GET /v1/me` → 401 envelope without a key; admin key
`d8f69546` (`ops-droplet`, `keys:admin`) minted with
`docker compose exec -T api opus-api keys create` on the box → 200; a `mac`
device key `d17aae7c` (`fleet:read,sessions:read,sessions:write,usage:read,inference`)
created over `POST /v1/keys` → usable at once, 403 on `/v1/keys`; `/v1/docs`
and `/v1/openapi.json` 200. The `mac` key is at
`~/.config/opus-systems/api-key` on the Mac (0600). No key plaintext is
stored anywhere else.

Gotchas found:

- **Caddyfile is a single-file bind mount**: `git pull` replaces the inode,
  the running container keeps the old one, and `caddy reload` says "config
  is unchanged". After any Caddyfile change:
  `docker compose up -d --force-recreate caddy`. (Recorded in Iron-Fleet's
  plan too.)
- **Dockerfile dependency cache**: `COPY` keeps source mtimes, so the dummy
  `lib.rs` built for the dependency layer can look *newer* than the real
  one and cargo keeps the empty lib. The Dockerfile removes the crate's
  `.fingerprint` as well as its `deps`.

## Stage 2 — done 2026-09-18 ~04:17 UTC

`d040822` deployed. Every fleet route is behind the door with scopes,
validation before any round trip, byte-for-byte streams, and upstream
errors in our vocabulary (PR #2; 23 tests against a stub control plane that
401s anything but `CONTROL_PLANE_TOKEN`).

Exit test, live from the Mac with the `mac` key: `/v1/fleet/agents` (four
agents, cent-string caps), `/v1/rig` → `online:false` with the connect
error as `reason` (rig off), `/v1/inference/models` → `503 rig_offline` +
`Retry-After: 5`, `/v1/usage` + CSV matching the control plane's, then
jarvis session `sesn_017LJ8vSBDwouo8dqssuajS2`: `POST /v1/sessions` → 201,
history via `/events?types=`, `GET /stream` → `: connected` then live
`data:` frames, `POST /events` → 200, `POST /interrupt` → 200 with the
`user.interrupt` appended, `GET /sessions/{id}` → idle, 6 ¢ of 50 ¢.

CI now runs fmt/clippy/tests on pull requests; the image builds only from
`main` (the PR for stage 2 initially had no CI at all).

## Stage 3 — done 2026-09-18 ~04:45 UTC

`97b39fd` deployed (PR #4). `GET /v1/sessions/{id}/ws` bridges the control
plane's SSE + POST routes into one socket of typed JSON frames (protocol in
`api-design.md`); per-key token bucket, 300/min default; CORS only for
`ALLOWED_ORIGINS` (none set). 34 tests incl. a real WebSocket client
against the served app.

Exit test, live: `cargo run --example ws_drive -- wss://api.opustower.dev
sesn_017LJ8vSBDwouo8dqssuajS2` with the `mac` key — `hello`, `message`
sent, live `event` frames through to `agent.message` ("17 times 23 is
391.") and `session.status_idle` in 3.1 s, `ping`/`pong`, clean close.
`api/examples/ws_drive.rs` is the reference client: what a headset does,
minus the headset.

## Stage 4 — next

Move the Tauri app onto the API (`Iron-Fleet/app`): base URL
`https://api.opustower.dev/v1`, a per-device key instead of the shared
control-plane token, `/inference/models` → `/v1/rig`. Then Caddy exposes
only `/webhooks/*` + `/healthz` on `fleet.opustower.dev`. Exit: both
desktop apps on the API; Usage tab identical.

## Operating

- Deploy: merge to `main` here → CI builds the image → on the droplet
  `/opt/iron-fleet/deploy/droplet/deploy.sh` (pulls all three images).
- Keys: `cd /opt/iron-fleet/deploy/droplet && docker compose exec -T api opus-api keys list|create|revoke`.
- Logs: `docker logs droplet-api-1`; every line carries `request_id`, `method`,
  `path`, `key`.
