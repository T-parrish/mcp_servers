# mcp_servers

A Cargo workspace of [MCP](https://modelcontextprotocol.io) servers written in Rust with
[`rmcp`](https://crates.io/crates/rmcp), sharing one build, one `.env`, and one telemetry stack.

| Crate | What it is |
|-------|------------|
| [`spotify_mcp_server`](spotify_mcp_server) | Log in via OAuth (PKCE), list your playlists, list a playlist's tracks, save its songs. Official Spotify Web API. |
| [`bandcamp_mcp_server`](bandcamp_mcp_server) | Search artists and songs, record where a library song can be bought, add to cart. Bandcamp's undocumented internal API. |
| [`mcp_core`](mcp_core) | Library shared by both servers: telemetry setup, OpenTelemetry instruments, the outbound rate limiter, and tool-result helpers. |
| [`mcp_db`](mcp_db) | Library shared by both servers: the Postgres pool, the schema, and the writes. |

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

## Persistence

Songs are recorded in Postgres, so that finding them on other stores later does not mean listing the
playlist again. Reading and saving are separate tools: `list_playlist_tracks` only reads, and
`save_playlist_songs` records a playlist's songs. Splitting them means a database problem can never
cost you a Spotify request you already made, and the server is fully usable with no database at all.

`save_playlist_songs` takes a playlist reference and fetches the tracks itself rather than accepting
track data — routing that through the assistant would be slower, more expensive, and corruptible,
and a paraphrased title becomes a wrong row that nothing downstream can detect.

`docker-compose.yml` runs a Postgres:

```sh
docker compose up -d postgres     # listens on 127.0.0.1:5432
```

Point the servers at it with `DATABASE_URL` in `.env` (the value in `.env.example` matches the
container). It is **optional**: unset, the server starts normally and only `save_playlist_songs`
refuses, naming the variable. But a database that *is* configured and turns out to be unreachable or
unmigrated stops startup — an absent database is a choice, a broken one is a misconfiguration worth
hearing about immediately. For the same reason a failed write fails the tool call rather than being
logged and dropped: a save that quietly did nothing is exactly what the tool exists to make visible.

### Migrations

The schema lives in [`mcp_db/migrations`](mcp_db/migrations) and is applied by
[`sqlx-cli`](https://crates.io/crates/sqlx-cli), which must match the `sqlx` version in
`Cargo.toml`:

```sh
cargo install sqlx-cli --no-default-features --features postgres,rustls
```

`./run.sh` applies pending migrations on every launch (skipping this when no `DATABASE_URL` is
configured), so a server started through the wrapper never meets an unmigrated database. To do it by
hand — or when starting a binary directly:

```sh
sqlx migrate run --source mcp_db/migrations      # apply
sqlx migrate info --source mcp_db/migrations     # what has been applied
sqlx migrate add --source mcp_db/migrations <name>   # author the next one
```

A new migration is just a new file in that directory; nothing in the Rust code needs to know about
it. Migrations are checksummed once applied, so never edit one that has already run.

### Tests

The database tests need Postgres, and get their own — a second, disposable server so they can never
reach a real library:

```sh
docker compose up -d postgres-test     # tmpfs storage, listens on :5433
cargo test -p mcp_db
```

The isolation is structural rather than a rule to remember. The test harness reads
`TEST_DATABASE_URL` and **never** `DATABASE_URL`, and panics before touching anything if the two
name the same database. Each test then creates its own `mcp_test_<random>` database, applies the
migrations to it, and drops it at the end, so tests cannot see each other's rows or leave anything
behind. With `TEST_DATABASE_URL` unset they skip and pass, which is what makes a plain
`cargo test` safe anywhere.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `fmt`, `clippy`, the full test
suite against a Postgres service container, and a separate check that the migrations still apply
through `sqlx-cli` — the path `run.sh` uses.

### Schema

Two tables:

- **`songs`** — one row per song. A song is identified by its *normalized* lead artist and title
  (`artist_key`, `title_key`: lower-cased, trimmed, internal whitespace collapsed), because the
  stores this is meant to be matched against share no identifier with Spotify — only the text.
  Those two columns are `GENERATED ALWAYS`, so the keys cannot disagree with the values they come
  from. Re-listing a playlist inserts nothing new: the write is `ON CONFLICT DO NOTHING`, and an
  existing row is never modified. `spotify_id` and `isrc` are recorded alongside for a future
  matcher that can do better than text.
- **`purchase_options`** — where a song can be bought, one row appended per search, so the history
  (including how a price moved) is kept. `song_id` references `songs`, many-to-one. Each row carries
  the platform, when it was searched, the URL, whether it has been purchased, and the price with its
  currency. Written by the Bandcamp server's `find_purchase_options`, which takes a song's artist
  and title, requires that song to already be in `songs`, and appends a row per Bandcamp hit — plus
  a row with a NULL `url` when a search finds nothing, so "looked and found nothing" is
  distinguishable from "never looked". `price` and `currency` stay NULL for now: Bandcamp's search
  endpoint does not report a price, and filling them would mean fetching each item's page.

The current state of a song on each platform is the most recent row per platform:

```sql
SELECT DISTINCT ON (song_id, platform) *
FROM purchase_options
ORDER BY song_id, platform, searched_at DESC;
```

## Telemetry

`docker-compose.yml` runs one [`grafana/otel-lgtm`](https://github.com/grafana/docker-otel-lgtm)
container (Grafana + Tempo + Prometheus + Loki) shared by every server:

```sh
docker compose up -d lgtm          # Grafana at http://localhost:3000 (admin/admin), OTLP on :4318
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
