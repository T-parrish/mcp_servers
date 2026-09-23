# beatport_mcp_server

An MCP server over [Beatport](https://www.beatport.com)'s v4 API (`api.beatport.com/v4`).

It records where a song from the local library can be bought on Beatport and for how much, into the
same `purchase_options` table as the Bandcamp server (see the workspace README's *Persistence*).

## Tools

| Tool | Arguments | Returns |
|------|-----------|---------|
| `find_purchase_options` | `artist` (string), `title` (string), `limit` (int, default 10), `only_if_not_on` (string, optional, e.g. `"bandcamp"`) | Each result's `title`, `mix_name`, `artists`, `url`, `price`, `currency`, `isrc`, `matched_by` (`isrc`/`text`), `track_id`; or `skipped`, or an `auth_required` result |
| `authenticate` | `token` (string, optional) | Auth status (`authenticated` / `invalid_token` / `unauthenticated` / `unknown`) and `username`. Pass `token` to save one; omit it to check the current one |

`find_purchase_options` needs the song in the library already (the Spotify server's
`save_playlist_songs` puts it there) and a configured `DATABASE_URL`.

- **Matching.** It looks the song up by the ISRC Spotify recorded for it first: a hit is the exact
  recording (`matched_by: "isrc"`). When that finds nothing — no ISRC, or Beatport sells a different
  version (an Extended Mix has its own ISRC) — it falls back to a text search, keeping results where
  one of the track's artists is the song's artist and the titles match (`matched_by: "text"`). Those
  may be another version of the song, so check `mix_name`.
- **Price, as AIFF.** `price` is what the track costs as AIFF: its listed price (`base_price`, the
  MP3 price) plus the account's AIFF surcharge. The catalog lists only the base price, and the
  format price list is closed to a docs token, so the surcharge is read once per start from the
  `upgrade_fees` on the account's own purchase history — a flat +0.75 USD across 300 real
  purchases. An account with no purchases cannot be priced, and the search fails saying so. A track
  listed but not for sale (`sale_type` other than `purchase`) is recorded with a NULL price.
- **Link.** Beatport has one page per track, not per format: the format is the account's download
  preference (*Account → Preferences*), so the stored `url` gets AIFF only when that is set to AIFF.
- **Fallback.** With `only_if_not_on: "bandcamp"` it first checks the song's most recent Bandcamp
  search, and skips (recording nothing) if that search found it. A song never searched on Bandcamp
  counts as not found.
- A search that finds nothing is recorded as a row with a NULL `url`, as on Bandcamp. A search that
  could not run — an expired token — records nothing and returns `auth_required`.

## Authentication

Beatport's API uses OAuth 2, but issues clients to partners. Until this project has its own, it uses a
bearer token copied by hand from your own logged-in session:

1. Log in on <https://api.beatport.com/v4/docs/>.
2. Open browser dev tools → Network → click any request to `api.beatport.com` → copy the value of
   its `Authorization` request header.
3. Call `authenticate {"token": "..."}` (with or without the `Bearer ` prefix), or write it to the
   token file yourself: `pbpaste > ~/.config/beatport_mcp_server/token`.

The token is loaded at startup from `BEATPORT_AUTH_TOKEN` (in `.env` or the environment), else the
token file (`$BEATPORT_TOKEN_FILE`, else `$XDG_CONFIG_HOME/beatport_mcp_server/token`, else
`~/.config/beatport_mcp_server/token`); `authenticate` saves to that file with owner-only
permissions. Keep it in one place: while `BEATPORT_AUTH_TOKEN` is set it wins at every start, so a
token saved with `authenticate` lasts only until the next restart — replace an expired one in `.env`. It is checked against Beatport's introspect endpoint,
which answers 200 even for a bad token, so a token counts as valid only when it names a user.

There is no refresh token, so an expired token has to be copied again. Your password never touches
this server. Everything that knows where the token comes from is in `src/beatport.rs`, so a real
OAuth client can replace it later without changing the tools.

> This uses a client id Beatport issued to its own docs page, not to this project — the same grey
> area as the Bandcamp server's session cookie. Treat it as a personal prototype.
