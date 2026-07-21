# bandcamp_mcp_server

An [MCP](https://modelcontextprotocol.io) server (Rust, [`rmcp`](https://crates.io/crates/rmcp)) that
exposes Bandcamp functionality as tools: search for artists, search for songs by an artist, and add
items to a cart.

> **Heads-up:** Bandcamp has no official public API. Search is backed by Bandcamp's *undocumented*
> internal endpoint (the same one bandcamp.com calls) — it works today but can change or break
> without notice, and lives in an ambiguous ToS area. `add_to_cart` is currently a **stub** (see
> [Limitations](#limitations)).

## Tools

| Tool | Arguments | Returns |
|------|-----------|---------|
| `search_artists` | `query` (string), `limit` (int, default 10) | Artists: `name`, `artist_id`, `location`, `url` |
| `search_songs` | `artist` (string), `query` (string, optional), `limit` (int, default 10) | Songs: `title`, `artist`, `album`, `track_id`, `url` |
| `add_to_cart` | `item_id` (int), `item_type` (string, default `"track"`), `item_name` (string, optional), `price` (float, optional) | Simulated cart result (see [Limitations](#limitations)) |

`search_songs` searches tracks matching `artist` (optionally narrowed by `query`) and keeps only
results whose band name matches `artist`.

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
      "env": { "RUST_LOG": "info" }
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

- **`add_to_cart` is a stub.** It validates and echoes the request as a `"simulated"` result but does
  **not** modify a real Bandcamp cart. A real cart requires an authenticated user session (cookies)
  and Bandcamp's non-public purchase flow, which is intentionally not implemented here.
- The internal search API is unofficial and undocumented; response shapes may change without notice.
