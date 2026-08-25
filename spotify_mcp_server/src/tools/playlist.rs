//! Reading a Spotify playlist: the shared fetch, the track shape it produces,
//! and how a playlist reference is parsed.
//!
//! Two actions need all of this — `list_playlist_tracks` returns the tracks,
//! `save_playlist_songs` records them — so it lives here rather than in either
//! of them. Only the `#[tool]` handlers are per-action.

use serde::{Deserialize, Serialize};

use crate::spotify::{ApiError, SpotifyClient};

/// Low-cardinality template for the span and metric attributes; the request
/// itself goes to the interpolated path.
pub(crate) const ROUTE: &str = "/v1/playlists/{playlist_id}/tracks";

/// Fetch up to `limit` tracks of a playlist, starting at `offset`.
///
/// Returns the tracks alongside the playlist's reported total.
pub(crate) async fn fetch_tracks(
    client: &SpotifyClient,
    id: &str,
    limit: usize,
    offset: usize,
) -> Result<(Vec<PlaylistTrack>, Option<u32>), ApiError> {
    let query = vec![("offset", offset.to_string())];
    let (raw, total) = client
        .get_paged::<RawItem>(ROUTE, &format!("/v1/playlists/{id}/tracks"), &query, limit)
        .await?;
    let tracks = raw
        .into_iter()
        .filter_map(PlaylistTrack::from_item)
        .collect();
    Ok((tracks, total))
}

#[derive(Debug, Serialize)]
pub(crate) struct PlaylistTrack {
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

    /// The row to record for this track, or `None` if it is not a song: a
    /// playlist can hold podcast episodes, and a track can be missing the
    /// artist or title that together identify it.
    pub(crate) fn to_new_song(&self) -> Option<mcp_db::NewSong> {
        if self.kind.as_deref() == Some("episode") {
            return None;
        }
        let artist = self.artists.first()?;
        if artist.trim().is_empty() || self.title.trim().is_empty() {
            return None;
        }
        Some(mcp_db::NewSong {
            artist: artist.clone(),
            title: self.title.clone(),
            artists: self.artists.clone(),
            album: self.album.clone(),
            album_release_date: self.album_release_date.clone(),
            duration_ms: self.duration_ms.and_then(|ms| i64::try_from(ms).ok()),
            spotify_id: self.id.clone(),
            isrc: self.isrc.clone(),
        })
    }
}

/// Extract the base-62 playlist ID from a bare ID, a `spotify:playlist:…` URI,
/// or an `open.spotify.com/playlist/…` URL.
pub(crate) fn playlist_id(input: &str) -> Option<String> {
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
