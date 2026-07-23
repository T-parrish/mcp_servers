# mcp_servers

A Cargo workspace of [MCP](https://modelcontextprotocol.io) servers written in Rust with
[`rmcp`](https://crates.io/crates/rmcp), sharing one build, one `.env`, and one telemetry stack.

| Crate | What it is |
|-------|------------|
| [`spotify_mcp_server`](spotify_mcp_server) | Log in via OAuth (PKCE), list your playlists, list a playlist's tracks. Official Spotify Web API. |
| [`bandcamp_mcp_server`](bandcamp_mcp_server) | Search artists and songs, add to cart. Bandcamp's undocumented internal API. |
| [`mcp_core`](mcp_core) | Library shared by both servers: telemetry setup, OpenTelemetry instruments, the outbound rate limiter, and tool-result helpers. |

Each server has its own README with tools, authentication and configuration — this file covers only
what the workspace shares.

## Build

```sh
cargo build --release              # every server, into the shared ./target
cargo build --release -p spotify_mcp_server   # just one
```

## Run & connect to Claude

`run.sh <server>` starts a server with the repo root as its working directory, so the shared `.env` is
found. Register both with Claude Code (user scope = available in every project):

```sh
claude mcp add -s user spotify  -- "$PWD/run.sh" spotify
claude mcp add -s user bandcamp -- "$PWD/run.sh" bandcamp
claude mcp list                    # both should report ✔ Connected
```

For Claude Desktop or other clients, use the same wrapper as the `command` with the server name in
`args` — see each server's *Use with an MCP client* section. The wrapper exists because MCP clients
set a command but not a working directory; running from the root is what lets one `.env` serve every
server.

## Configuration: one `.env` at the root

Copy `.env.example` to `.env` (gitignored) and edit. It is shared by every server; the variables are
namespaced (`SPOTIFY_*`, `BANDCAMP_*`) with `RUST_LOG` and the `OTEL_*` settings common to all. A
server ignores the others' variables.

`OTEL_SERVICE_NAME` is deliberately **not** set there: one value cannot name two services, so each
binary defaults `service.name` to its own name (`spotify-mcp` / `bandcamp-mcp`). Set it only to
override a single server for a one-off run.

## Telemetry

`docker-compose.yml` runs one [`grafana/otel-lgtm`](https://github.com/grafana/docker-otel-lgtm)
container (Grafana + Tempo + Prometheus + Loki) shared by every server:

```sh
docker compose up -d               # Grafana at http://localhost:3000 (admin/admin), OTLP on :4318
```

Set `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318` (already in `.env.example`) and the servers
export spans and metrics there, kept apart by `service.name`. Details — instrument names, the trace
model, and querying — are in each server's *OpenTelemetry export* section.

## Layout

```
mcp_servers/
  Cargo.toml            [workspace] members + [workspace.dependencies]
  .env / .env.example   one config file for every server (gitignored .env)
  claude.md             coding guidelines
  .claude/settings.json shared Claude Code settings
  docker-compose.yml    the shared telemetry stack
  run.sh                ./run.sh {spotify|bandcamp}
  mcp_core/             shared library crate
  spotify_mcp_server/   binary crate
  bandcamp_mcp_server/  binary crate
  target/               shared build output
```

### Adding a server

Create a binary crate, add it to `members` in the root `Cargo.toml`, and take shared dependencies from
`[workspace.dependencies]` with `<dep>.workspace = true`. In `main`, initialise telemetry with
`mcp_core::telemetry::init(mcp_core::service_info!("<name>-mcp", "<metric_prefix>"))`, then add a
`case` for it in `run.sh`. Reuse `mcp_core`'s rate limiter, instruments and tool helpers so the new
server matches the others.
