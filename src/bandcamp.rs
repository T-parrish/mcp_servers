//! Low-level client over Bandcamp's *undocumented* internal API.
//!
//! Bandcamp has no official public API. These endpoints are the same ones the
//! bandcamp.com website calls, so they can change or break without notice.
//!
//! This module only provides the shared HTTP primitive and wire types. The
//! per-action logic (filtering, output shaping) lives in `crate::tools`.

use anyhow::Context;
use serde::Deserialize;

const SEARCH_URL: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; bandcamp_mcp_server/0.1)";

/// Client holding a reusable HTTP connection pool.
pub struct BandcampClient {
    http: reqwest::Client,
}

impl BandcampClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");
        Self { http }
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
    pub(crate) band_name: Option<String>,
    pub(crate) album_name: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) item_url_path: Option<String>,
    pub(crate) item_url_root: Option<String>,
}
