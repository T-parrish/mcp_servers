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
