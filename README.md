# Opus Systems OS API

The front door to Opus Systems OS: the agent fleet (Iron-Fleet), its
sessions and usage, local inference on the rig, and — next — a Meta Quest 3
client. One Rust binary, `opus-api`, at `https://api.opustower.dev/v1`.

- Spec: `/v1/openapi.json` · docs: `/v1/docs`
- Design: [`docs/api-design.md`](docs/api-design.md)
- Conventions and build order: [`CLAUDE.md`](CLAUDE.md)

## Run locally

```sh
export CONTROL_PLANE_URL=http://localhost:8080 CONTROL_PLANE_TOKEN=… DATABASE_PATH=./opus-api.db
cargo run -- keys create --name dev --scopes fleet:read,sessions:read,sessions:write,usage:read,inference
cargo run -- serve                       # :8100
curl localhost:8100/v1/me -H "Authorization: Bearer osk_…"
```

`cargo test` runs the contract tests through the real router.
`OPUS_API_KEY=osk_… cargo run --example ws_drive -- wss://api.opustower.dev sesn_…`
drives one turn over the WebSocket — the reference client for headsets.

## Deploy

CI builds `ghcr.io/opus-systems-os/opus-systems-os-api/api` on every push
to `main`. The droplet's compose project (in Iron-Fleet, `deploy/droplet/`)
runs it beside the control plane; Caddy terminates TLS for
`api.opustower.dev`.

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
