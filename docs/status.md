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

## Stage 2 — next

`upstream/control_plane.rs` + `v1/{fleet,sessions,usage,inference,rig}.rs`,
per `CLAUDE.md`. `CONTROL_PLANE_TOKEN` is already in the droplet `.env`
and reaches the container. Exit: a jarvis session driven end to end through
`api.opustower.dev` with the `mac` key.

## Operating

- Deploy: merge to `main` here → CI builds the image → on the droplet
  `/opt/iron-fleet/deploy/droplet/deploy.sh` (pulls all three images).
- Keys: `cd /opt/iron-fleet/deploy/droplet && docker compose exec -T api opus-api keys list|create|revoke`.
- Logs: `docker logs droplet-api-1`; every line carries `request_id`, `method`,
  `path`, `key`.
