# bandcamp_mcp_server

An [MCP](https://modelcontextprotocol.io) server (Rust, [`rmcp`](https://crates.io/crates/rmcp)) that
exposes Bandcamp functionality as tools: search for artists, search for songs by an artist, and add
items to a cart.

> **Heads-up:** Bandcamp has no official public API. Everything here is backed by Bandcamp's
> *undocumented* internal endpoints (the same ones bandcamp.com calls) — they work today but can
> change or break without notice, and live in an ambiguous ToS area. `add_to_cart` mutates a real
> account and is **reverse-engineered**; see [Cart & authentication](#cart--authentication).

## Tools

| Tool | Arguments | Returns |
|------|-----------|---------|
| `search_artists` | `query` (string), `limit` (int, default 10) | Artists: `name`, `artist_id`, `location`, `url` |
| `search_songs` | `artist` (string), `query` (string, optional), `limit` (int, default 10) | Songs: `title`, `artist`, `band_id`, `album`, `track_id`, `url` |
| `add_to_cart` | `item_id` (int), `unit_price` (float), `item_type` (`"album"`/`"track"`/`"package"`, default `"album"`), `band_id` (int, optional), `quantity` (int, default 1), `item_url` (string, optional), `item_name` (string, optional) | The request that was (or would be) sent, plus Bandcamp's cart response, or an `auth_required` result |
| `authenticate` | `from_browser` (bool, optional), `cookie` (string, optional) | Auth status (`authenticated` / `invalid_cookie` / `unauthenticated`) and `fan_id`. Pass `from_browser:true` to pull the cookie from Chrome, or `cookie` to set it manually |

`search_songs` searches tracks matching `artist` (optionally narrowed by `query`) and keeps only
results whose band name matches `artist`. Pass a result's `band_id` (and its `url` as `item_url`)
straight into `add_to_cart`.

## Cart & authentication

`add_to_cart` drives Bandcamp's undocumented `POST /cart/cb` endpoint. Adds work against **your
account's cart** when a valid logged-in session is present (you then check out on bandcamp.com).
Bandcamp technically accepts *anonymous* cart adds, so the server verifies the session first (via the
identity endpoint) and refuses to send unless you're actually logged in — otherwise the item would
land in a throwaway anonymous cart.

### Providing the session cookie

The cookie is loaded at startup and can be refreshed at runtime, from (first match wins):

1. the `BANDCAMP_COOKIE` environment variable, then
2. a cached **cookie file** (`$BANDCAMP_COOKIE_FILE`, else
   `$XDG_CONFIG_HOME/bandcamp_mcp_server/cookie`, else `~/.config/bandcamp_mcp_server/cookie`).

The **`authenticate` tool** obtains a cookie, saves it to that file (owner-only permissions), and
verifies it against Bandcamp. Two ways to supply it:

- **Automatic (Chrome):** `authenticate {"from_browser": true}` reads the `bandcamp.com` cookie
  straight from your local Chrome profile (you must be logged in at bandcamp.com in Chrome). On macOS
  the first read triggers a Keychain "Allow" prompt. Requires the `rookie` crate (already a
  dependency).
- **Manual:** `authenticate {"cookie": "..."}` — while logged in at bandcamp.com, open browser dev
  tools → Network → any request → copy the full `Cookie` request header.

Either way the cookie persists and is auto-loaded on every later start. When it expires, `add_to_cart`
returns an `auth_required` result prompting you to call `authenticate` again.

### Write safety

| Env var | Purpose |
|---------|---------|
| `BANDCAMP_ALLOW_CART_WRITES` | Set to `1`/`true` to send real requests. **Unset/anything else = dry-run.** |

- **Dry-run (default):** `add_to_cart` builds and returns the exact request (`mode: "dry_run"`)
  without sending it. Safe for testing, and needs no cookie.
- **Live:** with `BANDCAMP_ALLOW_CART_WRITES=1` and a valid session, it POSTs to the item's site
  (derived from `item_url`, else `bandcamp.com`) and returns Bandcamp's response.

`unit_price` must meet the item's minimum (many Bandcamp items are "name your price").

> This adds to **your own** cart on **your own** account. Because it modifies real account state and
> the endpoint is reverse-engineered, it is dry-run by default; verify the first live call yourself.

## Rate limiting

All outbound requests (search, auth check, cart) pass through a shared limiter in the HTTP client, so
no tool can hammer Bandcamp's API. Two env vars tune it:

| Env var | Default | Purpose |
|---------|---------|---------|
| `BANDCAMP_MAX_CONCURRENT_REQUESTS` | `1` | Maximum requests in flight at once. |
| `BANDCAMP_MIN_REQUEST_INTERVAL_MS` | `750` | Minimum spacing between request *starts*, plus a small random jitter. |

The defaults are deliberately gentle (fully serialized, ~0.75–1.1s apart). Raise concurrency and/or
lower the interval for more throughput.

This crate is part of a Cargo workspace (see the [repo root](../README.md)); most commands below run
from the **workspace root**, one directory up.

## Build

From the workspace root:

```sh
cargo build --release -p bandcamp_mcp_server   # or: cargo build --release  (all servers)
```

The binary is written to the shared `../target/release/bandcamp_mcp_server`.

## Use with an MCP client

The server communicates over **stdio**. Point your MCP client at the workspace root's `run.sh`, which
takes the server name, `cd`s to the root (so `.env` is found) and execs the release binary:

```sh
claude mcp add -s user bandcamp -- /absolute/path/to/mcp_servers/run.sh bandcamp
```

`-s user` makes it available in every Claude Code project; use `-s local` for this one only, or
`-s project` to write a committed `.mcp.json`. Verify with `claude mcp list`, and remove with
`claude mcp remove bandcamp -s user`.

For Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`, restart after
editing) or any other client, the equivalent is:

```jsonc
{
  "mcpServers": {
    "bandcamp": {
      "command": "/absolute/path/to/mcp_servers/run.sh",
      "args": ["bandcamp"]
    }
  }
}
```

**Why the wrapper:** MCP clients let you set a command but not a working directory, and `.env` is
resolved relative to the process's cwd — pointed straight at the binary, the server would start with
none of your `.env` settings. The wrapper keeps the workspace `.env` the single source of truth, so
configuration changes need no client-config edits (just restart the server). If you would rather not
use it, point `command` at `target/release/bandcamp_mcp_server` and repeat the settings in an
`"env": {}` block.

## Configuration via `.env`

Configuration lives in **one `.env` at the workspace root**, shared by every server here (the vars are
namespaced, so there is no collision). On startup the server loads it from its working directory,
searching parent directories — so `run.sh`, which starts from the root, always finds it. Copy the
template and edit:

```sh
cp ../.env.example ../.env      # from this crate; or just `.env` at the root
```

- `.env` is **gitignored** — it may hold your `BANDCAMP_COOKIE`, so never commit it.
- **Real environment variables take precedence** over `.env` values.
- Only `BANDCAMP_*` (and the shared `RUST_LOG` / `OTEL_*`) vars affect this server; the `SPOTIFY_*`
  ones in the same file are simply ignored here.

## Logging & tracing

Uses [`tracing`](https://crates.io/crates/tracing) with instrumented spans on every operation.

- Logs are written to **stderr** — stdout is reserved for the MCP JSON-RPC protocol stream.
- Verbosity is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug` to see the outgoing
  Bandcamp requests.

### OpenTelemetry export

Spans and metrics can additionally be exported over OTLP (HTTP/protobuf). This is **off unless
`OTEL_EXPORTER_OTLP_ENDPOINT` is set**, so the default local experience is unchanged:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
  ../target/release/bandcamp_mcp_server
```

The standard `OTEL_*` variables (endpoint, headers, timeout, service name, resource attributes) are
honoured; `service.name` falls back to **`bandcamp-mcp`** (the binary's built-in default, so a shared
`.env` need not set `OTEL_SERVICE_NAME`) and `service.version` to the crate version.
Each tool call becomes a `SERVER` span (`tools/call <name>`, with `mcp.method.name` / `mcp.tool.name`)
containing a `CLIENT` span per Bandcamp request (`http.request.method`, `url.full`, `server.address`,
`http.response.status_code`). Buffered telemetry is flushed on shutdown.

The two signals can be pointed at different backends, or enabled independently, with
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`. Per the OTLP spec those
signal-specific variables are used **as-is**, so they must include the `/v1/traces` or `/v1/metrics`
path — only the shared `OTEL_EXPORTER_OTLP_ENDPOINT` gets it appended. Metrics are exported every
`OTEL_METRIC_EXPORT_INTERVAL` ms (default 60000).

| Instrument | Type | Unit | Attributes |
| --- | --- | --- | --- |
| `mcp.tool.calls` | counter | `{call}` | `mcp.tool.name`, `outcome` (`ok` / `error`) |
| `bandcamp.request.duration` | histogram | `s` | `http.request.method`, `server.address`, `http.response.status_code` (or `error.type` when the request never completed) |
| `bandcamp.rate_limiter.wait.duration` | histogram | `s` | — |

`bandcamp.rate_limiter.wait.duration` measures time blocked before a request starts, so it shows
whether the [rate limiter](#rate-limiting) rather than Bandcamp is your latency. `outcome=error`
counts *protocol* errors only — a tool that returns an in-band failure such as `auth_required` is
still a successful call.

Only this crate's spans are exported. rmcp's own `serve_inner` span lives for the whole stdio
session, so leaving it in would make every trace a child of one span that does not close until the
client disconnects — nothing would be queryable until then. Filtering it out gives one complete
trace per tool call.

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
  target/release/bandcamp_mcp_server
```

Then open <http://localhost:3000> (login `admin` / `admin`):

- **Explore → Tempo** for traces. Search `{ resource.service.name="bandcamp-mcp" }`; each tool call is
  a `tools/call <name>` span with the Bandcamp request nested inside it. Tempo buffers before making
  a trace searchable, so allow a few seconds after a call before it appears.
- **Explore → Prometheus** for the instruments, where the OTLP names arrive normalised to
  `mcp_tool_calls_total`, `bandcamp_request_duration_seconds`, and
  `bandcamp_rate_limiter_wait_duration_seconds`.

Servers sharing the stack are kept apart by `service.name` — each binary defaults it to its own name
(`bandcamp-mcp`, `spotify-mcp`), so no per-server `OTEL_SERVICE_NAME` is needed. Filter on it in both
Tempo and Prometheus.

Storage is in-memory: restarting the container discards everything. That suits "why was that call
slow" and short-lived stdio sessions, but not trends over time — for that you want a durable backend
(Tempo/Prometheus proper, SigNoz, or a hosted OTLP endpoint), which is a change of
`OTEL_EXPORTER_OTLP_ENDPOINT` and nothing else.

## Project layout

Telemetry wiring, the OpenTelemetry instruments, the rate limiter and the tool-result helpers live in
the workspace's [`mcp_core`](../mcp_core) crate, shared with the other servers. This crate holds only
what is Bandcamp-specific:

```
src/
  main.rs              stdio startup; names the service + metric prefix for mcp_core
  server.rs            BandcampServer type: shared client + combined tool router
  bandcamp.rs          low-level client for Bandcamp's internal API + wire types
  tools/
    mod.rs             combines the action routers
    search_artists.rs  one action: params + output type + #[tool] handler
    search_songs.rs    "
    add_to_cart.rs     "
    authenticate.rs    "
```

### Adding a new action

1. Create `src/tools/<action>.rs` with a
   `#[tool_router(router = <action>_router, vis = "pub")]` impl block on
   `BandcampServer` containing the `#[tool]` handler (see the existing actions).
2. In `src/tools/mod.rs`, add `mod <action>;` and
   `+ BandcampServer::<action>_router()` in `router()`.

Shared HTTP logic goes on `BandcampClient` in `bandcamp.rs`; reach it from a
handler via `self.client()`. Output goes through `tools::json_result` (re-exported from `mcp_core`).

## Limitations

- **`add_to_cart` is reverse-engineered.** The `/cart/cb` contract was derived from Bandcamp's site
  JavaScript, not a documented API — the exact fields or endpoint may change, and the first live call
  against your session is the real test. It is dry-run by default to avoid accidental mutations.
- All endpoints are unofficial and undocumented; response shapes may change without notice.
