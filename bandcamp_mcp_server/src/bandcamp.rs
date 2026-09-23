//! Low-level client over Bandcamp's *undocumented* internal API.
//!
//! Bandcamp has no official public API. These endpoints are the same ones the
//! bandcamp.com website calls, so they can change or break without notice.
//!
//! This module only provides the shared HTTP primitive and wire types. The
//! per-action logic (filtering, output shaping) lives in `crate::tools`.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use mcp_core::ratelimit::RateLimiter;
use serde::Deserialize;

const SEARCH_URL: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const IDENTITY_URL: &str = "https://bandcamp.com/api/fan/2/collection_summary";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; bandcamp_mcp_server/0.1)";

/// The origin every cart request goes to unless a caller supplies a more
/// specific (and validated) Bandcamp origin.
pub(crate) const DEFAULT_CART_ORIGIN: &str = "https://bandcamp.com";

/// Normalize a caller-supplied URL or origin to an `https://<host>` origin on
/// Bandcamp, or `None` if it points anywhere else.
///
/// Cart requests carry the full session cookie and their target is derived from
/// model-supplied tool input, so anything that is not `bandcamp.com` or a
/// `*.bandcamp.com` artist subdomain must never become a target. `http` URLs are
/// upgraded to `https`; port and userinfo are dropped.
pub(crate) fn bandcamp_origin(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return None;
    }
    // A trailing dot ("bandcamp.com.") is the same host to DNS but not to `==`.
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (host == "bandcamp.com" || host.ends_with(".bandcamp.com")).then(|| format!("https://{host}"))
}

/// Why a cart request could not be completed.
pub(crate) enum CartError {
    /// The session cookie is missing or was rejected — re-authentication needed.
    AuthRequired(String),
    /// Any other failure (network, unexpected response, ...).
    Other(anyhow::Error),
}

/// Whether the loaded cookie is a valid logged-in Bandcamp session.
pub(crate) enum SessionStatus {
    /// No cookie is loaded.
    NoCookie,
    /// A cookie is loaded but Bandcamp does not recognize it (expired/invalid).
    Invalid,
    /// Logged in; `fan_id` if it could be read from the response.
    Valid { fan_id: Option<i64> },
    /// Could not be determined (e.g. a network error while checking).
    Unknown(String),
}

/// Heuristic: does this response body look like Bandcamp's login page rather
/// than a JSON API reply (i.e. the session expired)?
fn looks_like_login(body: &str) -> bool {
    let b = body.to_lowercase();
    b.contains("<html") && (b.contains("/login") || b.contains("log in") || b.contains("sign up"))
}

/// Resolve the path of the cached cookie file: `$BANDCAMP_COOKIE_FILE`, else
/// `$XDG_CONFIG_HOME/bandcamp_mcp_server/cookie`, else `~/.config/...`.
fn cookie_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("BANDCAMP_COOKIE_FILE") {
        return PathBuf::from(p);
    }
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("bandcamp_mcp_server").join("cookie")
}

/// Client holding a reusable HTTP connection pool plus cart auth state.
pub struct BandcampClient {
    http: reqwest::Client,
    /// Guards every outbound request against hammering Bandcamp's API.
    limiter: RateLimiter,
    /// The `Cookie` header for authenticated (cart) requests. Refreshable at
    /// runtime via [`save_cookie`]; loaded at startup from env or the cookie file.
    ///
    /// [`save_cookie`]: Self::save_cookie
    cookie: RwLock<Option<String>>,
    /// Where the cookie is persisted so it survives restarts.
    cookie_file: PathBuf,
    /// Whether cart mutations are actually sent. Gated by `BANDCAMP_ALLOW_CART_WRITES`.
    allow_cart_writes: bool,
}

impl BandcampClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");

        // Rate-limit config. Concurrency is the requested knob; the paced
        // interval is what actually prevents bursty hammering.
        let limiter = RateLimiter::from_env("BANDCAMP", 1, 750);

        let cookie_file = cookie_file_path();
        // Startup load: env var wins, then the cached file.
        let (cookie, source) = match std::env::var("BANDCAMP_COOKIE")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            Some(c) => (Some(c), "env BANDCAMP_COOKIE"),
            None => match std::fs::read_to_string(&cookie_file) {
                Ok(c) if !c.trim().is_empty() => (Some(c.trim().to_string()), "cookie file"),
                _ => (None, "none"),
            },
        };
        match &cookie {
            Some(_) => tracing::info!(%source, "loaded bandcamp session cookie"),
            None => tracing::info!(
                cookie_file = %cookie_file.display(),
                "no bandcamp session cookie found; call the `authenticate` tool to add one"
            ),
        }

        let allow_cart_writes = std::env::var("BANDCAMP_ALLOW_CART_WRITES")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Self {
            http,
            limiter,
            cookie: RwLock::new(cookie),
            cookie_file,
            allow_cart_writes,
        }
    }

    /// Whether cart mutations will actually be sent (vs. dry-run).
    pub(crate) fn cart_writes_enabled(&self) -> bool {
        self.allow_cart_writes
    }

    /// Path where the cookie is cached (for status/help messages).
    pub(crate) fn cookie_file(&self) -> &Path {
        &self.cookie_file
    }

    /// Persist a new session cookie to the cookie file and load it into memory.
    pub(crate) fn save_cookie(&self, cookie: &str) -> anyhow::Result<()> {
        let cookie = cookie.trim();
        if cookie.is_empty() {
            anyhow::bail!("cookie is empty");
        }
        if let Some(parent) = self.cookie_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&self.cookie_file, cookie)
            .with_context(|| format!("writing {}", self.cookie_file.display()))?;
        // Best-effort: restrict to owner (the file holds a credential).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.cookie_file, std::fs::Permissions::from_mode(0o600));
        }
        *self.cookie.write().unwrap() = Some(cookie.to_string());
        tracing::info!(cookie_file = %self.cookie_file.display(), "saved bandcamp session cookie");
        Ok(())
    }

    /// Check whether the loaded cookie is a valid logged-in session, via the
    /// identity endpoint (which replies `{"error":true,...}` when not logged in).
    #[tracing::instrument(
        name = "GET",
        skip(self),
        fields(
            otel.kind = "client",
            otel.name = "GET",
            otel.status_code = tracing::field::Empty,
            http.request.method = "GET",
            url.full = IDENTITY_URL,
            server.address = "bandcamp.com",
            http.response.status_code = tracing::field::Empty,
        )
    )]
    pub(crate) async fn verify_session(&self) -> SessionStatus {
        let span = tracing::Span::current();
        let Some(cookie) = self.cookie.read().unwrap().clone() else {
            return SessionStatus::NoCookie;
        };
        let _permit = self.limiter.acquire().await;
        let started = Instant::now();
        let resp = match self
            .http
            .get(IDENTITY_URL)
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                span.record("otel.status_code", "error");
                mcp_core::metrics::record_request(
                    "GET",
                    "bandcamp.com",
                    None,
                    None,
                    started.elapsed(),
                );
                return SessionStatus::Unknown(e.to_string());
            }
        };
        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "GET",
            "bandcamp.com",
            None,
            Some(status.as_u16()),
            started.elapsed(),
        );
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                span.record("otel.status_code", "error");
                return SessionStatus::Unknown(e.to_string());
            }
        };
        if json.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            return SessionStatus::Invalid;
        }
        let fan_id = json.get("fan_id").and_then(|v| v.as_i64()).or_else(|| {
            json.pointer("/collection_summary/fan_id")
                .and_then(|v| v.as_i64())
        });
        SessionStatus::Valid { fan_id }
    }

    /// Read the `bandcamp.com` session cookie directly from the local Chrome
    /// profile. On macOS this decrypts values via the Keychain, which triggers a
    /// one-time "Allow" prompt. The blocking read runs off the async runtime.
    pub(crate) async fn fetch_browser_cookie(&self) -> anyhow::Result<String> {
        let cookies =
            tokio::task::spawn_blocking(|| rookie::chrome(Some(vec!["bandcamp.com".to_string()])))
                .await
                .context("browser cookie read task failed")?
                .map_err(|e| anyhow::anyhow!("reading Chrome cookies failed: {e}"))?;

        if cookies.is_empty() {
            anyhow::bail!(
                "no bandcamp.com cookies found in Chrome; log in at bandcamp.com in Chrome first"
            );
        }
        let header = cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        tracing::info!(count = cookies.len(), "read bandcamp cookies from Chrome");
        Ok(header)
    }

    /// POST a form to `<origin>/cart/cb` with the session cookie and return the
    /// parsed JSON response. Callers must check [`cart_writes_enabled`] first;
    /// this always sends.
    ///
    /// [`cart_writes_enabled`]: Self::cart_writes_enabled
    #[tracing::instrument(
        name = "POST",
        skip(self, form),
        fields(
            otel.kind = "client",
            otel.name = "POST",
            otel.status_code = tracing::field::Empty,
            http.request.method = "POST",
            url.full = tracing::field::Empty,
            server.address = tracing::field::Empty,
            http.response.status_code = tracing::field::Empty,
        )
    )]
    pub(crate) async fn post_cart_cb(
        &self,
        origin: &str,
        form: &[(&str, String)],
    ) -> Result<serde_json::Value, CartError> {
        let span = tracing::Span::current();
        // Checked here rather than in the caller so that no tool can route a
        // cookie-bearing request to an origin it chose. This runs before the
        // cookie is even read.
        let origin = bandcamp_origin(origin).ok_or_else(|| {
            span.record("otel.status_code", "error");
            CartError::Other(anyhow::anyhow!(
                "refusing to send the session cookie to non-bandcamp origin {origin:?}"
            ))
        })?;
        let cookie = self.cookie.read().unwrap().clone().ok_or_else(|| {
            span.record("otel.status_code", "error");
            CartError::AuthRequired("no session cookie is loaded".to_string())
        })?;
        let url = format!("{origin}/cart/cb");
        let host = origin.trim_start_matches("https://").to_string();
        span.record("url.full", &url);
        span.record("server.address", &host);

        let _permit = self.limiter.acquire().await;
        tracing::info!(%url, "sending cart add request");
        let started = Instant::now();
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/javascript, */*; q=0.01",
            )
            .header("X-Requested-With", "XMLHttpRequest")
            .form(form)
            .send()
            .await
            .map_err(|e| {
                span.record("otel.status_code", "error");
                mcp_core::metrics::record_request("POST", &host, None, None, started.elapsed());
                CartError::Other(anyhow::Error::new(e).context("cart request failed"))
            })?;

        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "POST",
            &host,
            None,
            Some(status.as_u16()),
            started.elapsed(),
        );
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(400).collect();

        // 401/403, or a login page served instead of JSON, means the cookie expired.
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            span.record("otel.status_code", "error");
            return Err(CartError::AuthRequired(format!(
                "bandcamp rejected the session cookie (HTTP {status})"
            )));
        }
        if !status.is_success() {
            span.record("otel.status_code", "error");
            return Err(CartError::Other(anyhow::anyhow!(
                "cart returned HTTP {status}: {snippet}"
            )));
        }
        match serde_json::from_str(&text) {
            Ok(json) => Ok(json),
            Err(_) if looks_like_login(&text) => {
                span.record("otel.status_code", "error");
                Err(CartError::AuthRequired(
                    "bandcamp returned a login page; the session cookie is likely expired"
                        .to_string(),
                ))
            }
            Err(e) => Err(CartError::Other(anyhow::Error::new(e).context(format!(
                "cart returned non-JSON (HTTP {status}): {snippet}"
            )))),
        }
    }

    /// Query the autocomplete endpoint with a search-filter code.
    ///
    /// Filter codes: `"b"` = bands/artists, `"t"` = tracks, `"a"` = albums.
    #[tracing::instrument(
        name = "POST",
        skip(self),
        fields(
            otel.kind = "client",
            otel.name = "POST",
            http.request.method = "POST",
            url.full = SEARCH_URL,
            server.address = "bandcamp.com",
            http.response.status_code = tracing::field::Empty,
            search_text = %search_text,
            filter = %filter,
        ),
        err,
    )]
    pub(crate) async fn autocomplete(
        &self,
        search_text: &str,
        filter: &str,
    ) -> anyhow::Result<Vec<RawResult>> {
        let body = serde_json::json!({
            "search_text": search_text,
            "search_filter": filter,
            "full_page": false,
            "fan_id": null,
        });

        let _permit = self.limiter.acquire().await;
        tracing::debug!("querying bandcamp autocomplete");
        let started = Instant::now();
        let resp = self
            .http
            .post(SEARCH_URL)
            .json(&body)
            .send()
            .await
            .inspect_err(|_| {
                mcp_core::metrics::record_request(
                    "POST",
                    "bandcamp.com",
                    None,
                    None,
                    started.elapsed(),
                );
            })
            .context("request to bandcamp failed")?;

        let status = resp.status();
        tracing::Span::current().record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "POST",
            "bandcamp.com",
            None,
            Some(status.as_u16()),
            started.elapsed(),
        );
        if !status.is_success() {
            anyhow::bail!("bandcamp returned HTTP {status}");
        }

        let parsed: AutocompleteResponse = resp
            .json()
            .await
            .context("failed to parse bandcamp response")?;
        tracing::debug!(count = parsed.auto.results.len(), "autocomplete returned");
        Ok(parsed.auto.results)
    }

    /// The price an item's page advertises for it, or `None` if the page does
    /// not sell that item on its own (e.g. a track only sold with its album).
    ///
    /// `item_type` is Bandcamp's single-letter code (`"t"`, `"a"`, `"p"`).
    pub(crate) async fn item_price(
        &self,
        item_url: &str,
        item_type: &str,
        item_id: u64,
    ) -> anyhow::Result<Option<Price>> {
        let ld = self.item_page(item_url).await?;
        price_in(&ld, item_type, item_id)
    }

    /// What a track's page says about it: its price, and what identifies the
    /// recording (to confirm it is the song searched for).
    pub(crate) async fn track_page(
        &self,
        track_url: &str,
        track_id: u64,
    ) -> anyhow::Result<TrackPage> {
        let ld = self.item_page(track_url).await?;
        Ok(TrackPage {
            price: price_in(&ld, "t", track_id)?,
            recording: recording_in(&ld),
        })
    }

    /// Fetch an item's page and return its schema.org JSON-LD.
    ///
    /// The page is fetched **without** the session cookie: `item_url` comes
    /// from search results or the model, and nothing on the page needs a login.
    #[tracing::instrument(
        name = "GET",
        skip(self),
        fields(
            otel.kind = "client",
            otel.name = "GET",
            http.request.method = "GET",
            url.full = tracing::field::Empty,
            server.address = tracing::field::Empty,
            http.response.status_code = tracing::field::Empty,
        ),
        err,
    )]
    async fn item_page(&self, item_url: &str) -> anyhow::Result<serde_json::Value> {
        let span = tracing::Span::current();
        let origin = bandcamp_origin(item_url)
            .with_context(|| format!("{item_url:?} is not a bandcamp URL"))?;
        // Rebuilt from the validated origin, dropping any query or fragment.
        let path = reqwest::Url::parse(item_url)?.path().to_string();
        let url = format!("{origin}{path}");
        let host = origin.trim_start_matches("https://").to_string();
        span.record("url.full", &url);
        span.record("server.address", &host);

        let _permit = self.limiter.acquire().await;
        let started = Instant::now();
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .inspect_err(|_| {
                mcp_core::metrics::record_request("GET", &host, None, None, started.elapsed());
            })
            .context("fetching the item page failed")?;

        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "GET",
            &host,
            None,
            Some(status.as_u16()),
            started.elapsed(),
        );
        if !status.is_success() {
            anyhow::bail!("item page returned HTTP {status}");
        }
        // A redirect off Bandcamp would mean the data came from someone else's page.
        if bandcamp_origin(resp.url().as_str()).is_none() {
            anyhow::bail!("item page redirected off bandcamp, to {}", resp.url());
        }
        let html = resp.text().await.context("reading the item page failed")?;
        page_json_ld(&html)
    }
}

/// An item's price as its page advertises it: the minimum for digital items
/// ("name your price" ones included, where it may be 0), the fixed price for
/// packages.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Price {
    pub(crate) amount: f64,
    /// ISO 4217 code; the artist's currency, not the buyer's.
    pub(crate) currency: String,
}

/// What a track's page says about it.
#[derive(Debug, PartialEq)]
pub(crate) struct TrackPage {
    /// `None` when the track is not sold on its own.
    pub(crate) price: Option<Price>,
    pub(crate) recording: Recording,
}

/// What identifies a recording, where the page states it. Labels often leave
/// the ISRC out; the duration is nearly always there.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Recording {
    pub(crate) isrc: Option<String>,
    pub(crate) duration_secs: Option<u32>,
}

/// The schema.org JSON-LD block of an item page.
fn page_json_ld(html: &str) -> anyhow::Result<serde_json::Value> {
    const OPEN: &str = r#"<script type="application/ld+json">"#;
    let start = html.find(OPEN).context("item page has no JSON-LD")? + OPEN.len();
    let len = html[start..]
        .find("</script>")
        .context("item page's JSON-LD is unterminated")?;
    serde_json::from_str(&html[start..start + len]).context("item page's JSON-LD is invalid")
}

/// Find the price of one item in an item page's JSON-LD.
///
/// Every buyable thing on the page is an `Offer` whose `url` ends in
/// `#<type><id>-buy` — `#t…` the track, `#a…` the album, `#p…` a physical
/// package, `#b…` a discography bundle — so the fragment picks out exactly the
/// item asked for. `Ok(None)` means the page does not sell that item.
fn price_in(
    ld: &serde_json::Value,
    item_type: &str,
    item_id: u64,
) -> anyhow::Result<Option<Price>> {
    let suffix = format!("#{item_type}{item_id}-buy");
    let Some(offer) = find_offer(ld, &suffix) else {
        return Ok(None);
    };
    // Digital offers state their minimum as `minPrice`; packages have only a price.
    let amount = offer
        .pointer("/priceSpecification/minPrice")
        .or_else(|| offer.get("price"))
        .and_then(|v| v.as_f64())
        .context("the item's offer has no price")?;
    let currency = offer
        .get("priceCurrency")
        .and_then(|v| v.as_str())
        .context("the item's offer has no currency")?
        .to_string();
    Ok(Some(Price { amount, currency }))
}

/// The recording a track page is about: its top-level `MusicRecording`.
fn recording_in(ld: &serde_json::Value) -> Recording {
    Recording {
        isrc: ld
            .get("isrcCode")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string()),
        duration_secs: ld
            .get("duration")
            .and_then(|v| v.as_str())
            .and_then(parse_duration),
    }
}

/// Seconds in an ISO 8601 duration as Bandcamp writes it: `P00H03M10S`.
fn parse_duration(iso: &str) -> Option<u32> {
    let rest = iso.strip_prefix('P')?;
    // Strict ISO puts a `T` before the time part; Bandcamp omits it.
    let rest = rest.strip_prefix('T').unwrap_or(rest);
    let (hours, rest) = rest.split_once('H')?;
    let (minutes, rest) = rest.split_once('M')?;
    let seconds = rest.strip_suffix('S')?;
    Some(
        hours.parse::<u32>().ok()? * 3600
            + minutes.parse::<u32>().ok()? * 60
            + seconds.parse::<u32>().ok()?,
    )
}

/// Depth-first search for the `Offer` whose `url` ends with `suffix`.
fn find_offer<'a>(value: &'a serde_json::Value, suffix: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            let is_match = map.get("@type").and_then(|t| t.as_str()) == Some("Offer")
                && map
                    .get("url")
                    .and_then(|u| u.as_str())
                    .is_some_and(|u| u.ends_with(suffix));
            if is_match {
                return Some(value);
            }
            map.values().find_map(|v| find_offer(v, suffix))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_offer(v, suffix)),
        _ => None,
    }
}

impl Default for BandcampClient {
    fn default() -> Self {
        Self::new()
    }
}

// --- Raw wire types (Bandcamp autocomplete response) ---

#[derive(Debug, Deserialize)]
struct AutocompleteResponse {
    auto: Auto,
}

#[derive(Debug, Deserialize)]
struct Auto {
    #[serde(default)]
    results: Vec<RawResult>,
}

/// A single autocomplete result. Fields are optional because they vary by the
/// result `type`. Actions map this into their own output shape.
#[derive(Debug, Deserialize)]
pub(crate) struct RawResult {
    #[serde(rename = "type")]
    pub(crate) result_type: Option<String>,
    pub(crate) id: Option<i64>,
    pub(crate) name: Option<String>,
    pub(crate) band_id: Option<i64>,
    pub(crate) band_name: Option<String>,
    pub(crate) album_name: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) item_url_path: Option<String>,
    pub(crate) item_url_root: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        Price, Recording, bandcamp_origin, page_json_ld, parse_duration, price_in, recording_in,
    };

    fn parse_price(html: &str, item_type: &str, item_id: u64) -> anyhow::Result<Option<Price>> {
        price_in(&page_json_ld(html)?, item_type, item_id)
    }

    #[test]
    fn accepts_bandcamp_hosts() {
        assert_eq!(
            bandcamp_origin("https://bandcamp.com/cart"),
            Some("https://bandcamp.com".to_string())
        );
        assert_eq!(
            bandcamp_origin("https://naibu.bandcamp.com/track/x"),
            Some("https://naibu.bandcamp.com".to_string())
        );
        // Case and a trailing dot still name the same host.
        assert_eq!(
            bandcamp_origin("https://Naibu.Bandcamp.Com./track/x"),
            Some("https://naibu.bandcamp.com".to_string())
        );
    }

    #[test]
    fn upgrades_http_to_https() {
        assert_eq!(
            bandcamp_origin("http://bandcamp.com/album/x"),
            Some("https://bandcamp.com".to_string())
        );
    }

    #[test]
    fn rejects_lookalikes_and_other_hosts() {
        for hostile in [
            "https://bandcamp.com.evil.example/x",
            "https://evil-bandcamp.com/x",
            "https://notbandcamp.com/x",
            "https://attacker.example/x",
            "file:///etc/passwd",
            "not a url",
            "",
        ] {
            assert_eq!(
                bandcamp_origin(hostile),
                None,
                "`{hostile}` must be refused"
            );
        }
    }

    #[test]
    fn drops_port_and_userinfo() {
        assert_eq!(
            bandcamp_origin("https://user:pass@bandcamp.com:8443/cart"),
            Some("https://bandcamp.com".to_string())
        );
    }

    /// A track page, trimmed to the shape Bandcamp serves: the track's own
    /// offer sits beside the release's packages and a discography bundle, all
    /// nested under `inAlbum`.
    const TRACK_PAGE: &str = r##"<html><head>
<script type="application/ld+json">
{"@type":"MusicRecording","name":"Windowlicker","isrcCode":"GBBPW9900001","duration":"P00H06M07S","inAlbum":{"albumRelease":[
  {"@type":"MusicRelease","offers":{"@type":"Offer",
    "url":"https://aphextwin.bandcamp.com/track/windowlicker#t229736348-buy",
    "priceCurrency":"GBP","price":0.99,"priceSpecification":{"minPrice":0.99}}},
  {"@type":"MusicRelease","offers":{"@type":"Offer",
    "url":"https://aphextwin.bandcamp.com/track/windowlicker#p2080462178-buy",
    "priceCurrency":"GBP","price":12.99,"priceSpecification":{"price":12.99}}},
  {"@type":"MusicRelease","offers":{"@type":"Offer",
    "url":"https://aphextwin.bandcamp.com/track/windowlicker#b113942541-buy",
    "priceCurrency":"GBP","price":51.43,"priceSpecification":{"minPrice":51.43}}}
]}}
</script>
</head><body></body></html>"##;

    #[test]
    fn finds_the_requested_items_price() {
        assert_eq!(
            parse_price(TRACK_PAGE, "t", 229736348).unwrap(),
            Some(Price {
                amount: 0.99,
                currency: "GBP".into()
            })
        );
        // A package has a fixed price rather than a minimum.
        assert_eq!(
            parse_price(TRACK_PAGE, "p", 2080462178).unwrap(),
            Some(Price {
                amount: 12.99,
                currency: "GBP".into()
            })
        );
    }

    #[test]
    fn prefers_the_minimum_over_the_listed_price() {
        let page = TRACK_PAGE.replace(
            r#""price":0.99,"priceSpecification":{"minPrice":0.99}"#,
            r#""price":1.5,"priceSpecification":{"minPrice":1.0}"#,
        );
        assert_eq!(
            parse_price(&page, "t", 229736348).unwrap().unwrap().amount,
            1.0
        );
    }

    #[test]
    fn a_free_item_costs_zero_rather_than_nothing() {
        let page = TRACK_PAGE.replace(
            r#""price":0.99,"priceSpecification":{"minPrice":0.99}"#,
            r#""price":0.0,"priceSpecification":{"minPrice":0.0}"#,
        );
        assert_eq!(
            parse_price(&page, "t", 229736348).unwrap().unwrap().amount,
            0.0
        );
    }

    #[test]
    fn an_item_the_page_does_not_sell_has_no_price() {
        // Wrong type for the id: the fragment must match exactly, so a track id
        // passed as an album is not mistaken for the track.
        assert_eq!(parse_price(TRACK_PAGE, "a", 229736348).unwrap(), None);
        // An id that is a prefix of a real one must not match it.
        assert_eq!(parse_price(TRACK_PAGE, "t", 22973634).unwrap(), None);
    }

    #[test]
    fn a_page_without_structured_data_is_an_error() {
        assert!(parse_price("<html>login</html>", "t", 229736348).is_err());
    }

    #[test]
    fn reads_the_recordings_isrc_and_duration() {
        let ld = page_json_ld(TRACK_PAGE).unwrap();
        assert_eq!(
            recording_in(&ld),
            Recording {
                isrc: Some("GBBPW9900001".into()),
                duration_secs: Some(367),
            }
        );
    }

    #[test]
    fn a_page_without_them_identifies_nothing() {
        let ld = serde_json::json!({"@type": "MusicRecording", "isrcCode": ""});
        assert_eq!(recording_in(&ld), Recording::default());
    }

    #[test]
    fn parses_bandcamps_durations() {
        assert_eq!(parse_duration("P00H03M10S"), Some(190));
        assert_eq!(parse_duration("P01H00M01S"), Some(3601));
        assert_eq!(parse_duration("3:10"), None);
    }
}
