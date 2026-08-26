//! Low-level client for the Spotify Web API.
//!
//! Owns everything shared by the tools: the HTTP connection pool, the outbound
//! rate limiter, the OAuth token set (persisted so it survives restarts, and
//! refreshed transparently when it expires), and the client-side spans and
//! metrics for every request.
//!
//! Per-action logic — which endpoint to call and how to shape the output — lives
//! in `crate::tools`.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use mcp_core::ratelimit::RateLimiter;
use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const API_HOST: &str = "api.spotify.com";
const ACCOUNTS_HOST: &str = "accounts.spotify.com";
const API_BASE: &str = "https://api.spotify.com";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

/// The scopes requested at authorization. Read-only, and the minimum needed to
/// enumerate the user's own playlists (including private and collaborative ones)
/// and read their contents.
pub(crate) const SCOPES: &str = "playlist-read-private playlist-read-collaborative";

/// Refresh this long before the access token actually expires, so a token does
/// not lapse mid-request.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Why a Spotify request could not be completed.
pub(crate) enum ApiError {
    /// No usable token, or Spotify rejected the one we hold — the user must run
    /// the `authenticate` tool again.
    AuthRequired(String),
    /// Any other failure (network, unexpected response, ...).
    Other(anyhow::Error),
}

impl ApiError {
    fn other(e: impl Into<anyhow::Error>) -> Self {
        ApiError::Other(e.into())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::AuthRequired(m) => write!(f, "{m}"),
            ApiError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// The token set as persisted to disk. `expires_at` is absolute (Unix seconds)
/// so it stays meaningful across restarts, unlike Spotify's relative `expires_in`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at: u64,
    #[serde(default)]
    pub(crate) scope: String,
}

impl Tokens {
    fn expired(&self) -> bool {
        now_secs() + EXPIRY_SKEW.as_secs() >= self.expires_at
    }
}

/// What the `authenticate` tool reports without starting a new login.
pub(crate) enum TokenStatus {
    /// No token set is stored.
    Missing,
    /// A token set is stored and still usable (possibly after a refresh).
    Valid { expires_at: u64, scope: String },
    /// Stored but expired with no refresh token — a fresh login is needed.
    Expired,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the path of the token file: `$SPOTIFY_TOKEN_FILE`, else
/// `$XDG_CONFIG_HOME/spotify_mcp_server/token.json`, else `~/.config/...`.
fn token_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SPOTIFY_TOKEN_FILE") {
        return PathBuf::from(p);
    }
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("spotify_mcp_server").join("token.json")
}

/// Client holding a reusable HTTP connection pool plus the OAuth token state.
pub struct SpotifyClient {
    http: reqwest::Client,
    /// Guards every outbound request against Spotify's rate limits.
    limiter: RateLimiter,
    /// The current token set, refreshed in place and mirrored to `token_file`.
    tokens: RwLock<Option<Tokens>>,
    /// Serializes token refreshes, so concurrent tool calls that all see an
    /// expired token issue one refresh request between them rather than one
    /// each. See [`SpotifyClient::refresh`].
    refresh_lock: tokio::sync::Mutex<()>,
    /// Where the token set is persisted so it survives restarts.
    token_file: PathBuf,
    /// The token endpoint. Always [`TOKEN_URL`] outside tests.
    token_url: String,
    /// The registered application's client ID. Without it, no login is possible.
    client_id: Option<String>,
    /// Loopback URI the authorization redirect comes back to. Must match one
    /// registered on the Spotify app exactly.
    redirect_uri: String,
}

impl SpotifyClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");

        // Spotify's published limits are per-app rolling windows rather than a
        // fixed rate, so these defaults just keep paginated listings polite.
        let limiter = RateLimiter::from_env("SPOTIFY", 4, 100);

        let client_id = std::env::var("SPOTIFY_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let redirect_uri = std::env::var("SPOTIFY_REDIRECT_URI")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());

        let token_file = token_file_path();
        let tokens = std::fs::read_to_string(&token_file)
            .ok()
            .and_then(|s| serde_json::from_str::<Tokens>(&s).ok());
        match &tokens {
            Some(t) => tracing::info!(
                token_file = %token_file.display(),
                expired = t.expired(),
                "loaded stored spotify tokens"
            ),
            None => tracing::info!(
                token_file = %token_file.display(),
                "no stored spotify tokens; call the `authenticate` tool to log in"
            ),
        }
        if client_id.is_none() {
            tracing::warn!("SPOTIFY_CLIENT_ID is not set; the `authenticate` tool cannot log in");
        }

        Self {
            http,
            limiter,
            tokens: RwLock::new(tokens),
            refresh_lock: tokio::sync::Mutex::new(()),
            token_file,
            token_url: TOKEN_URL.to_string(),
            client_id,
            redirect_uri,
        }
    }

    /// The configured client ID, if any.
    pub(crate) fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    /// The loopback redirect URI used by the authorization flow.
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Path where tokens are cached (for status/help messages).
    pub(crate) fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// What is currently stored, without contacting Spotify.
    pub(crate) fn token_status(&self) -> TokenStatus {
        match self.tokens.read().unwrap().as_ref() {
            None => TokenStatus::Missing,
            Some(t) if t.expired() && t.refresh_token.is_none() => TokenStatus::Expired,
            Some(t) => TokenStatus::Valid {
                expires_at: t.expires_at,
                scope: t.scope.clone(),
            },
        }
    }

    /// Persist a token set to the token file and load it into memory.
    fn store(&self, tokens: Tokens) -> anyhow::Result<()> {
        if let Some(parent) = self.token_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&tokens).context("serializing tokens")?;
        std::fs::write(&self.token_file, json)
            .with_context(|| format!("writing {}", self.token_file.display()))?;
        // Best-effort: restrict to owner (the file holds credentials).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.token_file, std::fs::Permissions::from_mode(0o600));
        }
        *self.tokens.write().unwrap() = Some(tokens);
        tracing::info!(token_file = %self.token_file.display(), "saved spotify tokens");
        Ok(())
    }

    /// Redeem an authorization code for a token set and store it. The PKCE
    /// verifier stands in for the client secret this flow does not have.
    pub(crate) async fn exchange_code(&self, code: &str, verifier: &str) -> anyhow::Result<Tokens> {
        let client_id = self
            .client_id
            .as_deref()
            .context("SPOTIFY_CLIENT_ID is not set")?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ];
        let tokens = self.post_token(&form, None).await?;
        self.store(tokens.clone())?;
        Ok(tokens)
    }

    /// Swap the stored refresh token for a new access token.
    ///
    /// Single-flighted: Spotify rotates the refresh token on this flow, so two
    /// concurrent refreshes with the same one would each be handed a different
    /// successor and the loser's would be the one persisted — leaving a stored
    /// refresh token Spotify has already superseded. Callers that arrive while a
    /// refresh is in flight wait for it and take its result.
    async fn refresh(&self) -> Result<Tokens, ApiError> {
        let _guard = self.refresh_lock.lock().await;

        // Whoever held the lock may have already refreshed; their token is the
        // current one, and asking for another would rotate this one out.
        let current = self.tokens.read().unwrap().clone();
        if let Some(tokens) = current
            && !tokens.expired()
        {
            return Ok(tokens);
        }

        let (refresh_token, previous_scope) = {
            let guard = self.tokens.read().unwrap();
            let tokens = guard
                .as_ref()
                .ok_or_else(|| ApiError::AuthRequired("no stored Spotify tokens".to_string()))?;
            let refresh = tokens.refresh_token.clone().ok_or_else(|| {
                ApiError::AuthRequired(
                    "the stored access token expired and there is no refresh token".to_string(),
                )
            })?;
            (refresh, tokens.scope.clone())
        };
        let client_id = self
            .client_id
            .as_deref()
            .ok_or_else(|| ApiError::AuthRequired("SPOTIFY_CLIENT_ID is not set".to_string()))?;

        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id),
        ];
        let mut tokens = match self.post_token(&form, Some(&refresh_token)).await {
            Ok(t) => {
                crate::metrics::record_token_refresh("ok");
                t
            }
            Err(e) => {
                crate::metrics::record_token_refresh("error");
                // A rejected refresh token is unrecoverable without the user.
                return Err(ApiError::AuthRequired(format!(
                    "refreshing the access token failed: {e:#}"
                )));
            }
        };
        // Spotify may omit both on refresh; carry the previous values forward.
        tokens.refresh_token = tokens.refresh_token.or(Some(refresh_token));
        if tokens.scope.is_empty() {
            tokens.scope = previous_scope;
        }
        self.store(tokens.clone()).map_err(ApiError::other)?;
        Ok(tokens)
    }

    /// POST to the token endpoint. `fallback_refresh` is the refresh token to
    /// keep when the response omits one.
    #[tracing::instrument(
        name = "POST",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = "POST",
            otel.status_code = tracing::field::Empty,
            http.request.method = "POST",
            url.full = %self.token_url,
            url.template = "/api/token",
            server.address = ACCOUNTS_HOST,
            http.response.status_code = tracing::field::Empty,
        ),
        err,
    )]
    async fn post_token(
        &self,
        form: &[(&str, &str)],
        fallback_refresh: Option<&str>,
    ) -> anyhow::Result<Tokens> {
        let span = tracing::Span::current();
        let _permit = self.limiter.acquire().await;
        let started = Instant::now();
        let resp = self
            .http
            .post(&self.token_url)
            .form(form)
            .send()
            .await
            .inspect_err(|_| {
                span.record("otel.status_code", "error");
                crate::metrics::record_request(
                    "POST",
                    ACCOUNTS_HOST,
                    Some("/api/token"),
                    None,
                    started.elapsed(),
                );
            })
            .context("token request to Spotify failed")?;

        let status = resp.status();
        span.record("http.response.status_code", status.as_u16());
        crate::metrics::record_request(
            "POST",
            ACCOUNTS_HOST,
            Some("/api/token"),
            Some(status.as_u16()),
            started.elapsed(),
        );
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            span.record("otel.status_code", "error");
            // The body is an OAuth error object; it carries no credentials.
            anyhow::bail!(
                "Spotify token endpoint returned HTTP {status}: {}",
                snippet(&body)
            );
        }

        let parsed: TokenResponse =
            serde_json::from_str(&body).context("failed to parse the token response")?;
        Ok(Tokens {
            access_token: parsed.access_token,
            refresh_token: parsed
                .refresh_token
                .or_else(|| fallback_refresh.map(str::to_string)),
            expires_at: now_secs() + parsed.expires_in,
            scope: parsed.scope.unwrap_or_default(),
        })
    }

    /// A usable access token, refreshing first if the stored one is at or near
    /// expiry.
    async fn access_token(&self) -> Result<String, ApiError> {
        let current = self.tokens.read().unwrap().clone();
        match current {
            None => Err(ApiError::AuthRequired(
                "not authenticated with Spotify".to_string(),
            )),
            Some(t) if t.expired() => Ok(self.refresh().await?.access_token),
            Some(t) => Ok(t.access_token),
        }
    }

    /// GET a Web API resource and deserialize it. `route` is the low-cardinality
    /// URL template used as a span/metric attribute.
    ///
    /// A 429 is retried once after the `Retry-After` delay; a 401 means the
    /// token was rejected and surfaces as [`ApiError::AuthRequired`].
    #[tracing::instrument(
        name = "GET",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = "GET",
            otel.status_code = tracing::field::Empty,
            http.request.method = "GET",
            url.full = %url,
            url.template = route,
            server.address = API_HOST,
            http.response.status_code = tracing::field::Empty,
            http.request.resend_count = tracing::field::Empty,
        ),
    )]
    async fn get<T: DeserializeOwned>(&self, route: &'static str, url: Url) -> Result<T, ApiError> {
        let span = tracing::Span::current();
        let token = self.access_token().await?;

        for attempt in 0..2 {
            let _permit = self.limiter.acquire().await;
            let started = Instant::now();
            let resp = self
                .http
                .get(url.clone())
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| {
                    span.record("otel.status_code", "error");
                    crate::metrics::record_request(
                        "GET",
                        API_HOST,
                        Some(route),
                        None,
                        started.elapsed(),
                    );
                    ApiError::other(anyhow::Error::new(e).context("request to Spotify failed"))
                })?;

            let status = resp.status();
            span.record("http.response.status_code", status.as_u16());
            crate::metrics::record_request(
                "GET",
                API_HOST,
                Some(route),
                Some(status.as_u16()),
                started.elapsed(),
            );

            // Rate limited: honour Retry-After once, then give up.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(30);
                tracing::warn!(retry_after, %route, "rate limited by Spotify; retrying");
                span.record("http.request.resend_count", 1);
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("otel.status_code", "error");
                return Err(ApiError::AuthRequired(
                    "Spotify rejected the access token (HTTP 401)".to_string(),
                ));
            }
            if status == reqwest::StatusCode::FORBIDDEN {
                span.record("otel.status_code", "error");
                return Err(ApiError::AuthRequired(format!(
                    "Spotify refused the request (HTTP 403); the stored token may lack the \
                     required scopes ({SCOPES})"
                )));
            }
            let body = resp.text().await.map_err(ApiError::other)?;
            if !status.is_success() {
                span.record("otel.status_code", "error");
                return Err(ApiError::Other(anyhow::anyhow!(
                    "Spotify returned HTTP {status}: {}",
                    snippet(&body)
                )));
            }
            return serde_json::from_str(&body).map_err(|e| {
                span.record("otel.status_code", "error");
                ApiError::other(
                    anyhow::Error::new(e)
                        .context(format!("failed to parse the response from {route}")),
                )
            });
        }
        Err(ApiError::Other(anyhow::anyhow!(
            "Spotify kept rate limiting {route}"
        )))
    }

    /// GET a single resource under the API base, e.g. `/v1/me`.
    pub(crate) async fn get_path<T: DeserializeOwned>(
        &self,
        route: &'static str,
        path: &str,
    ) -> Result<T, ApiError> {
        let url = api_url(path, &[]).map_err(ApiError::other)?;
        self.get(route, url).await
    }

    /// The profile of the account the stored tokens belong to. Doubles as the
    /// check that a login is actually accepted by Spotify.
    pub(crate) async fn current_user(&self) -> Result<CurrentUser, ApiError> {
        self.get_path("/v1/me", "/v1/me").await
    }

    /// GET a paginated collection, following `next` until `limit` items have
    /// been collected or the collection is exhausted.
    ///
    /// Returns the items alongside the collection's reported `total`.
    pub(crate) async fn get_paged<T: DeserializeOwned>(
        &self,
        route: &'static str,
        path: &str,
        query: &[(&str, String)],
        limit: usize,
    ) -> Result<(Vec<T>, Option<u32>), ApiError> {
        // Spotify caps `limit` at 50 for these endpoints; ask for as much of the
        // remainder as it will give per request.
        let page_size = limit.clamp(1, 50);
        let mut query = query.to_vec();
        query.push(("limit", page_size.to_string()));
        let mut next = Some(api_url(path, &query).map_err(ApiError::other)?);

        let mut items = Vec::new();
        let mut total = None;
        while let Some(url) = next.take() {
            let page: Page<T> = self.get(route, url).await?;
            total = page.total.or(total);
            items.extend(page.items);
            if items.len() >= limit {
                items.truncate(limit);
                break;
            }
            next = match page.next {
                Some(url) => Some(Url::parse(&url).map_err(ApiError::other)?),
                None => None,
            };
        }
        tracing::debug!(count = items.len(), total, %route, "collected paginated items");
        Ok((items, total))
    }
}

impl Default for SpotifyClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build an absolute Web API URL from a path and query parameters.
fn api_url(path: &str, query: &[(&str, String)]) -> anyhow::Result<Url> {
    let mut url = Url::parse(API_BASE)
        .expect("the API base is a valid constant")
        .join(path)
        .with_context(|| format!("building a URL for {path}"))?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Ok(url)
}

/// Trim a response body down to something safe to put in an error message.
fn snippet(body: &str) -> String {
    body.chars().take(300).collect()
}

// --- Wire types ---

/// The subset of `GET /v1/me` this server reports.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CurrentUser {
    pub(crate) id: String,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: Option<String>,
}

/// Spotify's paging object. Only the fields this server needs are modelled.
#[derive(Debug, Deserialize)]
struct Page<T> {
    // Spelled out rather than `#[serde(default)]`, which would add a needless
    // `T: Default` bound to the derived impl.
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    next: Option<String>,
    total: Option<u32>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A token endpoint that counts the requests it serves and hands each one a
    /// distinct token pair, so a duplicate refresh is visible in both the count
    /// and in which tokens the callers end up with.
    ///
    /// `delay` is how long a request is held open before answering — long enough
    /// for every caller to have reached the refresh.
    async fn mock_token_endpoint(delay: Duration) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));

        let served = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let served = Arc::clone(&served);
                tokio::spawn(async move {
                    let n = served.fetch_add(1, Ordering::SeqCst) + 1;
                    // The request is not inspected; one read is enough to let the
                    // client finish writing it.
                    let _ = socket.read(&mut [0u8; 4096]).await;
                    tokio::time::sleep(delay).await;
                    let body = format!(
                        r#"{{"access_token":"access-{n}","refresh_token":"refresh-{n}","expires_in":3600,"scope":"{SCOPES}"}}"#
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{addr}/api/token"), requests)
    }

    /// A client wired to `token_url`, holding `tokens`, persisting to a token
    /// file of its own.
    fn test_client(token_url: String, tokens: Tokens) -> SpotifyClient {
        static N: AtomicUsize = AtomicUsize::new(0);
        let token_file = std::env::temp_dir().join(format!(
            "spotify_mcp_test_{}_{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        SpotifyClient {
            http: reqwest::Client::new(),
            limiter: RateLimiter::new(4, Duration::ZERO),
            tokens: RwLock::new(Some(tokens)),
            refresh_lock: tokio::sync::Mutex::new(()),
            token_file,
            token_url,
            client_id: Some("test-client-id".to_string()),
            redirect_uri: DEFAULT_REDIRECT_URI.to_string(),
        }
    }

    fn expired_tokens() -> Tokens {
        Tokens {
            access_token: "stale-access".to_string(),
            refresh_token: Some("stored-refresh".to_string()),
            // Inside the skew window, so `expired()` is true.
            expires_at: now_secs(),
            scope: SCOPES.to_string(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_refresh_once() {
        let (token_url, requests) = mock_token_endpoint(Duration::from_millis(150)).await;
        let client = Arc::new(test_client(token_url, expired_tokens()));

        let calls: Vec<_> = (0..8)
            .map(|_| {
                let client = Arc::clone(&client);
                tokio::spawn(async move { client.access_token().await.map_err(|e| e.to_string()) })
            })
            .collect();
        let mut tokens = Vec::new();
        for call in calls {
            tokens.push(call.await.unwrap().expect("access token"));
        }

        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "one refresh between them"
        );
        // Every caller — not just the one that did the work — ends up with the
        // token that refresh produced.
        assert!(
            tokens.iter().all(|t| t == "access-1"),
            "callers disagreed on the access token: {tokens:?}"
        );
        // And the refresh token stored is the one Spotify handed back with it.
        let stored = client.tokens.read().unwrap().clone().unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-1"));

        let _ = std::fs::remove_file(client.token_file());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_valid_token_is_used_without_refreshing() {
        let (token_url, requests) = mock_token_endpoint(Duration::ZERO).await;
        let tokens = Tokens {
            expires_at: now_secs() + 3600,
            ..expired_tokens()
        };
        let client = test_client(token_url, tokens);

        let token = client.access_token().await.map_err(|e| e.to_string());
        assert_eq!(token.unwrap(), "stale-access");
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_file(client.token_file());
    }
}
