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

## Build

```sh
cargo build --release
```

The binary is written to `target/release/bandcamp_mcp_server`.

## Use with an MCP client

The server communicates over **stdio**. Point your MCP client at the built binary:

```jsonc
{
  "mcpServers": {
    "bandcamp": {
      "command": "/absolute/path/to/target/release/bandcamp_mcp_server",
      "env": {
        "RUST_LOG": "info",
        // Optional, for live add_to_cart (see Cart & authentication). The cookie can
        // instead be supplied at runtime via the `authenticate` tool:
        // "BANDCAMP_ALLOW_CART_WRITES": "1",
        // "BANDCAMP_COOKIE": "identity=...; session=..."
      }
    }
  }
}
```

## Logging & tracing

Uses [`tracing`](https://crates.io/crates/tracing) with instrumented spans on every operation.

- Logs are written to **stderr** — stdout is reserved for the MCP JSON-RPC protocol stream.
- Verbosity is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug` to see the outgoing
  Bandcamp requests.

## Project layout

```
src/
  main.rs              tracing setup + stdio server startup
  server.rs            BandcampServer type: shared client + combined tool router
  bandcamp.rs          low-level client for Bandcamp's internal API + wire types
  tools/
    mod.rs             combines the action routers; shared helpers
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
handler via `self.client()`.

## Limitations

- **`add_to_cart` is reverse-engineered.** The `/cart/cb` contract was derived from Bandcamp's site
  JavaScript, not a documented API — the exact fields or endpoint may change, and the first live call
  against your session is the real test. It is dry-run by default to avoid accidental mutations.
- All endpoints are unofficial and undocumented; response shapes may change without notice.
