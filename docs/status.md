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

## Stage 4 — done 2026-09-22 (Mac half 2026-09-18 ~05:05 UTC)

The Tauri app talks to `https://api.opustower.dev/v1` with the `mac` key
(Iron-Fleet PR #30; `/agents` → `/fleet/agents`, `/inference/models` →
`/rig`). Caddy's `access-api.log` shows it polling `/v1/fleet/agents`,
`/v1/rig`, `/v1/sessions` → 200.

The Windows half followed at the rig on 2026-09-22 (Iron-Fleet #37 and the
plan's "Rig backlog"): the rig got its own key, `win-rig` `28033876`
(`fleet:read,sessions:read,sessions:write,usage:read,inference,voice,ops:read`),
in `%APPDATA%\com.ironfleet.app\control-plane.json` with `url`
`https://api.opustower.dev/v1`, and the Tauri app was rebuilt against it —
`/v1/me` → `win-rig`, four agents, `/v1/rig` online with both models, and
the app's 5 s poll at 200 in `access-api.log`. Caddy was then narrowed:
`fleet.opustower.dev` serves `/webhooks/*` and `/healthz` and 404s
everything else, so this API is the only front door and no laptop holds
`CONTROL_PLANE_TOKEN` as a client credential. Verified from the rig —
`/healthz` 200, `/agents` `/sessions` `/usage` 404, `GET` on the webhook
route 405 and an unsigned `POST` 400 in the control plane's log.

## Stage 5a — C# SDK done 2026-09-18 ~14:30 UTC

`sdk/csharp/OpusSystems.Api` (PR #6, `1745ebd`): hand-written
`netstandard2.1` client — `OpusClient` (REST) + `SessionSocket` (the
WebSocket protocol as C# events), Newtonsoft.Json the only dependency,
`OpusApiException` carrying the envelope. Not generated from the spec, on
purpose (`sdk/csharp/README.md` says why). CI builds and unit-tests it.

Exit test, live from the Mac (`OPUS_API_KEY=… OPUS_LIVE_SESSION=1 dotnet
test`): 7/7 — me/agents/rig/usage typed, errors typed, and a jarvis
session created through `OpusClient` then driven over `SessionSocket`
to `agent.message` ("19 times 21 is 399.") and `end_turn`, with
`LastReplyAsync` agreeing.

Found on the way: a malformed session id was `502 upstream` (Anthropic's
`invalid_request_error`, rendered 502 by the control plane). Fixed in
both: the API maps Anthropic's caller-side errors to `400
invalid_request` / `404 not_found` and makes type ↔ status always agree
(#6); the control plane keeps an Anthropic 400 a 400 (Iron-Fleet #32).
Both deployed.

## Stage 5b — done 2026-09-18 ~16:05 UTC

The native Jarvis app (`Opus-Systems-OS/Jarvis`, PR #1 `613d95b`) is a
fleet client. What it needed, now in the platform:

- **control plane** (Iron-Fleet #33 `1833dba`): `POST /sessions` takes
  `tools` (client-executed `custom` tools) and `system_suffix`, applied as
  `agent_with_overrides` on the agent's *live* definition (a `tools`
  override replaces in full, so the agent's own tools are restated); `POST
  /sessions/{id}/tool-results` answers `agent.custom_tool_use`. The agent
  resource never changes; other clients' sessions never see the tools.
- **API** (#8 `3a514de`): the same on `/v1/sessions`, validated before any
  round trip; WebSocket `tool_result` frame and `?deltas=true` → `delta`
  frames; C# SDK `CustomTool`/`ToolResult`/`OnDelta`/`SendToolResultAsync`.
- **App**: `OpusClient.swift` replaces the Anthropic client; SSE with
  `event_deltas=agent.message` feeds the speaker; music tools declared per
  session and run on the Mac; personality via `system_suffix`; model picker
  gone; images refused (upstream user messages are text-only).

Exit test, live on the Mac, `sesn_01XCKBqeihxVDr6J6RCDpjKk` (key
`jarvis-mac`, `sessions:read,sessions:write`): spoken streamed replies
("161."), then "play should i stay or should i go" → `agent.custom_tool_use
play_music` → `requires_action` → Mac ran it → `user.custom_tool_result
"Now playing … by The Clash."` → spoken confirmation. Five turns, 10 ¢,
visible in the fleet's Usage. Earlier, through the control plane alone: a
`get_device_time` tool round-trip and the suffix ("…, Sir.") for 6 ¢.

Parked: iOS build/signing (`Sources/Shared` compiles for it), the
Windows Python port (rig backlog), images (revisit if Managed Agents
documents image blocks for `user.message`).

## Voice — done 2026-09-19

`POST /v1/voice/speak` (scope `voice`) proxies Fish Audio with the key and
reference voice on the droplet; every client (Tauri, Swift, Quest) speaks
with it. Fish bills per character against the user's API credit.

## Ops — done 2026-09-20 ~01:10 UTC (#14, Iron-Fleet #36)

`GET /v1/ops` and `/v1/ops/{service}` (scope `ops:read`): GitHub,
UptimeRobot, the droplet (DigitalOcean), Docker (engine socket, `:ro` in
compose), Tailscale, Cloudflare — one read token each in the droplet
`.env`, 30 s cache, failures as `down` rows. Live on first deploy: six
rows, five `ok`, Tailscale `warn` (the rig is off); the DO metrics agent
is on, so the droplet row carries load and memory. `CreateSession.client`
→ metadata `iron_fleet_client` (for the Mac answering the headset's
music tools). Keys re-minted with `ops:read`: `mac` `7d26eb91`, `quest-3`
`9c44f721` (old `quest-3` `89d4f0f9` to revoke once the headset is
confirmed on the new one). The GitHub token is a classic `ghp_` — swap
for a fine-grained read-only one when convenient.

## What's next

The API's build order is complete. Open threads, none blocking:

- `GET /v1/fleet/environments` with queue stats (`workers_polling`,
  depth) — needs a control-plane route; verify the Managed Agents
  environments shape first.
- ~~A Unity sample against the C# SDK~~ — done 2026-09-18 (#10,
  `sdk/unity/JarvisSample`, Unity 6000.6.2f1): EditMode 3/3 incl. live
  `Me`/`Rig`, PlayMode 1/1 with a real turn streamed over the WebSocket
  under Unity's runtime. Next for the Quest: Android Build Support in the
  Hub, Meta XR/OpenXR packages, a world-space panel + speech in place of
  the IMGUI console. No headset was available; everything runs in the
  editor.
- A per-key allowed-tools policy if a client should be limited to a
  declared tool set.
- Whatever real use turns up.

## Operating

- Deploy: merge to `main` here → CI builds the image → on the droplet
  `/opt/iron-fleet/deploy/droplet/deploy.sh` (pulls all three images).
- Keys: `cd /opt/iron-fleet/deploy/droplet && docker compose exec -T api opus-api keys list|create|revoke`.
- Logs: `docker logs droplet-api-1`; every line carries `request_id`, `method`,
  `path`, `key`.
