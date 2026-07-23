# spotify_mcp_server

An [MCP](https://modelcontextprotocol.io) server (Rust, [`rmcp`](https://crates.io/crates/rmcp)) that
exposes Spotify functionality as tools: log in via OAuth, list your own playlists, and list the tracks
of a playlist with their metadata.

Backed by the **official** [Spotify Web API](https://developer.spotify.com/documentation/web-api),
using the Authorization Code flow with PKCE — read-only scopes, no client secret.

## Tools

| Tool | Arguments | Returns |
|------|-----------|---------|
| `authenticate` | `force` (bool, optional), `timeout_seconds` (int, default 120) | Login status (`authenticated` / `not_configured` / `authorization_failed` / `listener_failed`), the Spotify user, granted scopes, and where tokens are cached |
| `list_playlists` | `limit` (int, default 50), `only_mine` (bool, default true) | Playlists: `id`, `name`, `description`, `owner`, `owner_id`, `public`, `collaborative`, `track_count`, `url`, `snapshot_id` |
| `list_playlist_tracks` | `playlist_id` (string), `limit` (int, default 100), `offset` (int, default 0) | `playlist_id`, `offset`, `returned`, `total`, and `tracks`: `title`, `artists`, `album`, `album_release_date`, `duration_ms`, `explicit`, `popularity`, `disc_number`, `track_number`, `isrc`, `url`, `uri`, `added_at`, `added_by`, `is_local`, `kind` |

`list_playlists` returns only playlists you own by default (that is what "my playlists" usually
means); pass `only_mine:false` to include ones you merely follow. Feed a result's `id` straight into
`list_playlist_tracks`, which also accepts a `spotify:playlist:…` URI or an open.spotify.com URL.

Both listing tools follow Spotify's pagination automatically until `limit` items are collected. Until
a login is stored they return an in-band `auth_required` result telling the assistant to call
`authenticate` first.

## Authentication

### One-time setup

1. Create an app at <https://developer.spotify.com/dashboard>.
2. Add **`http://127.0.0.1:8888/callback`** to the app's *Redirect URIs* — it must match
   `SPOTIFY_REDIRECT_URI` exactly. Spotify rejects plain `http` except on the loopback interface, and
   requires the literal IP rather than `localhost`.
3. Set `SPOTIFY_CLIENT_ID` (in `.env` or the client config). **No client secret is needed**: the flow
   uses PKCE, so nothing long-lived and secret is stored on disk.

### Logging in

Call the `authenticate` tool. It:

1. binds a one-shot HTTP listener on the loopback redirect URI,
2. opens Spotify's consent page in your default browser (and returns the URL if it cannot),
3. captures the `?code=…` redirect, checks the CSRF `state`, and redeems the code with the PKCE
   verifier,
4. saves the token set to `~/.config/spotify_mcp_server/token.json` (owner-only permissions) and
   confirms the login against `GET /v1/me`.

The handler blocks while you approve, up to `timeout_seconds` (default 120) — if your MCP client has a
shorter request timeout, lower it to fit. On timeout the tool returns the authorize URL so you can
open it manually and call `authenticate` again.

Later runs reuse the stored tokens, and the access token is refreshed transparently when it expires,
so this is normally a one-time step. Pass `force:true` to re-authorize (e.g. after changing scopes).

| Env var | Default | Purpose |
|---------|---------|---------|
| `SPOTIFY_CLIENT_ID` | — | **Required.** Your Spotify app's client ID. |
| `SPOTIFY_REDIRECT_URI` | `http://127.0.0.1:8888/callback` | Loopback redirect, registered on the app verbatim. Change the port if 8888 is taken. |
| `SPOTIFY_TOKEN_FILE` | `$XDG_CONFIG_HOME/spotify_mcp_server/token.json`, else `~/.config/…` | Where tokens are cached. |

### Scopes

`playlist-read-private playlist-read-collaborative` — the minimum needed to enumerate your own
playlists (including private and collaborative ones) and read their contents. Everything this server
does is read-only; no tool can modify your account.

## Rate limiting

All outbound requests (API calls and token exchanges) pass through a shared limiter in the HTTP
client, so paginated listings cannot burst into Spotify's rate limits. Two env vars tune it:

| Env var | Default | Purpose |
|---------|---------|---------|
| `SPOTIFY_MAX_CONCURRENT_REQUESTS` | `4` | Maximum requests in flight at once. |
| `SPOTIFY_MIN_REQUEST_INTERVAL_MS` | `100` | Minimum spacing between request *starts*, plus a small random jitter. |

If Spotify replies `429` anyway, the request is retried once after the `Retry-After` delay (capped at
30s) and the retry is recorded on the span as `http.request.resend_count`.

This crate is part of a Cargo workspace (see the [repo root](../README.md)); most commands below run
from the **workspace root**, one directory up.

## Build

From the workspace root:

```sh
cargo build --release -p spotify_mcp_server   # or: cargo build --release  (all servers)
```

The binary is written to the shared `../target/release/spotify_mcp_server`.

## Use with an MCP client

The server communicates over **stdio**. Point your MCP client at the workspace root's `run.sh`, which
takes the server name, `cd`s to the root (so `.env` is found) and execs the release binary:

```sh
claude mcp add -s user spotify -- /absolute/path/to/mcp_servers/run.sh spotify
```

`-s user` makes it available in every Claude Code project; use `-s local` for this one only, or
`-s project` to write a committed `.mcp.json`. Verify with `claude mcp list`, and remove with
`claude mcp remove spotify -s user`.

For Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`, restart after
editing) or any other client, the equivalent is:

```jsonc
{
  "mcpServers": {
    "spotify": {
      "command": "/absolute/path/to/mcp_servers/run.sh",
      "args": ["spotify"]
    }
  }
}
```

**Why the wrapper:** MCP clients let you set a command but not a working directory, and `.env` is
resolved relative to the process's cwd — pointed straight at the binary, the server would start
without `SPOTIFY_CLIENT_ID`. The wrapper keeps the workspace `.env` the single source of truth, so
configuration changes need no client-config edits (just restart the server). If you would rather not
use it, point `command` at `target/release/spotify_mcp_server` and repeat the settings in an
`"env": {}` block.

## Configuration via `.env`

Configuration lives in **one `.env` at the workspace root**, shared by every server here (the vars are
namespaced, so there is no collision). On startup the server loads it from its working directory,
searching parent directories — so `run.sh`, which starts from the root, always finds it. Copy the
template and edit:

```sh
cp ../.env.example ../.env      # from this crate; or just `.env` at the root
```

- `.env` is **gitignored** — never commit it.
- **Real environment variables take precedence** over `.env` values.
- Only `SPOTIFY_*` (and the shared `RUST_LOG` / `OTEL_*`) vars affect this server; the `BANDCAMP_*`
  ones in the same file are simply ignored here.

## Logging & tracing

Uses [`tracing`](https://crates.io/crates/tracing) with instrumented spans on every operation.

- Logs are written to **stderr** — stdout is reserved for the MCP JSON-RPC protocol stream.
- Verbosity is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug` to see pagination and
  the outgoing Spotify requests.
- Credentials never reach a span: the OAuth handlers use `skip_all`, and only the `code_challenge`
  (not the verifier) ever appears in a logged URL.

### OpenTelemetry export

Spans and metrics can additionally be exported over OTLP (HTTP/protobuf). This is **off unless
`OTEL_EXPORTER_OTLP_ENDPOINT` is set**, so the default local experience is unchanged:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
  ../target/release/spotify_mcp_server
```

The standard `OTEL_*` variables (endpoint, headers, timeout, service name, resource attributes) are
honoured; `service.name` falls back to **`spotify-mcp`** (the binary's built-in default, so a shared
`.env` need not set `OTEL_SERVICE_NAME`) and `service.version` to the crate version.
Each tool call becomes a `SERVER` span (`tools/call <name>`, with `mcp.method.name` / `mcp.tool.name`)
containing a `CLIENT` span per Spotify request (`http.request.method`, `url.full`, `url.template`,
`server.address`, `http.response.status_code`). Buffered telemetry is flushed on shutdown.

The two signals can be pointed at different backends, or enabled independently, with
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`. Per the OTLP spec those
signal-specific variables are used **as-is**, so they must include the `/v1/traces` or `/v1/metrics`
path — only the shared `OTEL_EXPORTER_OTLP_ENDPOINT` gets it appended. Metrics are exported every
`OTEL_METRIC_EXPORT_INTERVAL` ms (default 60000).

| Instrument | Type | Unit | Attributes |
| --- | --- | --- | --- |
| `mcp.tool.calls` | counter | `{call}` | `mcp.tool.name`, `outcome` (`ok` / `error`) |
| `spotify.request.duration` | histogram | `s` | `http.request.method`, `server.address`, `url.template`, `http.response.status_code` (or `error.type` when the request never completed) |
| `spotify.rate_limiter.wait.duration` | histogram | `s` | — |
| `spotify.token.refreshes` | counter | `{refresh}` | `outcome` (`ok` / `error`) |

`url.template` is the low-cardinality route (e.g. `/v1/playlists/{playlist_id}/tracks`), so playlist
IDs never explode the metric's cardinality — the concrete URL stays on the span's `url.full`.
`spotify.rate_limiter.wait.duration` measures time blocked before a request starts, so it shows
whether the [rate limiter](#rate-limiting) rather than Spotify is your latency. `outcome=error` counts
*protocol* errors only — a tool that returns an in-band failure such as `auth_required` is still a
successful call.

Only this crate's spans are exported. rmcp's own `serve_inner` span lives for the whole stdio session,
so leaving it in would make every trace a child of one span that does not close until the client
disconnects — nothing would be queryable until then. Filtering it out gives one complete trace per
tool call.

Because MCP-over-stdio has no standard place to carry W3C trace context, each tool call starts a new
trace — it cannot be linked to the calling agent's trace.

### Viewing telemetry locally

The repo root's `docker-compose.yml` runs [`grafana/otel-lgtm`](https://github.com/grafana/docker-otel-lgtm)
— Grafana with Tempo (traces), Prometheus (metrics) and Loki, pre-wired in one container. It ingests
OTLP directly, so no collector is needed. One stack is shared by every MCP server in this repo, so
start it from the **parent directory**:

```sh
docker compose up -d      # from the workspace root, where the compose file lives
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
OTEL_METRIC_EXPORT_INTERVAL=5000 \
  target/release/spotify_mcp_server
```

Then open <http://localhost:3000> (login `admin` / `admin`):

- **Explore → Tempo** for traces. Search `{ resource.service.name="spotify-mcp" }`; each tool call is
  a `tools/call <name>` span with the Spotify requests nested inside it. Tempo buffers before making a
  trace searchable, so allow a few seconds after a call before it appears.
- **Explore → Prometheus** for the instruments, where the OTLP names arrive normalised to
  `mcp_tool_calls_total`, `spotify_request_duration_seconds`,
  `spotify_rate_limiter_wait_duration_seconds`, and `spotify_token_refreshes_total`.

Servers sharing the stack are kept apart by `service.name` — each binary defaults it to its own name
(`spotify-mcp`, `bandcamp-mcp`), so no per-server `OTEL_SERVICE_NAME` is needed. Filter on it in both
Tempo and Prometheus.

Storage is in-memory: restarting the container discards everything. That suits "why was that call
slow" and short-lived stdio sessions, but not trends over time — for that you want a durable backend
(Tempo/Prometheus proper, SigNoz, or a hosted OTLP endpoint), which is a change of
`OTEL_EXPORTER_OTLP_ENDPOINT` and nothing else.

## Project layout

Telemetry wiring, the OpenTelemetry instruments, the rate limiter and the tool-result helpers live in
the workspace's [`mcp_core`](../mcp_core) crate, shared with the other servers. This crate holds only
what is Spotify-specific:

```
src/
  main.rs                   stdio startup; names the service + metric prefix for mcp_core
  metrics.rs                the Spotify-only token-refresh counter (re-exports mcp_core's shared ones)
  server.rs                 SpotifyServer type: shared client + combined tool router
  spotify.rs                low-level Web API client: HTTP, tokens, pagination, wire types
  oauth.rs                  PKCE + the loopback listener that captures the redirect
  tools/
    mod.rs                  combines the action routers; the auth-error helper
    authenticate.rs         one action: params + output type + #[tool] handler
    list_playlists.rs       "
    list_playlist_tracks.rs "
```

### Adding a new action

1. Create `src/tools/<action>.rs` with a
   `#[tool_router(router = <action>_router, vis = "pub")]` impl block on
   `SpotifyServer` containing the `#[tool]` handler (see the existing actions).
2. In `src/tools/mod.rs`, add `mod <action>;` and
   `+ SpotifyServer::<action>_router()` in `router()`.

Shared HTTP logic goes on `SpotifyClient` in `spotify.rs`; reach it from a handler via
`self.client()`. `get_path` and `get_paged` already handle auth, refresh, rate limiting, 429 retries,
spans and metrics — a new read-only action is usually just a route, a wire type and an output type.
Use `tools::json_result` (re-exported from `mcp_core`) for output, and return errors through
`tools::api_error_result` so an expired login surfaces as `auth_required` rather than a protocol error.

### Tests

```sh
cargo test -p spotify_mcp_server      # from the workspace root
```

Covers the parts worth pinning without a live Spotify account: PKCE challenge derivation, authorize-URL
construction, the loopback callback (happy path, forged `state`, denied authorization, stray requests),
and playlist ID/URI/URL parsing.

## Limitations

- **A new scope needs a new login.** Adding tools that write (e.g. modifying playlists) means widening
  `SCOPES` in `src/spotify.rs` and calling `authenticate {"force": true}`; the stored token keeps the
  scopes it was granted.
- **The login is interactive.** `authenticate` needs a browser on the same machine as the server, since
  the redirect is captured on loopback.
- Only the first 50 items per request are fetched (Spotify's cap); larger `limit` values are assembled
  from several requests, so listing a very long playlist costs several API calls.
