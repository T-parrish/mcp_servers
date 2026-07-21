//! Thin client over Bandcamp's *undocumented* internal search API.
//!
//! Bandcamp has no official public API. These endpoints are the same ones the
//! bandcamp.com website calls, so they can change or break without notice.

use anyhow::Context;
use serde::{Deserialize, Serialize};

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

    /// Search for artists / bands by name.
    #[tracing::instrument(skip(self), fields(query = %query, limit))]
    pub async fn search_artists(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Artist>> {
        // "b" = band/artist entity. ("a" is albums, "t" is tracks.)
        let results = self.autocomplete(query, "b").await?;
        let artists: Vec<Artist> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("b"))
            .take(limit)
            .map(Artist::from)
            .collect();
        tracing::info!(count = artists.len(), "artist search complete");
        Ok(artists)
    }

    /// Search for songs (tracks) by a given artist.
    ///
    /// `query` optionally narrows the search (e.g. a partial track title). Results
    /// are filtered to tracks whose band name matches `artist` (case-insensitive).
    #[tracing::instrument(skip(self), fields(artist = %artist, limit))]
    pub async fn search_songs(
        &self,
        artist: &str,
        query: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Song>> {
        let search_text = match query {
            Some(q) => format!("{artist} {q}"),
            None => artist.to_string(),
        };
        let results = self.autocomplete(&search_text, "t").await?;
        let artist_lc = artist.to_lowercase();
        let songs: Vec<Song> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("t"))
            .filter(|r| {
                r.band_name
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&artist_lc))
            })
            .take(limit)
            .map(Song::from)
            .collect();
        tracing::info!(count = songs.len(), "song search complete");
        Ok(songs)
    }

    /// Add an item to the cart.
    ///
    /// STUB: a real Bandcamp cart requires an authenticated user session and the
    /// site's non-public purchase flow, which is intentionally not implemented
    /// here. This records intent and returns a simulated result.
    #[tracing::instrument(skip(self))]
    pub fn add_to_cart(
        &self,
        item_id: u64,
        item_type: &str,
        item_name: Option<&str>,
        price: Option<f64>,
    ) -> CartResult {
        tracing::warn!(
            item_id,
            item_type,
            "add_to_cart is stubbed: no real Bandcamp cart mutation is performed"
        );
        CartResult {
            status: "simulated",
            message: format!(
                "Simulated adding {item_type} {item_id} to cart. Real Bandcamp cart \
                 integration is not implemented (it requires an authenticated session)."
            ),
            item_id,
            item_type: item_type.to_string(),
            item_name: item_name.map(str::to_string),
            price,
        }
    }

    /// Low-level call to the autocomplete endpoint.
    #[tracing::instrument(skip(self), fields(search_text = %search_text, filter = %filter))]
    async fn autocomplete(&self, search_text: &str, filter: &str) -> anyhow::Result<Vec<RawResult>> {
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

/// A single result. Fields are optional because they vary by result `type`.
#[derive(Debug, Deserialize)]
struct RawResult {
    #[serde(rename = "type")]
    result_type: Option<String>,
    id: Option<i64>,
    name: Option<String>,
    band_name: Option<String>,
    album_name: Option<String>,
    location: Option<String>,
    item_url_path: Option<String>,
    item_url_root: Option<String>,
}

// --- Public output types ---

#[derive(Debug, Serialize)]
pub struct Artist {
    pub name: String,
    pub artist_id: Option<i64>,
    pub location: Option<String>,
    pub url: Option<String>,
}

impl From<RawResult> for Artist {
    fn from(r: RawResult) -> Self {
        Artist {
            name: r.name.unwrap_or_default(),
            artist_id: r.id,
            location: r.location,
            // Band results carry their URL in item_url_root, not item_url_path.
            url: r.item_url_root.or(r.item_url_path),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Song {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_id: Option<i64>,
    pub url: Option<String>,
}

impl From<RawResult> for Song {
    fn from(r: RawResult) -> Self {
        Song {
            title: r.name.unwrap_or_default(),
            artist: r.band_name,
            album: r.album_name,
            track_id: r.id,
            url: r.item_url_path,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CartResult {
    pub status: &'static str,
    pub message: String,
    pub item_id: u64,
    pub item_type: String,
    pub item_name: Option<String>,
    pub price: Option<f64>,
}
