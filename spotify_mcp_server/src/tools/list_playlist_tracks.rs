//! Action: list the tracks of one playlist, with their metadata.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::server::SpotifyServer;
use crate::tools::{api_error_result, json_result};

/// Low-cardinality template for the span and metric attributes; the request
/// itself goes to the interpolated path.
const ROUTE: &str = "/v1/playlists/{playlist_id}/tracks";
const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListPlaylistTracksParams {
    /// The playlist to read. Accepts a bare Spotify ID, a `spotify:playlist:…`
    /// URI, or an open.spotify.com playlist URL.
    pub playlist_id: String,
    /// Maximum number of tracks to return (default 100). Pages are followed
    /// automatically to reach this many.
    pub limit: Option<usize>,
    /// Index of the first track to return (default 0), for paging through a
    /// long playlist across several calls.
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize)]
struct PlaylistTrack {
    /// `"track"` or `"episode"` — playlists can hold podcast episodes too.
    kind: Option<String>,
    id: Option<String>,
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    album_release_date: Option<String>,
    duration_ms: Option<u64>,
    explicit: Option<bool>,
    /// Spotify's 0–100 popularity score.
    popularity: Option<u32>,
    disc_number: Option<u32>,
    track_number: Option<u32>,
    isrc: Option<String>,
    url: Option<String>,
    uri: Option<String>,
    /// When the track was added to the playlist (ISO 8601).
    added_at: Option<String>,
    added_by: Option<String>,
    /// True for a file local to the owner's library rather than Spotify's catalog.
    is_local: bool,
}

impl PlaylistTrack {
    fn from_item(item: RawItem) -> Option<Self> {
        // `track` is null for items Spotify can no longer resolve.
        let track = item.track?;
        Some(PlaylistTrack {
            kind: track.kind,
            id: track.id,
            title: track.name,
            artists: track.artists.into_iter().map(|a| a.name).collect(),
            album: track.album.as_ref().map(|a| a.name.clone()),
            album_release_date: track.album.and_then(|a| a.release_date),
            duration_ms: track.duration_ms,
            explicit: track.explicit,
            popularity: track.popularity,
            disc_number: track.disc_number,
            track_number: track.track_number,
            isrc: track.external_ids.isrc,
            url: track.external_urls.spotify,
            uri: track.uri,
            added_at: item.added_at,
            added_by: item.added_by.and_then(|u| u.id),
            is_local: item.is_local,
        })
    }
}

#[tool_router(router = list_playlist_tracks_router, vis = "pub")]
impl SpotifyServer {
    #[tool(
        description = "List the tracks of a Spotify playlist with their metadata (artists, \
                          album, duration, ISRC, popularity, when and by whom they were added). \
                          Takes a playlist ID, URI or URL — get one from `list_playlists`."
    )]
    #[tracing::instrument(
        name = "tools/call list_playlist_tracks",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "list_playlist_tracks",
            spotify.playlist.id = tracing::field::Empty,
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn list_playlist_tracks(
        &self,
        Parameters(params): Parameters<ListPlaylistTracksParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.list_playlist_tracks_inner(params).await;
        crate::metrics::record_tool_call("list_playlist_tracks", crate::tools::outcome(&result));
        result
    }

    async fn list_playlist_tracks_inner(
        &self,
        params: ListPlaylistTracksParams,
    ) -> Result<CallToolResult, McpError> {
        let id = playlist_id(&params.playlist_id).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "`{}` is not a Spotify playlist ID, URI or URL",
                    params.playlist_id
                ),
                None,
            )
        })?;
        tracing::Span::current().record("spotify.playlist.id", &id);

        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let offset = params.offset.unwrap_or(0);
        let query = vec![("offset", offset.to_string())];

        let (raw, total) = match self
            .client()
            .get_paged::<RawItem>(ROUTE, &format!("/v1/playlists/{id}/tracks"), &query, limit)
            .await
        {
            Ok(page) => page,
            Err(e) => return api_error_result(e, "listing playlist tracks failed"),
        };

        let tracks: Vec<PlaylistTrack> = raw
            .into_iter()
            .filter_map(PlaylistTrack::from_item)
            .collect();
        tracing::Span::current().record("result.count", tracks.len());
        tracing::info!(
            count = tracks.len(),
            total,
            "playlist track listing complete"
        );

        json_result(&json!({
            "playlist_id": id,
            "offset": offset,
            "returned": tracks.len(),
            "total": total,
            "tracks": tracks,
        }))
    }
}

/// Extract the base-62 playlist ID from a bare ID, a `spotify:playlist:…` URI,
/// or an `open.spotify.com/playlist/…` URL.
fn playlist_id(input: &str) -> Option<String> {
    let input = input.trim();
    let candidate = if let Some(rest) = input.rsplit_once("playlist:").map(|(_, r)| r) {
        rest
    } else if let Some(rest) = input.rsplit_once("/playlist/").map(|(_, r)| r) {
        // Strip any `?si=…` tracking suffix a shared URL carries.
        rest.split(['?', '/']).next().unwrap_or(rest)
    } else {
        input
    };
    (!candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| candidate.to_string())
}

// --- Raw wire types (Spotify playlist track object) ---

#[derive(Debug, Deserialize)]
struct RawItem {
    added_at: Option<String>,
    added_by: Option<RawAddedBy>,
    #[serde(default)]
    is_local: bool,
    /// Null when the item no longer resolves to a playable track.
    track: Option<RawTrack>,
}

#[derive(Debug, Deserialize)]
struct RawAddedBy {
    id: Option<String>,
}

/// Covers both track and episode items; everything episodes lack is optional.
#[derive(Debug, Deserialize)]
struct RawTrack {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artists: Vec<RawArtist>,
    album: Option<RawAlbum>,
    duration_ms: Option<u64>,
    explicit: Option<bool>,
    popularity: Option<u32>,
    disc_number: Option<u32>,
    track_number: Option<u32>,
    uri: Option<String>,
    #[serde(default)]
    external_ids: RawExternalIds,
    #[serde(default)]
    external_urls: RawExternalUrls,
}

#[derive(Debug, Deserialize)]
struct RawArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawAlbum {
    name: String,
    release_date: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawExternalIds {
    isrc: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawExternalUrls {
    spotify: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::playlist_id;

    const ID: &str = "37i9dQZF1DXcBWIGoYBM5M";

    #[test]
    fn accepts_every_form_of_playlist_reference() {
        assert_eq!(playlist_id(ID).as_deref(), Some(ID));
        assert_eq!(playlist_id(&format!("  {ID} ")).as_deref(), Some(ID));
        assert_eq!(
            playlist_id(&format!("spotify:playlist:{ID}")).as_deref(),
            Some(ID)
        );
        assert_eq!(
            playlist_id(&format!("https://open.spotify.com/playlist/{ID}?si=abc123")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn rejects_things_that_are_not_playlist_ids() {
        assert_eq!(playlist_id(""), None);
        assert_eq!(playlist_id("not an id"), None);
        // An album URL is a valid Spotify URL, but not a playlist.
        assert_eq!(playlist_id("https://open.spotify.com/album/abc"), None);
    }
}
