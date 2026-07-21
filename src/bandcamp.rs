//! Low-level client over Bandcamp's *undocumented* internal API.
//!
//! Bandcamp has no official public API. These endpoints are the same ones the
//! bandcamp.com website calls, so they can change or break without notice.
//!
//! This module only provides the shared HTTP primitive and wire types. The
//! per-action logic (filtering, output shaping) lives in `crate::tools`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::Deserialize;
use tokio::sync::{Semaphore, SemaphorePermit};

const SEARCH_URL: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const IDENTITY_URL: &str = "https://bandcamp.com/api/fan/2/collection_summary";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; bandcamp_mcp_server/0.1)";

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

/// A small jitter in `[0, span/2]`, using the clock as cheap entropy so paced
/// requests don't land on an exact grid.
fn jitter(span: Duration) -> Duration {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    span.mul_f64((n as f64 / 1_000_000_000.0) * 0.5)
}

/// Bounds outbound requests: at most `max_concurrent` in flight, and request
/// *starts* spaced at least `min_interval` (plus jitter) apart.
struct RateLimiter {
    semaphore: Semaphore,
    min_interval: Duration,
    /// Earliest instant the next request may start.
    next_slot: Mutex<Instant>,
}

impl RateLimiter {
    fn new(max_concurrent: usize, min_interval: Duration) -> Self {
        Self {
            semaphore: Semaphore::new(max_concurrent),
            min_interval,
            next_slot: Mutex::new(Instant::now()),
        }
    }

    /// Wait for a concurrency slot and the paced start time, returning a permit
    /// that must be held for the duration of the request.
    async fn acquire(&self) -> SemaphorePermit<'_> {
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("rate-limiter semaphore is never closed");

        // Reserve a paced start slot. The std mutex is dropped before awaiting.
        let start_at = {
            let mut slot = self.next_slot.lock().unwrap();
            let start_at = (*slot).max(Instant::now());
            *slot = start_at + self.min_interval + jitter(self.min_interval);
            start_at
        };
        if let Some(delay) = start_at.checked_duration_since(Instant::now()) {
            tokio::time::sleep(delay).await;
        }
        permit
    }
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
        let max_concurrent = std::env::var("BANDCAMP_MAX_CONCURRENT_REQUESTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1);
        let min_interval_ms = std::env::var("BANDCAMP_MIN_REQUEST_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(750);
        tracing::info!(max_concurrent, min_interval_ms, "configured request rate limiter");
        let limiter = RateLimiter::new(max_concurrent, Duration::from_millis(min_interval_ms));

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
            let _ = std::fs::set_permissions(
                &self.cookie_file,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        *self.cookie.write().unwrap() = Some(cookie.to_string());
        tracing::info!(cookie_file = %self.cookie_file.display(), "saved bandcamp session cookie");
        Ok(())
    }

    /// Check whether the loaded cookie is a valid logged-in session, via the
    /// identity endpoint (which replies `{"error":true,...}` when not logged in).
    #[tracing::instrument(skip(self))]
    pub(crate) async fn verify_session(&self) -> SessionStatus {
        let Some(cookie) = self.cookie.read().unwrap().clone() else {
            return SessionStatus::NoCookie;
        };
        let _permit = self.limiter.acquire().await;
        let resp = match self
            .http
            .get(IDENTITY_URL)
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return SessionStatus::Unknown(e.to_string()),
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => return SessionStatus::Unknown(e.to_string()),
        };
        if json.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            return SessionStatus::Invalid;
        }
        let fan_id = json
            .get("fan_id")
            .and_then(|v| v.as_i64())
            .or_else(|| json.pointer("/collection_summary/fan_id").and_then(|v| v.as_i64()));
        SessionStatus::Valid { fan_id }
    }

    /// Read the `bandcamp.com` session cookie directly from the local Chrome
    /// profile. On macOS this decrypts values via the Keychain, which triggers a
    /// one-time "Allow" prompt. The blocking read runs off the async runtime.
    pub(crate) async fn fetch_browser_cookie(&self) -> anyhow::Result<String> {
        let cookies = tokio::task::spawn_blocking(|| {
            rookie::chrome(Some(vec!["bandcamp.com".to_string()]))
        })
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
    #[tracing::instrument(skip(self, form), fields(origin = %origin))]
    pub(crate) async fn post_cart_cb(
        &self,
        origin: &str,
        form: &[(&str, String)],
    ) -> Result<serde_json::Value, CartError> {
        let cookie = self.cookie.read().unwrap().clone().ok_or_else(|| {
            CartError::AuthRequired("no session cookie is loaded".to_string())
        })?;
        let url = format!("{origin}/cart/cb");

        let _permit = self.limiter.acquire().await;
        tracing::info!(%url, "sending cart add request");
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ACCEPT, "application/json, text/javascript, */*; q=0.01")
            .header("X-Requested-With", "XMLHttpRequest")
            .form(form)
            .send()
            .await
            .map_err(|e| CartError::Other(anyhow::Error::new(e).context("cart request failed")))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(400).collect();

        // 401/403, or a login page served instead of JSON, means the cookie expired.
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(CartError::AuthRequired(format!(
                "bandcamp rejected the session cookie (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Err(CartError::Other(anyhow::anyhow!(
                "cart returned HTTP {status}: {snippet}"
            )));
        }
        match serde_json::from_str(&text) {
            Ok(json) => Ok(json),
            Err(_) if looks_like_login(&text) => Err(CartError::AuthRequired(
                "bandcamp returned a login page; the session cookie is likely expired".to_string(),
            )),
            Err(e) => Err(CartError::Other(anyhow::Error::new(e).context(format!(
                "cart returned non-JSON (HTTP {status}): {snippet}"
            )))),
        }
    }

    /// Query the autocomplete endpoint with a search-filter code.
    ///
    /// Filter codes: `"b"` = bands/artists, `"t"` = tracks, `"a"` = albums.
    #[tracing::instrument(skip(self), fields(search_text = %search_text, filter = %filter))]
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
        let resp = self
            .http
            .post(SEARCH_URL)
            .json(&body)
            .send()
            .await
            .context("request to bandcamp failed")?;

        let status = resp.status();
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
