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

## Deploy

CI builds `ghcr.io/opus-systems-os/opus-systems-os-api/api` on every push
to `main`. The droplet's compose project (in Iron-Fleet, `deploy/droplet/`)
runs it beside the control plane; Caddy terminates TLS for
`api.opustower.dev`.
