//! Low-level client over Beatport's v4 API.
//!
//! The API is official, but its credentials are not ours: until Beatport issues
//! this project its own OAuth client, the bearer token is one copied by hand from
//! a logged-in session on Beatport's API docs page. Everything that knows where a
//! token comes from is in this module, so replacing that with a real OAuth flow
//! later touches nothing else.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use mcp_core::ratelimit::RateLimiter;
use serde::Deserialize;

const API_HOST: &str = "api.beatport.com";
const API_BASE: &str = "https://api.beatport.com/v4";
const INTROSPECT_URL: &str = "https://api.beatport.com/v4/auth/o/introspect/";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; beatport_mcp_server/0.1)";

/// Why a catalog request failed.
pub(crate) enum ApiError {
    /// No token is loaded, or Beatport rejected it — re-authentication needed.
    AuthRequired(String),
    /// Any other failure (network, unexpected response, ...).
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Whether the loaded token is a valid logged-in Beatport session.
pub(crate) enum SessionStatus {
    /// No token is loaded.
    NoToken,
    /// A token is loaded but Beatport does not recognize it (expired/invalid).
    Invalid,
    /// Logged in as `username`.
    Valid { username: String },
    /// Could not be determined (e.g. a network error while checking).
    Unknown(String),
}

/// Resolve the path of the cached token file: `$BEATPORT_TOKEN_FILE`, else
/// `$XDG_CONFIG_HOME/beatport_mcp_server/token`, else `~/.config/...`.
fn token_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("BEATPORT_TOKEN_FILE") {
        return PathBuf::from(p);
    }
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("beatport_mcp_server").join("token")
}

/// Accept a token however it was copied: bare, or as the whole
/// `Authorization: Bearer …` header value.
fn normalize_token(token: &str) -> &str {
    let token = token.trim();
    token
        .strip_prefix("Authorization:")
        .map(str::trim)
        .unwrap_or(token)
        .strip_prefix("Bearer ")
        .map(str::trim)
        .unwrap_or(token)
}

/// Client holding a reusable HTTP connection pool plus the bearer token.
pub struct BeatportClient {
    http: reqwest::Client,
    /// Guards every outbound request.
    limiter: RateLimiter,
    /// The bearer token. Refreshable at runtime via [`save_token`]; loaded at
    /// startup from env or the token file.
    ///
    /// [`save_token`]: Self::save_token
    token: RwLock<Option<String>>,
    /// Where the token is persisted so it survives restarts.
    token_file: PathBuf,
    /// The AIFF surcharge, read once from the account's purchase history.
    aiff_fee: tokio::sync::OnceCell<RawPrice>,
}

impl BeatportClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");

        // Beatport publishes no rate limit, so start conservative.
        let limiter = RateLimiter::from_env("BEATPORT", 1, 500);

        let token_file = token_file_path();
        // Startup load: env var wins, then the cached file.
        let (token, source) = match std::env::var("BEATPORT_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            Some(t) => (
                Some(normalize_token(&t).to_string()),
                "env BEATPORT_AUTH_TOKEN",
            ),
            None => match std::fs::read_to_string(&token_file) {
                Ok(t) if !t.trim().is_empty() => {
                    (Some(normalize_token(&t).to_string()), "token file")
                }
                _ => (None, "none"),
            },
        };
        match &token {
            Some(_) => tracing::info!(%source, "loaded beatport token"),
            None => tracing::info!(
                token_file = %token_file.display(),
                "no beatport token found; call the `authenticate` tool to add one"
            ),
        }

        Self {
            http,
            limiter,
            token: RwLock::new(token),
            token_file,
            aiff_fee: tokio::sync::OnceCell::new(),
        }
    }

    /// Path where the token is cached (for status/help messages).
    pub(crate) fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// Persist a new token to the token file and load it into memory.
    pub(crate) fn save_token(&self, token: &str) -> anyhow::Result<()> {
        let token = normalize_token(token);
        if token.is_empty() {
            anyhow::bail!("token is empty");
        }
        if let Some(parent) = self.token_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&self.token_file, token)
            .with_context(|| format!("writing {}", self.token_file.display()))?;
        // Best-effort: restrict to owner (the file holds a credential).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.token_file, std::fs::Permissions::from_mode(0o600));
        }
        *self.token.write().unwrap() = Some(token.to_string());
        tracing::info!(token_file = %self.token_file.display(), "saved beatport token");
        Ok(())
    }

    /// Check whether the loaded token is a valid logged-in session.
    ///
    /// The introspect endpoint answers 200 whether or not the token is any
    /// good — an unrecognized one is treated as anonymous — so validity is
    /// whether it names a user, not the status code.
    #[tracing::instrument(
        name = "GET",
        skip(self),
        fields(
            otel.kind = "client",
            otel.name = "GET",
            otel.status_code = tracing::field::Empty,
            http.request.method = "GET",
            url.full = INTROSPECT_URL,
            server.address = API_HOST,
            http.response.status_code = tracing::field::Empty,
        )
    )]
    pub(crate) async fn verify_session(&self) -> SessionStatus {
        let span = tracing::Span::current();
        let Some(token) = self.token.read().unwrap().clone() else {
            return SessionStatus::NoToken;
        };
        let _permit = self.limiter.acquire().await;
        let started = Instant::now();
        let resp = match self
            .http
            .get(INTROSPECT_URL)
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                span.record("otel.status_code", "error");
                mcp_core::metrics::record_request("GET", API_HOST, None, None, started.elapsed());
                return SessionStatus::Unknown(e.to_string());
            }
        };
        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "GET",
            API_HOST,
            None,
            Some(status.as_u16()),
            started.elapsed(),
        );
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return SessionStatus::Invalid;
        }
        if !status.is_success() {
            span.record("otel.status_code", "error");
            return SessionStatus::Unknown(format!("introspect returned HTTP {status}"));
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                span.record("otel.status_code", "error");
                return SessionStatus::Unknown(e.to_string());
            }
        };
        if json.get("user_id").is_none_or(|v| v.is_null()) {
            return SessionStatus::Invalid;
        }
        let username = json
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        SessionStatus::Valid { username }
    }
}

impl BeatportClient {
    /// Tracks whose ISRC is exactly `isrc` — usually one, sometimes a few
    /// releases of the same recording.
    pub(crate) async fn tracks_by_isrc(&self, isrc: &str) -> Result<Vec<RawTrack>, ApiError> {
        let list: TrackList = self
            .get_catalog("/catalog/tracks/", &[("isrc", isrc), ("per_page", "25")])
            .await?;
        Ok(list.tracks)
    }

    /// Full-text track search, best matches first.
    pub(crate) async fn search_tracks(&self, query: &str) -> Result<Vec<RawTrack>, ApiError> {
        let list: TrackList = self
            .get_catalog(
                "/catalog/search/",
                &[("q", query), ("type", "tracks"), ("per_page", "25")],
            )
            .await?;
        Ok(list.tracks)
    }

    /// What buying a track as AIFF costs on top of its listed price.
    ///
    /// The catalog lists only the base price, and the format price list is
    /// closed to this token. But every purchase in the account's download
    /// history carries the lossless surcharges (`upgrade_fees`), which are flat
    /// per account — +0.75 USD across 300 real purchases at base prices of
    /// 1.49, 1.69 and 2.49. So the fee is read from there, once, rather than
    /// hard-coded where it would silently go stale.
    pub(crate) async fn aiff_fee(&self) -> Result<RawPrice, ApiError> {
        let fee = self
            .aiff_fee
            .get_or_try_init(|| async {
                let list: DownloadList = self
                    .get_catalog("/my/downloads/", &[("per_page", "1")])
                    .await?;
                list.results
                    .into_iter()
                    .next()
                    .and_then(|d| d.upgrade_fees.aiff)
                    .ok_or_else(|| {
                        ApiError::Other(anyhow::anyhow!(
                            "the account has no purchase to read the AIFF fee from"
                        ))
                    })
            })
            .await?;
        Ok(fee.clone())
    }

    /// An authenticated GET against the catalog, decoded as `T`.
    #[tracing::instrument(
        name = "GET",
        skip(self, query),
        fields(
            otel.kind = "client",
            otel.name = "GET",
            otel.status_code = tracing::field::Empty,
            http.request.method = "GET",
            url.path = path,
            server.address = API_HOST,
            http.response.status_code = tracing::field::Empty,
        )
    )]
    async fn get_catalog<T: serde::de::DeserializeOwned>(
        &self,
        path: &'static str,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        let span = tracing::Span::current();
        let token = self.token.read().unwrap().clone().ok_or_else(|| {
            span.record("otel.status_code", "error");
            ApiError::AuthRequired("no token is loaded".to_string())
        })?;

        let url = reqwest::Url::parse_with_params(&format!("{API_BASE}{path}"), query)
            .context("building the request URL")?;

        let _permit = self.limiter.acquire().await;
        let started = Instant::now();
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                span.record("otel.status_code", "error");
                mcp_core::metrics::record_request(
                    "GET",
                    API_HOST,
                    Some(path),
                    None,
                    started.elapsed(),
                );
                anyhow::Error::new(e).context("request to beatport failed")
            })?;

        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        mcp_core::metrics::record_request(
            "GET",
            API_HOST,
            Some(path),
            Some(status.as_u16()),
            started.elapsed(),
        );
        // An expired or unrecognized token is treated as anonymous, which the
        // catalog refuses with 401.
        if status == reqwest::StatusCode::UNAUTHORIZED {
            span.record("otel.status_code", "error");
            return Err(ApiError::AuthRequired(
                "beatport rejected the token; it has likely expired".to_string(),
            ));
        }
        if !status.is_success() {
            span.record("otel.status_code", "error");
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(anyhow::anyhow!("beatport returned HTTP {status}: {body}").into());
        }
        Ok(resp
            .json()
            .await
            .context("failed to parse beatport response")?)
    }
}

// --- Raw wire types (Beatport v4 catalog) ---

/// A page of tracks: `tracks` from search, `results` from the tracks listing.
#[derive(Debug, Deserialize)]
struct TrackList {
    #[serde(alias = "results", default)]
    tracks: Vec<RawTrack>,
}

/// The fields of a catalog track this server uses.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTrack {
    pub(crate) id: u64,
    pub(crate) name: String,
    /// e.g. "Original Mix", "Extended Mix", "Logistics Remix".
    pub(crate) mix_name: Option<String>,
    pub(crate) slug: String,
    pub(crate) isrc: Option<String>,
    /// When the release this listing belongs to came out (`YYYY-MM-DD`).
    pub(crate) new_release_date: Option<String>,
    #[serde(default)]
    pub(crate) artists: Vec<RawArtist>,
    pub(crate) price: Option<RawPrice>,
    pub(crate) sale_type: Option<RawSaleType>,
}

impl RawTrack {
    /// The track's page on beatport.com (the `url` field is the API's own).
    pub(crate) fn web_url(&self) -> String {
        format!("https://www.beatport.com/track/{}/{}", self.slug, self.id)
    }

    /// Its price, if it is for sale. Beatport lists a price on every track, but
    /// only a `purchase` sale type means it can actually be bought.
    pub(crate) fn purchase_price(&self) -> Option<&RawPrice> {
        let buyable = self
            .sale_type
            .as_ref()
            .is_some_and(|s| s.name == "purchase");
        self.price.as_ref().filter(|_| buyable)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawArtist {
    pub(crate) name: String,
}

/// A page of the account's download (purchase) history.
#[derive(Debug, Deserialize)]
struct DownloadList {
    #[serde(default)]
    results: Vec<RawDownload>,
}

#[derive(Debug, Deserialize)]
struct RawDownload {
    #[serde(default)]
    upgrade_fees: RawUpgradeFees,
}

/// Surcharges for lossless formats, keyed by format name.
#[derive(Debug, Default, Deserialize)]
struct RawUpgradeFees {
    aiff: Option<RawPrice>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawPrice {
    pub(crate) value: f64,
    /// ISO 4217 code.
    pub(crate) code: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSaleType {
    pub(crate) name: String,
}

impl Default for BeatportClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_token;

    #[test]
    fn accepts_a_token_however_it_was_copied() {
        for copied in [
            "abc123",
            "  abc123\n",
            "Bearer abc123",
            "Authorization: Bearer abc123",
        ] {
            assert_eq!(normalize_token(copied), "abc123", "from {copied:?}");
        }
    }
}
